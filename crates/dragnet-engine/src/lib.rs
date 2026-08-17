// SPDX-License-Identifier: AGPL-3.0-only
//! dragnet-engine — Dragnet boru hattını tek çağrıyla başlatan çekirdek kütüphane.
//!
//! Harvester (dragnet-dht) → sighting → sınırlı fetcher havuzu (dragnet-meta) →
//! store (dragnet-store) → arama API (dragnet-api) zincirini tek süreçte kurar.
//! Hem `dragnetd` daemon'ı hem de Tauri masaüstü kabuğu bu çekirdeği kullanır
//! (sello-core/sello-app deseni) — böylece "tek exe" mümkün olur, ayrı bir
//! daemon süreci gerekmez.

pub mod semantic_indexer;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use dragnet_core::InfoHash;
use dragnet_dht::{HarvesterConfig, StatsSnapshot};
use dragnet_meta::{FetchConfig, MetadataFetcher};
use dragnet_store::Store;

pub use dragnet_store::TorrentSummary;

/// Çekirdek yapılandırması (figment/dosya bağımlılığı yok; çağıran doldurur).
/// Çekirdek yapılandırması. API sunucusu ARTIK çekirdekte DEĞİL — tarama durunca
/// (Engine drop) arama API'sinin de düşmesini önlemek için çağıran (daemon/app)
/// API'yi ayrı ve uzun-ömürlü çalıştırır (`Engine::store()`'a karşı).
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub db_path: String,
    pub harvester_port: u16,
    pub harvester_max_queries_per_sec: f64,
    pub fetch_workers: usize,
    pub fetch_peer_concurrency: usize,
    pub seed_infohashes: Vec<String>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            db_path: "dragnet.db".to_string(),
            harvester_port: 6881,
            harvester_max_queries_per_sec: 50.0,
            fetch_workers: 12,
            fetch_peer_concurrency: 12,
            seed_infohashes: Vec::new(),
        }
    }
}

