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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use dragnet_core::InfoHash;
use dragnet_dht::{HarvesterConfig, StatsSnapshot};
use dragnet_meta::{FetchConfig, MetadataFetcher};
use dragnet_store::{metric, Store};

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
    /// Aynı anda kaç aday triyaj edilsin (her biri bir DHT araması). Triyaj, çekimin
    /// aday arzını üreten aşamadır; ölçümde asıl darboğaz burasıydı (saatte ~900-1.400
    /// ölçüm, buna karşılık ~2.800-3.600 yeni infohash). Her ölçüm çoğunlukla ağ
    /// beklemesidir, CPU'ya neredeyse dokunmaz.
    pub triage_concurrency: usize,
    pub seed_infohashes: Vec<String>,
    /// Depolama büyüme freni (F8-4), bayt: veritabanı bütçesi ve disk rezervi (0 = kapalı).
    /// Motor kendi `Store` örneğini açtığı için sınırlar buradan geçirilmelidir —
    /// yoksa uygulama tarafındaki sınır yazan yolu (sighting/metadata) etkilemez.
    pub db_max_bytes: u64,
    pub disk_reserve_bytes: u64,
    /// Çoklu düğüm kimliği (F9): aynı anda kaç DHT kimliğiyle dinlensin (1-8).
    /// BEP-42 bir IP için 8 geçerli kimliğe izin verir; her kimlik ayrı UDP portunda
    /// (`harvester_port` + i) dinler ve pasif hasat kimlik sayısıyla ölçeklenir.
    pub harvester_instances: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            db_path: "dragnet.db".to_string(),
            harvester_port: 6881,
            harvester_max_queries_per_sec: 50.0,
            fetch_workers: 12,
            fetch_peer_concurrency: 12,
            triage_concurrency: 24,
            seed_infohashes: Vec::new(),
            db_max_bytes: 0,
            disk_reserve_bytes: 0,
            harvester_instances: 1,
        }
    }
}

/// Triyaj peer sayımı için süre bütçesi: "peer var mı" sorusu, tam arama değil.
const TRIAGE_TIMEOUT: Duration = Duration::from_secs(6);
/// Triyajda bir aday için toplanacak azami peer adresi. Bu sayıya ulaşılınca DHT araması
/// erkenden biter (ölçüm: canlı adaylarda ilk peer'ler ~0,3-1,0 sn içinde geliyor), ve
/// toplanan adresler çekim aşamasına ipucu olarak devredilir. `PeerHints` zaten 16'da
/// kırptığı için daha fazlasını toplamak boşuna DHT trafiği olurdu.
const TRIAGE_PEER_CAP: usize = 16;
/// **Triyaj bekleyen** kuyruğun üst sınırı (F11, F13'te anlamı düzeltildi): bunun
/// üstünde soğuk BEP-51 örnekleri alınmaz; sıcak sinyaller ve peer'li görülmeler her
/// zaman kabul edilir.
///
/// Ölçülen triyaj kapasitesi saatte ~8.000-16.000, dolayısıyla 20.000 yaklaşık 1,5-2
/// saatlik iştir — sıra gelene kadar ölen torrent biriktirmemek için bu kadarı yeterli.
/// Sayım İŞLENMEMİŞ yükü almalıdır (`count_triage_backlog`): toplam bekleyeni saymak,
/// triyajdan geçmiş ve yalnız soğumada bekleyen kayıtları da "iş" sanıp girişi
/// gereksiz yere kesiyordu.
const MAX_PENDING_BACKLOG: i64 = 20_000;
/// Ölü bekleyen kayıtlar bu süreden eskiyse silinir (kullanıcı isteği: 3 gün).
const DEAD_PURGE_AFTER_SECS: i64 = 3 * 24 * 3600;

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
    harvester_stats: Vec<Arc<dragnet_dht::Stats>>,
    fetch_stats: Arc<dragnet_meta::FetchStats>,
    harvester_addr: SocketAddr,
    fetcher: Arc<MetadataFetcher>,
    tasks: Vec<JoinHandle<()>>,
}

