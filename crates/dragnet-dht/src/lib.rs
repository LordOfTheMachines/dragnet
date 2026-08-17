// SPDX-License-Identifier: AGPL-3.0-only
//! dragnet-dht — Mainline DHT hasatçısı (Faz 1).
//!
//! BitTorrent Mainline DHT'ye bir düğüm olarak katılır ve ağda uçuşan
//! `get_peers` / `announce_peer` sorgularını **pasif** olarak dinleyerek infohash
//! toplar. Toplananları [`dragnet_core::InfoHash`] olarak sınırlı bir tokio
//! kanalından yayar.
//!
//! ## Neden kendi KRPC katmanı?
//! Değerlendirilen crate'ler pasif hasat için gereken "gelen sorgu gövdesini görme"
//! yeteneğini vermiyor (bkz. [`krpc`] modül başlığı ve `docs/ARCHITECTURE.md` §7):
//! - `mainline` (v8) olgun ve bakımlı, ama `RequestFilter` yalnız `bool` döndürüyor;
//!   sorgunun içindeki `info_hash`'i taşıyan tip crate dışına export edilmemiş.
//! - `rustydht-lib` bu işe uygundu ama artık ne crates.io'da ne de GitHub'da mevcut.
//!
//! Bu yüzden `mainline`'ı **temel** olarak seçtik (BEP-42 uyumlu düğüm kimliği
//! üretimi `mainline::Id`, bootstrap listesi, ve Faz 2'de `get_peers` istemcisi) ve
//! pasif dinlemeyi kendi ince KRPC dinleyicimizle yapıyoruz.
//!
//! ## Hasat stratejisi (magnetico benzeri)
//! Pasif dinleme tek başına yeterince trafik görmez; ağın bizi tanıması gerekir.
//! Bu yüzden aktif olarak rastgele hedeflere `find_node` göndeririz: bu bizi birçok
//! düğümün yönlendirme tablosuna sokar ve karşılığında onların `get_peers`
//! sorgularını bize yöneltmesini sağlar. Düğüm kimliğini periyodik döndürerek
//! kimlik uzayında farklı bölgelerin trafiğini görürüz ("horizontal crawling").

mod dedup;
mod krpc;
mod ratelimit;

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use dragnet_core::InfoHash;
use mainline::{Id, DEFAULT_BOOTSTRAP_NODES};

use dedup::RecentSet;
use krpc::{Message, Method};
use ratelimit::TokenBucket;

pub use krpc::ID_LEN;

/// Hasatçı yapılandırması. [`Default`] makul üretim değerleri verir.
#[derive(Debug, Clone)]
pub struct HarvesterConfig {
    /// Dinlenecek IPv4 arayüzü. Varsayılan `0.0.0.0` (tüm arayüzler).
    pub bind_address: Ipv4Addr,
    /// UDP portu. `0` = işletim sisteminin atadığı efemer port (her zaman bağlanır).
    /// Sabit ve yönlendirilmiş (port-forward) bir port pasif hasadı artırır.
    pub port: u16,
    /// İnfohash yayınlanan sınırlı kanalın kapasitesi (backpressure sınırı).
    pub channel_capacity: usize,
    /// Saniyedeki azami giden sorgu (find_node) — basit rate-limit.
    pub max_queries_per_sec: f64,
    /// Aktif crawl tık aralığı.
    pub crawl_tick: Duration,
    /// Her tıkta sorgulanacak azami düğüm sayısı (rate-limit ile birlikte sınırlar).
    pub crawl_batch: usize,
    /// Düğüm kimliğinin döndürülme aralığı.
    pub id_rotation: Duration,
    /// Yakın zamanda görülen benzersiz infohash filtresinin kapasitesi.
    pub dedup_capacity: usize,
    /// Sorgulanmayı bekleyen düğüm kuyruğunun azami boyutu.
    pub node_queue_capacity: usize,
}

impl Default for HarvesterConfig {
    fn default() -> Self {
        Self {
            bind_address: Ipv4Addr::UNSPECIFIED,
            port: 0,
            channel_capacity: 1024,
            // Nazik varsayılan: ev router'larının bağlantı-izleme (conntrack)
            // tablosunu doldurup interneti kilitlememek için düşük tutuldu.
            // Port-forward + iyi bağlantısı olanlar artırabilir.
            max_queries_per_sec: 50.0,
            crawl_tick: Duration::from_millis(100),
            crawl_batch: 4,
            id_rotation: Duration::from_secs(600),
            dedup_capacity: 1 << 18,
            node_queue_capacity: 8192,
        }
    }
}