/// Çekirdek başlatma hataları.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("depolama hatası: {0}")]
    Store(#[from] dragnet_store::StoreError),
    #[error("IO hatası: {0}")]
    Io(#[from] std::io::Error),
}

/// Anlık çekirdek durumu (dashboard/log için).
#[derive(Debug, Clone)]
pub struct EngineSnapshot {
    pub harvester: StatsSnapshot,
    /// Metadata çekim sayaçları (Faz E): deneme/başarı/peer yok/peer başarısız/ort. süre.
    pub fetch: dragnet_meta::FetchStatsSnapshot,
    /// Çekim kuyruğu: (pending, sıcak-pending, unreachable, son 1 saatte fetched).
    pub queue: (i64, i64, i64, i64),
    /// Fetcher DHT istemcisi: (firewalled, dış adres, port) — erişilebilirlik göstergesi.
    pub dht_client: (bool, Option<String>, u16),
    pub fetched_torrents: i64,
    pub total_infohashes: i64,
    pub harvester_addr: SocketAddr,
}

/// Çalışan Dragnet çekirdeği. Bırakılınca tüm arka plan görevleri durur.
pub struct Engine {
    store: Store,
    harvester_stats: Arc<dragnet_dht::Stats>,
    fetch_stats: Arc<dragnet_meta::FetchStats>,
    harvester_addr: SocketAddr,
    fetcher: Arc<MetadataFetcher>,
    tasks: Vec<JoinHandle<()>>,
}

impl Engine {
    /// Boru hattını başlatır (store aç, harvester + fetcher havuzu + API spawn et).
    pub async fn start(config: EngineConfig) -> Result<Engine, EngineError> {
        let store = Store::open(&config.db_path).await?;

        // ÖNCE harvester (yönlendirilmiş sabit portu — varsayılan 6881 — o almalı: pasif hasat
        // gelen trafiğe bağlıdır); mainline istemcisi 6881 doluysa kendiliğinden efemer porta düşer.
        let mut harvester = dragnet_dht::spawn(HarvesterConfig {
            port: config.harvester_port,
            max_queries_per_sec: config.harvester_max_queries_per_sec,
            ..Default::default()
        })
        .await?;

        let fetcher = Arc::new(MetadataFetcher::new(FetchConfig {
            concurrency: config.fetch_peer_concurrency,
            ..Default::default()
        })?);
        let harvester_stats = harvester.stats();
        let fetch_stats = fetcher.stats();
        let harvester_addr = harvester.local_addr();
        info!(addr = %harvester_addr, "harvester çalışıyor");

        let mut tasks = Vec::new();

        // NOT: Arama API'si burada DEĞİL — çağıran (daemon/app) ayrı ve uzun-ömürlü
        // çalıştırır, böylece tarama durunca arama erişimi kesilmez.

        // Şema-öncesi indekslenmiş kayıtların kategorilerini bir kez düzelt (arka planda).
        {
            let store = store.clone();
            tasks.push(tokio::spawn(async move {
                match store.recategorize(50_000).await {
                    Ok(n) if n > 0 => {
                        info!(updated = n, "mevcut kayıtlar yeniden kategorilendirildi")
                    }
                    _ => {}
                }
            }));
        }

        let sem = Arc::new(Semaphore::new(config.fetch_workers.max(1)));

        // Başlangıç seed infohash'leri.
        for hex in &config.seed_infohashes {
            match InfoHash::from_hex(hex) {
                Some(ih) => {
                    let store = store.clone();
                    let fetcher = Arc::clone(&fetcher);
                    tasks.push(tokio::spawn(async move {
                        fetch_and_store(ih, &[], &store, &fetcher).await;
                    }));
                }
                None => warn!(hex, "geçersiz seed infohash (40-hex olmalı), atlanıyor"),
            }
        }

        // Canlılık kontrolü (nazik): indekslenen torrent'leri periyodik DHT scrape
        // ile kontrol edip canlı peer sayısını güncelle. Böylece "hangisi indirilebilir"
        // bilinir (ölüler 0 peer). Küçük batch + kısa timeout → düşük ek yük.
        {
            let store = store.clone();
            let fetcher = Arc::clone(&fetcher);
            tasks.push(tokio::spawn(async move {
                let mut ticker = tokio::time::interval(Duration::from_secs(6));
                ticker.tick().await;
                loop {
                    ticker.tick().await;
                    let batch = match store.torrents_to_check(2).await {
                        Ok(b) if !b.is_empty() => b,
                        _ => continue,
                    };
                    let mut handles = Vec::new();
                    for ih in batch {
                        let store = store.clone();
                        let fetcher = Arc::clone(&fetcher);
                        handles.push(tokio::spawn(async move {
                            let n = fetcher.count_peers(ih, Duration::from_secs(8)).await;
                            let _ = store.update_liveness(ih, n as i64, now_unix()).await;
                        }));
                    }
                    for h in handles {
                        let _ = h.await;
                    }
                }
            }));
        }

        // Ana boru hattı: harvester akışını tüket → sighting yaz (kaynak bilgisiyle:
        // pasif get_peers/announce = sıcak). Çekim burada TETİKLENMEZ — Faz E: firehose
        // içinden "izin boşsa çek" yaklaşımı hem rastgeleydi hem de işçiler doluyken gelen
        // hash'leri hiç denemiyordu; onun yerine aşağıdaki öncelikli zamanlayıcı çalışır.
        // Peer ipuçları (BEP-51 takip get_peers'ten): infohash → taze adresler; bellek-içi,
        // sınırlı; çekim zamanlayıcısı önce bunları dener.
        let hints: Arc<std::sync::Mutex<PeerHints>> =
            Arc::new(std::sync::Mutex::new(PeerHints::new(50_000)));
        {
            let store = store.clone();
            let hints = Arc::clone(&hints);
            tasks.push(tokio::spawn(async move {
                while let Some(s) = harvester.infohashes.recv().await {
                    if !s.peers.is_empty() {
                        hints
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .insert(s.infohash, s.peers.clone());
                    }
                    if let Err(e) = store
                        .record_sighting_full(
                            s.infohash,
                            now_unix(),
                            s.source.is_hot(),
                            s.peers.len() as i64,
                            s.repeats.max(1) as i64,
                        )
                        .await
                    {
                        debug!(error = %e, "record_sighting hatası");
                    }
                }
            }));
        }

        // Çekim zamanlayıcısı: boş işçi izni oldukça depodan öncelikli adayları çeker
        // (sıcak > popüler > taze; soğuma ile yeniden deneme). Adaylar seçilirken
        // `last_attempt` işaretlenir; başarı → upsert (fetched), başarısızlık →
        // deneme sınırında `unreachable`.
        {
            let store = store.clone();
            let fetcher = Arc::clone(&fetcher);
            let sem = Arc::clone(&sem);
            let hints = Arc::clone(&hints);
            let workers = config.fetch_workers.max(1);
            tasks.push(tokio::spawn(async move {
                // Yeni açılan DHT istemcisi ısınmadan get_peers zayıf döner.
                tokio::time::sleep(Duration::from_secs(5)).await;
                loop {
                    let free = sem.available_permits();
                    if free == 0 {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        continue;
                    }
                    let batch = match store
                        .next_to_fetch(free.min(workers) as i64, now_unix())
                        .await
                    {
                        Ok(b) => b,
                        Err(e) => {
                            warn!(error = %e, "çekim kuyruğu okunamadı");
                            tokio::time::sleep(Duration::from_secs(5)).await;
                            continue;
                        }
                    };
                    if batch.is_empty() {
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        continue;
                    }
                    for infohash in batch {
                        let Ok(permit) = Arc::clone(&sem).acquire_owned().await else {
                            return;
                        };
                        let store = store.clone();
                        let fetcher = Arc::clone(&fetcher);
                        let peer_hints = hints
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .take(&infohash);
                        tokio::spawn(async move {
                            let _permit = permit;
                            fetch_and_store(infohash, &peer_hints, &store, &fetcher).await;
                        });
                    }
                }
            }));
        }

        Ok(Engine {
            store,
            harvester_stats,
            fetch_stats,
            harvester_addr,
            fetcher,
            tasks,
        })
    }

    /// İndeks deposunun bir kopyası (arama/dashboard sorguları için).
    pub fn store(&self) -> Store {
        self.store.clone()
    }

    /// Harvester'ın dinlediği yerel adres.
    pub fn harvester_addr(&self) -> SocketAddr {
        self.harvester_addr
    }

    /// Anlık durum (harvester sayaçları + indeks büyüklüğü).
    pub async fn snapshot(&self) -> EngineSnapshot {
        EngineSnapshot {
            harvester: self.harvester_stats.snapshot(),
            fetch: self.fetch_stats.snapshot(),
            queue: self
                .store
                .fetch_queue_stats(now_unix())
                .await
                .unwrap_or((0, 0, 0, 0)),
            dht_client: self.fetcher.dht_reachability().await,
            fetched_torrents: self.store.count_fetched().await.unwrap_or(0),
            total_infohashes: self.store.count_total().await.unwrap_or(0),
            harvester_addr: self.harvester_addr,
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// Peer ipucu önbelleği: infohash → taze adresler (en fazla `cap` kayıt, FIFO tahliye).
struct PeerHints {
    map: std::collections::HashMap<InfoHash, Vec<std::net::SocketAddrV4>>,
    order: std::collections::VecDeque<InfoHash>,
    cap: usize,
}

impl PeerHints {
    fn new(cap: usize) -> Self {
        Self {
            map: std::collections::HashMap::with_capacity(cap.min(4096)),
            order: std::collections::VecDeque::with_capacity(cap.min(4096)),
            cap,
        }
    }
    fn insert(&mut self, ih: InfoHash, mut peers: Vec<std::net::SocketAddrV4>) {
        peers.truncate(16);
        if self.map.insert(ih, peers).is_none() {
            self.order.push_back(ih);
            while self.order.len() > self.cap {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                }
            }
        }
    }
    fn take(&mut self, ih: &InfoHash) -> Vec<std::net::SocketAddrV4> {
        self.map.remove(ih).unwrap_or_default()
    }
}

/// Bir infohash için metadata çeker (ipucu peer'ler önce); başarılıysa yazar, başarısızsa
/// deneme sınırında `unreachable` işaretler.
async fn fetch_and_store(
    infohash: InfoHash,
    hints: &[std::net::SocketAddrV4],
    store: &Store,
    fetcher: &MetadataFetcher,
) {
    match fetcher.fetch_with_hints(infohash, hints).await {
        Ok(record) => {
            let files = record.files.len();
            let name = record.name.clone();
            match store.upsert_torrent(&record).await {
                Ok(()) => info!(infohash = %infohash, name = %name, files, "metadata indekslendi"),
                Err(e) => error!(error = %e, "torrent yazılamadı"),
            }
        }
        Err(e) => {
            debug!(infohash = %infohash, error = %e, "metadata çekilemedi");
            let _ = store.mark_fetch_failed(infohash).await;
        }
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
