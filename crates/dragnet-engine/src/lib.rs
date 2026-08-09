// SPDX-License-Identifier: AGPL-3.0-only
//! dragnet-engine — Dragnet boru hattını tek çağrıyla başlatan çekirdek kütüphane.
//!
//! Harvester (dragnet-dht) → sighting → sınırlı fetcher havuzu (dragnet-meta) →
//! store (dragnet-store) → arama API (dragnet-api) zincirini tek süreçte kurar.
//! Hem `dragnetd` daemon'ı hem de Tauri masaüstü kabuğu bu çekirdeği kullanır
//! (sello-core/sello-app deseni) — böylece "tek exe" mümkün olur, ayrı bir
//! daemon süreci gerekmez.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use dragnet_api::ApiConfig;
use dragnet_core::InfoHash;
use dragnet_dht::{HarvesterConfig, StatsSnapshot};
use dragnet_meta::{FetchConfig, MetadataFetcher};
use dragnet_store::Store;

pub use dragnet_store::{Store as IndexStore, TorrentSummary};

/// Çekirdek yapılandırması (figment/dosya bağımlılığı yok; çağıran doldurur).
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub db_path: String,
    pub api_bind: SocketAddr,
    pub api_token: Option<String>,
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
            api_bind: SocketAddr::from(([127, 0, 0, 1], 8080)),
            api_token: None,
            harvester_port: 0,
            harvester_max_queries_per_sec: 50.0,
            fetch_workers: 2,
            fetch_peer_concurrency: 6,
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
    pub fetched_torrents: i64,
    pub total_infohashes: i64,
    pub harvester_addr: SocketAddr,
}

/// Çalışan Dragnet çekirdeği. Bırakılınca tüm arka plan görevleri durur.
pub struct Engine {
    store: Store,
    harvester_stats: Arc<dragnet_dht::Stats>,
    harvester_addr: SocketAddr,
    tasks: Vec<JoinHandle<()>>,
}

impl Engine {
    /// Boru hattını başlatır (store aç, harvester + fetcher havuzu + API spawn et).
    pub async fn start(config: EngineConfig) -> Result<Engine, EngineError> {
        let store = Store::open(&config.db_path).await?;

        let fetcher = Arc::new(MetadataFetcher::new(FetchConfig {
            concurrency: config.fetch_peer_concurrency,
            peer_gather_timeout: Duration::from_secs(12),
            ..Default::default()
        })?);

        let mut harvester = dragnet_dht::spawn(HarvesterConfig {
            port: config.harvester_port,
            max_queries_per_sec: config.harvester_max_queries_per_sec,
            ..Default::default()
        })
        .await?;
        let harvester_stats = harvester.stats();
        let harvester_addr = harvester.local_addr();
        info!(addr = %harvester_addr, "harvester çalışıyor");

        let mut tasks = Vec::new();

        // Arama API'si.
        {
            let api_cfg = ApiConfig {
                bind: config.api_bind,
                token: config.api_token.clone(),
                ..Default::default()
            };
            let api_store = store.clone();
            tasks.push(tokio::spawn(async move {
                if let Err(e) = dragnet_api::serve(api_cfg, api_store).await {
                    error!(error = %e, "API sunucusu durdu");
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
                        fetch_and_store(ih, &store, &fetcher).await;
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

        // Ana boru hattı: harvester akışını tüket, sighting yaz, gerekirse çek.
        {
            let store = store.clone();
            let fetcher = Arc::clone(&fetcher);
            let sem = Arc::clone(&sem);
            tasks.push(tokio::spawn(async move {
                while let Some(infohash) = harvester.infohashes.recv().await {
                    if let Err(e) = store.record_sighting(infohash, now_unix()).await {
                        debug!(error = %e, "record_sighting hatası");
                    }
                    match store.needs_metadata(infohash).await {
                        Ok(true) => {}
                        Ok(false) => continue,
                        Err(e) => {
                            debug!(error = %e, "needs_metadata hatası");
                            continue;
                        }
                    }
                    if let Ok(permit) = Arc::clone(&sem).try_acquire_owned() {
                        let store = store.clone();
                        let fetcher = Arc::clone(&fetcher);
                        tokio::spawn(async move {
                            let _permit = permit;
                            fetch_and_store(infohash, &store, &fetcher).await;
                        });
                    }
                }
            }));
        }

        Ok(Engine {
            store,
            harvester_stats,
            harvester_addr,
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

/// Bir infohash için metadata çeker; başarılıysa yazar, başarısızsa `unreachable` işaretler.
async fn fetch_and_store(infohash: InfoHash, store: &Store, fetcher: &MetadataFetcher) {
    match fetcher.fetch(infohash).await {
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
            let _ = store.mark_unreachable(infohash).await;
        }
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