/// Çalışma sayaçları (atomik). [`Harvester::stats`] üzerinden okunur.
#[derive(Debug, Default)]
pub struct Stats {
    pub received_packets: AtomicU64,
    pub queries_seen: AtomicU64,
    pub get_peers_seen: AtomicU64,
    pub announce_seen: AtomicU64,
    pub responses_seen: AtomicU64,
    pub nodes_learned: AtomicU64,
    pub unique_infohashes: AtomicU64,
    pub duplicates: AtomicU64,
    /// Kanal dolu olduğu için düşürülen benzersiz infohash sayısı (backpressure).
    pub dropped_channel_full: AtomicU64,
    pub queries_sent: AtomicU64,
    /// BEP-51 `sample_infohashes` yanıtlarından gelen toplam infohash örneği sayısı.
    pub samples_seen: AtomicU64,
    /// Rate-limit nedeniyle gönderilemeyen crawl sorgusu sayısı.
    pub rate_limited: AtomicU64,
}

/// Anlık sayaç görüntüsü (kolay loglama için).
#[derive(Debug, Clone, Copy)]
pub struct StatsSnapshot {
    pub received_packets: u64,
    pub queries_seen: u64,
    pub get_peers_seen: u64,
    pub announce_seen: u64,
    pub responses_seen: u64,
    pub nodes_learned: u64,
    pub unique_infohashes: u64,
    pub duplicates: u64,
    pub dropped_channel_full: u64,
    pub queries_sent: u64,
    pub samples_seen: u64,
    pub rate_limited: u64,
}

impl Stats {
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            received_packets: self.received_packets.load(Ordering::Relaxed),
            queries_seen: self.queries_seen.load(Ordering::Relaxed),
            get_peers_seen: self.get_peers_seen.load(Ordering::Relaxed),
            announce_seen: self.announce_seen.load(Ordering::Relaxed),
            responses_seen: self.responses_seen.load(Ordering::Relaxed),
            nodes_learned: self.nodes_learned.load(Ordering::Relaxed),
            unique_infohashes: self.unique_infohashes.load(Ordering::Relaxed),
            duplicates: self.duplicates.load(Ordering::Relaxed),
            dropped_channel_full: self.dropped_channel_full.load(Ordering::Relaxed),
            queries_sent: self.queries_sent.load(Ordering::Relaxed),
            samples_seen: self.samples_seen.load(Ordering::Relaxed),
            rate_limited: self.rate_limited.load(Ordering::Relaxed),
        }
    }
}

/// Bir infohash görülmesinin kaynağı. Pasif kaynaklar (`GetPeers`, `Announce`) "şu anda
/// birileri bunu arıyor/sunuyor" demektir → metadata çekiminde öncelik sinyali ("sıcak").
/// `Sample` (BEP-51) düğüm deposundan örnek: eski/ölü olabilir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SightingSource {
    Sample,
    GetPeers,
    Announce,
}

impl SightingSource {
    /// Sıcak sinyal mi (pasif trafik)?
    pub fn is_hot(self) -> bool {
        !matches!(self, Self::Sample)
    }
}

/// Hasat akışının bir öğesi: infohash + kaynak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sighting {
    pub infohash: InfoHash,
    pub source: SightingSource,
}

/// Çalışan hasatçı. `infohashes` alanından benzersiz infohash akışı okunur.
/// Bırakılınca (drop) arka plan görevleri de durur.
pub struct Harvester {
    /// Benzersiz infohash akışı (sınırlı kanal).
    pub infohashes: mpsc::Receiver<Sighting>,
    stats: Arc<Stats>,
    tasks: Vec<JoinHandle<()>>,
    local_addr: SocketAddr,
}

impl Harvester {
    /// Sayaçların paylaşımlı görünümü.
    pub fn stats(&self) -> Arc<Stats> {
        Arc::clone(&self.stats)
    }