impl Engine {
    /// Boru hattını başlatır (store aç, harvester + fetcher havuzu + API spawn et).
    pub async fn start(config: EngineConfig) -> Result<Engine, EngineError> {
        let store = Store::open(&config.db_path).await?;
        // F8-4: sınırları uygula ve ilk ölçümü hemen yap (yazan yollar bunu kontrol eder).
        store.set_limits(config.db_max_bytes, config.disk_reserve_bytes);
        store.refresh_pressure();

        // ÖNCE harvester (yönlendirilmiş sabit portu — varsayılan 6881 — o almalı: pasif hasat
        // gelen trafiğe bağlıdır); mainline istemcisi 6881 doluysa kendiliğinden efemer porta düşer.
        // ÇOKLU DÜĞÜM KİMLİĞİ (F9): BEP-42 bir IP için 8 geçerli kimliğe izin verir
        // (kimliğin ilk 21 biti IP + 3 bitlik rastgele bileşenden türetilir). Her kimlik
        // ayrı bir UDP portunda dinler; ağın farklı bölgelerinden trafik alırız ve pasif
        // hasat kimlik sayısıyla ölçeklenir. Portlar: `harvester_port`, +1, +2, …
        // (modemde hepsinin yönlendirilmesi gerekir; yönlendirilmeyenler yine aktif
        // hasat—BEP-51—yapar, yalnız gelen sorgu almazlar).
        let instances = config.harvester_instances.clamp(1, 8);
        let mut harvesters = Vec::with_capacity(instances);
        for i in 0..instances {
            let port = if config.harvester_port == 0 {
                0
            } else {
                config.harvester_port.saturating_add(i as u16)
            };
            // Kimlik/düğüm önbelleği veritabanının yanında durur (her kimlik ayrı dosya).
            // Kimliğin oturumlar arası KORUNMASI pasif hasadın birikmesi için şart.
            let state_path = (!config.db_path.is_empty())
                .then(|| std::path::PathBuf::from(format!("{}.dht{i}", config.db_path)));
            match dragnet_dht::spawn(HarvesterConfig {
                port,
                state_path,
                // Giden sorgu bütçesi kimlikler ARASINDA BÖLÜŞÜLÜR: ölçümde her kimliğe
                // tam bütçe verince (4 × 50/sn) toplam DHT trafiği metadata çekiminin
                // peer aramalarıyla yarıştı ve isim üretimi saatte 325 → 171'e düştü.
                // Çoklu kimliğin amacı giden trafiği değil, **gelen** trafiği artırmaktır.
                max_queries_per_sec: config.harvester_max_queries_per_sec / instances.max(1) as f64,
                ..Default::default()
            })
            .await
            {
                Ok(h) => harvesters.push(h),
                // Port doluysa (ör. qBittorrent) o kimliği atla; tek bir port hatası
                // taramayı düşürmemeli.
                Err(e) => warn!(port, error = %e, "ek harvester başlatılamadı, atlanıyor"),
            }
        }
        if harvesters.is_empty() {
            harvesters.push(
                dragnet_dht::spawn(HarvesterConfig {
                    port: config.harvester_port,
                    max_queries_per_sec: config.harvester_max_queries_per_sec,
                    state_path: (!config.db_path.is_empty())
                        .then(|| std::path::PathBuf::from(format!("{}.dht0", config.db_path))),
                    ..Default::default()
                })
                .await?,
            );
        }
        info!(
            instances = harvesters.len(),
            "harvester kimlikleri başlatıldı"
        );
        let mut harvester = harvesters.remove(0);

        let mut fetcher_inner = MetadataFetcher::new(FetchConfig {
            concurrency: config.fetch_peer_concurrency,
            ..Default::default()
        })?;
        // F12: uTP yedek yolu. TCP zaman aşımına uğrayan peer'ler uTP ile yeniden
        // denenir; kazanç `peer_utp_ok` sayacıyla üretimde ölçülür.
        let utp_ready = fetcher_inner.enable_utp().await;
        info!(utp = utp_ready, "metadata fetcher hazır");
        let fetcher = Arc::new(fetcher_inner);
        // Tüm kimliklerin sayaçları toplanarak raporlanır; ek kimliklerin infohash
        // akışları birincil kanala aktarılır (tüketici tarafı değişmez).
        let mut harvester_stats = vec![harvester.stats()];
        let extra_sink = harvester.sink();
        for mut h in harvesters {
            harvester_stats.push(h.stats());
            let sink = extra_sink.clone();
            tokio::spawn(async move {
                while let Some(s) = h.infohashes.recv().await {
                    if sink.send(s).await.is_err() {
                        break;
                    }
                }
            });
        }
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

        // Peer ipuçları (infohash → taze adresler; bellek-içi, sınırlı). İki kaynaktan
        // dolar: BEP-51 takip `get_peers` yanıtları (harvester) ve TRİYAJ (aşağıda).
        // Çekim zamanlayıcısı bir adayı alırken ipuçlarını da alır ve DHT aramasını
        // beklemeden doğrudan dener.
        let hints: Arc<std::sync::Mutex<PeerHints>> =
            Arc::new(std::sync::Mutex::new(PeerHints::new(50_000)));

        // TRİYAJ (F10): metadata çekmeden ÖNCE ucuz bir DHT peer sayımı. Yalnız peer'i
        // olan (sağlıklı) torrent'ler pahalı çekime girsin diye. Kullanıcı teşhisi:
        // "hiç paylaşanı olmayan eski bir torrent'i istediğin kadar çağır, indiremezsin".
        // Ölçüm de aynı yeri gösteriyordu (peer denemelerinin %97'si zaman aşımı).
        //
        // F13 — İKİ ÖLÇÜLMÜŞ DEĞİŞİKLİK:
        //
        // 1) BULUNAN PEER ADRESLERİ ARTIK SAKLANIYOR. Eskiden triyaj `count_peers` ile
        //    yalnız SAYIYI alıp adresleri çöpe atıyordu; sonra çekim aşaması aynı
        //    infohash için DHT aramasını SIFIRDAN tekrarlıyordu. Ölçüm (peerstat):
        //    triyajdan geçmiş adaylarda bir arama medyan 15 / ortalama 64 peer buluyor
        //    ve medyan 2,7 sn sürüyor — yani her adayda hem ~2,7 sn hem de onlarca taze
        //    adres iki kez ödeniyordu. Artık adresler ipucu olarak çekime devrediliyor.
        //
        // 2) TUR BARİYERİ KALKTI. Eskiden 8'lik parti alınıp `for h in handles { await }`
        //    ile hepsinin bitmesi bekleniyordu; tur en yavaş ölçüme kilitleniyor ve
        //    ölçülen verim saatte yalnız ~900-1.400 oluyordu. Artık çekim zamanlayıcısıyla
        //    aynı desen: izin (semaphore) boşaldıkça sürekli akış.
        {
            let store = store.clone();
            let fetcher = Arc::clone(&fetcher);
            let hints = Arc::clone(&hints);
            let triage_sem = Arc::new(Semaphore::new(config.triage_concurrency.max(1)));
            let cap = config.triage_concurrency.max(1);
            tasks.push(tokio::spawn(async move {
                loop {
                    if store.growth_paused() {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        continue;
                    }
                    let free = triage_sem.available_permits();
                    if free == 0 {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                    let batch = match store.next_to_triage(free.min(cap) as i64, now_unix()).await {
                        Ok(b) if !b.is_empty() => b,
                        Ok(_) => {
                            // Triyaj kuyruğu boş: hasadın yeni aday getirmesini bekle.
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            continue;
                        }
                        Err(e) => {
                            warn!(error = %e, "triyaj kuyruğu okunamadı");
                            tokio::time::sleep(Duration::from_secs(5)).await;
                            continue;
                        }
                    };
                    for ih in batch {
                        let Ok(permit) = Arc::clone(&triage_sem).acquire_owned().await else {
                            return;
                        };
                        let store = store.clone();
                        let fetcher = Arc::clone(&fetcher);
                        let hints = Arc::clone(&hints);
                        tokio::spawn(async move {
                            let _permit = permit;
                            // Kısa bütçe: bu bir "var mı yok mu" ölçümü, tam arama değil.
                            // `TRIAGE_PEER_CAP` peer bulununca arama erkenden biter.
                            let peers = fetcher.peers_of(ih, TRIAGE_TIMEOUT, TRIAGE_PEER_CAP).await;
                            // Olay olarak sayılır: ölü kayıt SİLİNDİĞİ için tablodaki
                            // satırlara bakarak triyaj hızı ölçülemez (bkz. `metrics`).
                            let _ = store.bump_metric(metric::TRIAGE_DONE, now_unix()).await;
                            if peers.is_empty() {
                                // F11: peer'i olmayan torrent'in metadata'sı çekilemez;
                                // saklamak kuyruğu ve diski zehirliyor → hemen sil.
                                // Gerçekten canlanırsa DHT'de yeniden görülür.
                                let _ = store.bump_metric(metric::TRIAGE_DEAD, now_unix()).await;
                                let _ = store.delete_pending(ih).await;
                            } else {
                                hints
                                    .lock()
                                    .unwrap_or_else(|p| p.into_inner())
                                    .insert(ih, peers.clone());
                                let _ =
                                    store.record_probe(ih, peers.len() as i64, now_unix()).await;
                            }
                        });
                    }
                }
            }));
        }

        // ÖLÜ TEMİZLİĞİ (F10): triyajda peer bulunamamış ve günlerdir görülmemiş bekleyen
        // kayıtlar ile eski `unreachable` kayıtlar silinir — adlı kayıtlara dokunulmaz.
        // Ölü yığın hem kuyruğu hem diski zehirliyordu (2 milyon bekleyen kayıt).
        {
            let store = store.clone();
            tasks.push(tokio::spawn(async move {
                let mut ticker = tokio::time::interval(Duration::from_secs(3600));
                loop {
                    ticker.tick().await;
                    match store.purge_dead(now_unix(), DEAD_PURGE_AFTER_SECS).await {
                        Ok(n) if n > 0 => info!(deleted = n, "ölü bekleyen kayıtlar temizlendi"),
                        Err(e) => warn!(error = %e, "ölü kayıt temizliği başarısız"),
                        _ => {}
                    }
                }
            }));
        }

        // HARVESTER SAYAÇLARINI KALICI KIL: bunlar süreç-içi atomik sayaçlar, süreç
        // kapanınca kaybolur ve teşhis araçlarından görünmez. "Hasat neden düştü?"
        // sorusunu ancak bunlar cevaplar — aktif örnekleme mi durdu, pasif trafik mi
        // gelmiyor? Periyodik olarak FARKLARI `metrics` tablosuna yazılır.
        {
            let store = store.clone();
            let stats: Vec<Arc<dragnet_dht::Stats>> = harvester_stats.clone();
            tasks.push(tokio::spawn(async move {
                let mut ticker = tokio::time::interval(Duration::from_secs(60));
                ticker.tick().await;
                let snap = |s: &[Arc<dragnet_dht::Stats>]| {
                    s.iter()
                        .map(|x| x.snapshot())
                        .reduce(|a, b| a.merge(b))
                        .unwrap_or_else(|| dragnet_dht::Stats::default().snapshot())
                };
                let mut prev = snap(&stats);
                loop {
                    ticker.tick().await;
                    let cur = snap(&stats);
                    let now = now_unix();
                    // KALP ATIŞI: motorun yaşadığını gösterir. Bir teşhis oturumunda
                    // sayaçlar 11 dakika boyunca kıpırdamadı ve bunun "sistem yavaş" mı
                    // yoksa "motor durmuş" mu olduğu ayırt edilemedi — sayaçların DONMASI
                    // ile SIFIR OLMASI aynı görünüyordu. Bu metrik her turda artar;
                    // artmıyorsa sorun boru hattında değil, motorun kendisindedir.
                    let _ = store.bump_metric(metric::ENGINE_ALIVE, now).await;
                    for (name, d) in [
                        (metric::DHT_SAMPLES, cur.samples_seen - prev.samples_seen),
                        (metric::DHT_ANNOUNCE, cur.announce_seen - prev.announce_seen),
                        (
                            metric::DHT_GET_PEERS,
                            cur.get_peers_seen - prev.get_peers_seen,
                        ),
                        (
                            metric::DHT_QUERIES_SENT,
                            cur.queries_sent - prev.queries_sent,
                        ),
                        (
                            metric::DHT_RATE_LIMITED,
                            cur.rate_limited - prev.rate_limited,
                        ),
                        (
                            metric::DHT_RESPONSES,
                            cur.responses_seen - prev.responses_seen,
                        ),
                        (
                            metric::DHT_NODES_LEARNED,
                            cur.nodes_learned - prev.nodes_learned,
                        ),
                        (
                            metric::DHT_DROPPED,
                            cur.dropped_channel_full - prev.dropped_channel_full,
                        ),
                        (
                            metric::DHT_SOCK_ERR,
                            (cur.send_errors + cur.recv_errors)
                                - (prev.send_errors + prev.recv_errors),
                        ),
                        (metric::DHT_DUPLICATES, cur.duplicates - prev.duplicates),
                        (
                            metric::DHT_HARVESTED,
                            cur.unique_infohashes - prev.unique_infohashes,
                        ),
                    ] {
                        let _ = store.add_metric(name, d as i64, now).await;
                    }
                    prev = cur;
                }
            }));
        }

        // F8-4: depolama basıncını periyodik ölç (motorun kendi Store örneği için).
        {
            let store = store.clone();
            tasks.push(tokio::spawn(async move {
                let mut ticker = tokio::time::interval(Duration::from_secs(30));
                loop {
                    ticker.tick().await;
                    store.refresh_pressure();
                }
            }));
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
        {
            let store = store.clone();
            let hints = Arc::clone(&hints);
            tasks.push(tokio::spawn(async move {
                // GİRİŞ KISMA (F11): infohash toplamak metadata çekmekten kat kat hızlı.
                // Kuyruk kapasitenin üstüne çıkınca **soğuk** örnekler (BEP-51) alınmaz;
                // sıcak sinyaller (announce/get_peers) ve peer'li görülmeler her zaman
                // kabul edilir. Kullanıcı teşhisi: "metadata sorgusu yapabildiğimizin biraz
                // fazlası kadar infohash çekelim; boşuna infohash'te hızlanıp metadata'da
                // çakılmaya gerek yok."
                let mut pending = 0i64;
                let mut last_count = Instant::now() - Duration::from_secs(60);
                while let Some(s) = harvester.infohashes.recv().await {
                    if last_count.elapsed() >= Duration::from_secs(10) {
                        // İŞLENMEMİŞ yük ölçülür (triyaj bekleyenler), toplam bekleyen
                        // DEĞİL: toplam sayı, triyajdan geçmiş ve yalnız yeniden-deneme
                        // soğumasında bekleyen kayıtları da içeriyordu. Ölçümde bekleyen
                        // 23.166'nın 14.648'i böyleydi — yani bitmiş iş "kuyruk dolu"
                        // sayılıp taze infohash girişini kesiyordu, oysa triyaj kuyruğu
                        // boşalmış ve sistem aç bekliyordu.
                        pending = store.count_triage_backlog().await.unwrap_or(pending);
                        last_count = Instant::now();
                    }
                    if pending > MAX_PENDING_BACKLOG && !s.source.is_hot() && s.peers.is_empty() {
                        continue; // triyaj kuyruğu dolu: soğuk örneği alma
                    }
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
            harvester: self
                .harvester_stats
                .iter()
                .map(|s| s.snapshot())
                .reduce(|a, b| a.merge(b))
                .unwrap_or_else(|| dragnet_dht::Stats::default().snapshot()),
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
    let hinted = !hints.is_empty();
    let started = Instant::now();
    let _ = store.bump_metric(metric::FETCH_ATTEMPT, now_unix()).await;
    match fetcher.fetch_with_hints(infohash, hints).await {
        Ok(record) => {
            let files = record.files.len();
            let name = record.name.clone();
            let _ = store.bump_metric(metric::FETCH_OK, now_unix()).await;
            // İpucu adresleri, DHT araması devreye girmeden (HINT_GRACE) sonuç verdiyse
            // bu çekim ağa hiç arama maliyeti ödetmedi — F13'ün asıl kazancı budur.
            if hinted && started.elapsed() < dragnet_meta::HINT_GRACE {
                let _ = store.bump_metric(metric::FETCH_OK_HINTED, now_unix()).await;
            }
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