    /// Soketin bağlandığı yerel adres (efemer port `0` verildiyse gerçek portu görmek için).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

impl Drop for Harvester {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// Hasatçının paylaşımlı iç durumu.
struct Shared {
    socket: UdpSocket,
    our_id: Mutex<[u8; ID_LEN]>,
    nodes: Mutex<VecDeque<SocketAddrV4>>,
    limiter: Mutex<TokenBucket>,
    dedup: Mutex<RecentSet>,
    /// Pasif (sıcak) sighting'ler ana dedup'ı atlar ama kısa pencerede tekrarları bastırır.
    hot_dedup: Mutex<RecentSet>,
    stats: Arc<Stats>,
    sink: mpsc::Sender<Sighting>,
    /// Giden yanıtlar (ping/find_node/get_peers ack) — ayrı gönderici task drenajlar,
    /// böylece yanıt gönderimi paket ALIMINI bloklamaz (yük altında UDP kaybını azaltır).
    reply_tx: mpsc::Sender<(Vec<u8>, SocketAddrV4)>,
    node_queue_capacity: usize,
    txid: AtomicU32,
}

impl Shared {
    fn our_id(&self) -> [u8; ID_LEN] {
        *self.our_id.lock().unwrap()
    }

    /// Öğrenilen düğümleri kuyruğa ekler (kapasiteyle sınırlı). Eklenen sayısını döner.
    fn push_nodes(&self, learned: &[SocketAddrV4]) -> u64 {
        let mut q = self.nodes.lock().unwrap();
        let mut added = 0;
        for &n in learned {
            if q.len() >= self.node_queue_capacity {
                break;
            }
            q.push_back(n);
            added += 1;
        }
        added
    }

    fn pop_nodes(&self, n: usize) -> Vec<SocketAddrV4> {
        let mut q = self.nodes.lock().unwrap();
        (0..n).filter_map(|_| q.pop_front()).collect()
    }

    fn queue_len(&self) -> usize {
        self.nodes.lock().unwrap().len()
    }

    fn next_txid(&self) -> [u8; 2] {
        (self.txid.fetch_add(1, Ordering::Relaxed) as u16).to_be_bytes()
    }
}

/// Bir hasatçı başlatır. Soketi bağlar (bağlama hataları burada yüzeye çıkar),
/// arka plan görevlerini spawn eder ve infohash alıcısını döndürür.
pub async fn spawn(config: HarvesterConfig) -> std::io::Result<Harvester> {
    // İstenen porta bağlan; başka uygulama (ör. qBittorrent 6881) tutuyorsa
    // çökmek yerine efemer porta düş.
    let socket = match UdpSocket::bind(SocketAddrV4::new(config.bind_address, config.port)).await {
        Ok(s) => s,
        Err(e) if config.port != 0 => {
            warn!(
                port = config.port,
                error = %e,
                "harvester portu bağlanamadı (başka uygulama kullanıyor olabilir, \
                 örn. qBittorrent 6881); efemer porta düşülüyor"
            );
            UdpSocket::bind(SocketAddrV4::new(config.bind_address, 0)).await?
        }
        Err(e) => return Err(e),
    };
    let local_addr = socket.local_addr()?;
    info!(%local_addr, "dragnet-dht dinliyor");

    let (tx, rx) = mpsc::channel(config.channel_capacity);
    let (reply_tx, reply_rx) = mpsc::channel::<(Vec<u8>, SocketAddrV4)>(512);
    let stats = Arc::new(Stats::default());

    let shared = Arc::new(Shared {
        socket,
        our_id: Mutex::new(*Id::random().as_bytes()),
        nodes: Mutex::new(VecDeque::with_capacity(config.node_queue_capacity)),
        limiter: Mutex::new(TokenBucket::new(config.max_queries_per_sec)),
        dedup: Mutex::new(RecentSet::new(config.dedup_capacity)),
        hot_dedup: Mutex::new(RecentSet::new(4096)),
        stats: Arc::clone(&stats),
        sink: tx,
        reply_tx,
        node_queue_capacity: config.node_queue_capacity,
        txid: AtomicU32::new(0),
    });

    // Kuyruğu bootstrap düğümleriyle doldur.
    seed_bootstrap(&shared).await;

    let tasks = vec![
        tokio::spawn(recv_loop(Arc::clone(&shared))),
        tokio::spawn(reply_loop(Arc::clone(&shared), reply_rx)),
        tokio::spawn(crawl_loop(Arc::clone(&shared), config.clone())),
        tokio::spawn(rotate_loop(Arc::clone(&shared), config.id_rotation)),
    ];

    Ok(Harvester {
        infohashes: rx,
        stats,
        tasks,
        local_addr,
    })
}

/// Bootstrap host'larını çözer ve düğüm kuyruğuna ekler.
async fn seed_bootstrap(shared: &Shared) {
    let mut resolved = Vec::new();
    for host in DEFAULT_BOOTSTRAP_NODES {
        match tokio::net::lookup_host(host).await {
            Ok(addrs) => {
                for a in addrs {
                    if let SocketAddr::V4(v4) = a {
                        resolved.push(v4);
                    }
                }
            }
            Err(e) => debug!(host, error = %e, "bootstrap çözümlenemedi"),
        }
    }
    if resolved.is_empty() {
        warn!("hiçbir bootstrap düğümü çözülemedi; ağ erişimi yok olabilir");
    } else {
        shared.push_nodes(&resolved);
    }
}

/// Gelen UDP paketlerini dinleyip işleyen ana pasif hasat döngüsü.
async fn recv_loop(shared: Arc<Shared>) {
    let mut buf = [0u8; 2048];
    loop {
        let (len, from) = match shared.socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                debug!(error = %e, "recv_from hatası");
                continue;
            }
        };
        let from_v4 = match from {
            SocketAddr::V4(v4) => v4,
            SocketAddr::V6(_) => continue,
        };
        shared
            .stats
            .received_packets
            .fetch_add(1, Ordering::Relaxed);
        handle_incoming(&shared, &buf[..len], from_v4).await;
    }
}

/// Giden yanıt kuyruğunu drenajlayan ayrı görev — gönderim gecikmesi recv_loop'u
/// bloklamaz (aynı sokette eşzamanlı send/recv güvenlidir).
async fn reply_loop(shared: Arc<Shared>, mut rx: mpsc::Receiver<(Vec<u8>, SocketAddrV4)>) {
    while let Some((pkt, addr)) = rx.recv().await {
        let _ = shared.socket.send_to(&pkt, addr).await;
    }
}

/// Tek bir gelen paketi işler: infohash hasat eder ve gerektiğinde yanıt yollar.
async fn handle_incoming(shared: &Shared, data: &[u8], from: SocketAddrV4) {
    let msg = match krpc::parse(data) {
        Some(m) => m,
        None => return,
    };

    match msg {
        Message::Query(q) => {
            shared.stats.queries_seen.fetch_add(1, Ordering::Relaxed);
            let id = shared.our_id();
            let reply = match q.method {
                Method::GetPeers => {
                    shared.stats.get_peers_seen.fetch_add(1, Ordering::Relaxed);
                    if let Some(ih) = q.info_hash {
                        harvest(shared, ih, SightingSource::GetPeers);
                    }
                    Some(krpc::build_get_peers_response(
                        &q.txid,
                        &id,
                        &token_for(from),
                    ))
                }
                Method::AnnouncePeer => {
                    shared.stats.announce_seen.fetch_add(1, Ordering::Relaxed);
                    if let Some(ih) = q.info_hash {
                        harvest(shared, ih, SightingSource::Announce);
                    }
                    Some(krpc::build_response_id_only(&q.txid, &id))
                }
                // ping / find_node / bilinmeyen: tabloda kalmak için nazikçe ack'le.
                Method::Ping | Method::FindNode | Method::Other => {
                    Some(krpc::build_response_id_only(&q.txid, &id))
                }
            };
            if let Some(pkt) = reply {
                // Gönderimi kuyruğa koy (bloklamadan); reply_loop drenajlar.
                // Kuyruk doluysa yanıtı düşür (backpressure) — recv asla bloklanmaz.
                let _ = shared.reply_tx.try_send((pkt, from));
            }
        }
        Message::Response(r) => {
            shared.stats.responses_seen.fetch_add(1, Ordering::Relaxed);
            if !r.nodes.is_empty() {
                let added = shared.push_nodes(&r.nodes);
                shared
                    .stats
                    .nodes_learned
                    .fetch_add(added, Ordering::Relaxed);
            }
            // BEP-51: yanıttaki infohash örneklerini aktif olarak hasat et.
            if !r.samples.is_empty() {
                shared
                    .stats
                    .samples_seen
                    .fetch_add(r.samples.len() as u64, Ordering::Relaxed);
                for ih in r.samples {
                    harvest(shared, ih, SightingSource::Sample);
                }
            }
        }
        Message::Other => {}
    }
}

/// Bir infohash görülmesini dedup'tan geçirip kanala yayar. Örnekler (Sample) büyük
/// dedup'tan geçer; pasif kaynaklar (sıcak) daha önce örneklenmiş olsa da yayılır — yalnız
/// kısa pencerede tekrarları bastırılır — çünkü "şu anda aranıyor" sinyali değerlidir.
fn harvest(shared: &Shared, ih: [u8; ID_LEN], source: SightingSource) {
    let is_new = if source.is_hot() {
        shared.hot_dedup.lock().unwrap().insert(ih)
    } else {
        shared.dedup.lock().unwrap().insert(ih)
    };
    if !is_new {
        shared.stats.duplicates.fetch_add(1, Ordering::Relaxed);
        return;
    }
    match shared.sink.try_send(Sighting {
        infohash: InfoHash::from_bytes(ih),
        source,
    }) {
        Ok(()) => {
            shared
                .stats
                .unique_infohashes
                .fetch_add(1, Ordering::Relaxed);
        }
        Err(mpsc::error::TrySendError::Full(_)) => {
            // Backpressure: tüketici yetişemiyor, zarifçe düşür (çökme yok).
            shared
                .stats
                .dropped_channel_full
                .fetch_add(1, Ordering::Relaxed);
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            // Alıcı bırakılmış; sessizce yok say.
        }
    }
}

/// Aktif crawl: rate-limit dahilinde kuyruktaki düğümlere sorgu yollar.
///
/// Çoğunlukla BEP-51 `sample_infohashes` (aktif, NAT-dostu hasat; yanıt hem
/// infohash örnekleri hem yeni düğümler verir), her 4 sorgudan biri `find_node`
/// (BEP-51 desteklemeyen düğümlerden de düğüm keşfini garantilemek için).
async fn crawl_loop(shared: Arc<Shared>, config: HarvesterConfig) {
    let mut ticker = tokio::time::interval(config.crawl_tick);
    let mut counter: u32 = 0;
    loop {
        ticker.tick().await;

        // Kuyruk boşaldıysa bootstrap'tan yeniden tohumla.
        if shared.queue_len() == 0 {
            seed_bootstrap(&shared).await;
        }

        let targets = shared.pop_nodes(config.crawl_batch);
        if targets.is_empty() {
            continue;
        }
        let our_id = shared.our_id();
        for node in targets {
            // Rate-limit: jeton yoksa bu sorguyu atla.
            let allowed = shared.limiter.lock().unwrap().try_take();
            if !allowed {
                shared.stats.rate_limited.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let target = *Id::random().as_bytes();
            let txid = shared.next_txid();
            counter = counter.wrapping_add(1);
            let pkt = if counter.is_multiple_of(4) {
                krpc::build_find_node(&txid, &our_id, &target)
            } else {
                krpc::build_sample_infohashes(&txid, &our_id, &target)
            };
            match shared.socket.send_to(&pkt, node).await {
                Ok(_) => {
                    shared.stats.queries_sent.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => debug!(error = %e, "crawl sorgusu gönderilemedi"),
            }
        }
    }
}

/// Düğüm kimliğini periyodik döndürür (kimlik uzayında konum değiştirir).
async fn rotate_loop(shared: Arc<Shared>, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    ticker.tick().await; // ilk anlık tık: hemen döndürme.
    loop {
        ticker.tick().await;
        let new_id = *Id::random().as_bytes();
        *shared.our_id.lock().unwrap() = new_id;
        debug!("düğüm kimliği döndürüldü");
    }
}

/// `get_peers` yanıtı için basit, adrese bağlı token üretir (doğrulamıyoruz).
fn token_for(addr: SocketAddrV4) -> [u8; 4] {
    addr.ip().octets()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_sane() {
        let c = HarvesterConfig::default();
        assert!(c.max_queries_per_sec > 0.0);
        assert!(c.channel_capacity > 0);
        assert!(c.crawl_batch > 0);
    }

    #[tokio::test]
    async fn spawns_and_binds_ephemeral_port() {
        // Ağ trafiği beklemeden yalnız bağlanmayı ve temiz kapanmayı doğrula.
        let cfg = HarvesterConfig {
            port: 0,
            ..Default::default()
        };
        let harvester = spawn(cfg).await.expect("bağlanmalı");
        assert_ne!(harvester.local_addr().port(), 0, "efemer port atanmalı");
        let s = harvester.stats.snapshot();
        assert_eq!(s.unique_infohashes, 0);
        // drop → görevler abort olur.
    }
}
