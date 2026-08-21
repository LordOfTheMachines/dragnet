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
/// KRPC (BEP-5) paket kurucu/çözücüleri. Uygulama tarafı UDP sağlık yoklaması için
/// `build_find_node`'u kullanır (bkz. dragnet-app: ağ sağlığı kartı).
pub mod krpc;
mod ratelimit;

use std::collections::{HashMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    /// Her BEP-51 örnek yanıtı için en fazla kaç yeni infohash'e o düğüme doğrudan
    /// `get_peers` sorulacağı (taze peer ipucu; rate-limit bütçesinden harcanır). 0 = kapalı.
    pub followups_per_sample: usize,
    /// Sorgulanmayı bekleyen düğüm kuyruğunun azami boyutu.
    pub node_queue_capacity: usize,
    /// Düğüm kimliğinin ve öğrenilen düğümlerin saklanacağı dosya (`None` = kalıcılık yok).
    ///
    /// Neden: pasif hasat (gelen `announce_peer`/`get_peers`) ancak ağdaki yönlendirme
    /// tablolarında yer edinince gelir ve bu birikim saatler alır. Kimlik her açılışta
    /// yeniden üretilirse ağ bizi HER SEFERİNDE yeni bir düğüm sanar ve birikim sıfırlanır
    /// — aynı gerekçeyle kimlik döndürme aralığı da 10 dk'dan 60 dk'ya çıkarılmıştı.
    /// Dosyada ayrıca son bilinen düğümler tutulur; açılışta DNS'i beklemeden ağa dönülür.
    pub state_path: Option<std::path::PathBuf>,
}

/// Durum dosyasına yazılacak azami düğüm sayısı (6 bayt/düğüm → ~6 KB).
const STATE_NODES: usize = 1000;
/// Durum dosyasının kaydedilme aralığı.
const STATE_SAVE_INTERVAL: Duration = Duration::from_secs(300);
/// Düğüm kuyruğu kuruduğunda yeniden tohumlama denemeleri arasındaki en kısa süre.
const RESEED_MIN_INTERVAL: Duration = Duration::from_secs(5);

impl Default for HarvesterConfig {
    fn default() -> Self {
        Self {
            bind_address: Ipv4Addr::UNSPECIFIED,
            // Varsayılan 6881: modemde yönlendirilmesi en olası port; doluysa efemer porta düşer.
            port: 6881,
            channel_capacity: 1024,
            // Nazik varsayılan: ev router'larının bağlantı-izleme (conntrack)
            // tablosunu doldurup interneti kilitlememek için düşük tutuldu.
            // Port-forward + iyi bağlantısı olanlar artırabilir.
            max_queries_per_sec: 50.0,
            crawl_tick: Duration::from_millis(100),
            // Tık başına gönderilen sorgu. 100 ms tık ile bu, saniyede en fazla
            // `crawl_batch × 10` sorgu demektir — yani 4 iken bütçe 50 verilse bile tavan
            // 40'ta kalıyordu. Harvester sorguları (find_node / sample_infohashes) TEK
            // PAKETTİR; DHT *aramalarının* (~50 paket) aksine ucuzdur, dolayısıyla
            // infohash keşfini hızlandırmanın en ucuz yolu buradan geçer. Asıl kısılması
            // gereken triyaj/çekim aramalarıdır (bkz. `docs/CEKIM-HIZI.md` §4).
            crawl_batch: 16,
            // KİMLİK DÖNDÜRME VARSAYILAN OLARAK KAPALI (0). Gerekçe: her döndürmede
            // (BEP-42 rastgele bileşeni değişir) ağ bizi YENİ bir düğüm sanar ve
            // tablolarındaki girdimiz bayatlar; pasif trafik (announce/get_peers) ise ancak
            // tablolarda kalıcı yer edinince gelir. Ölçümle bir kez 10 dk → 60 dk yapılmış
            // ve hasat iyileşmişti; aynı gerekçe sonuna kadar götürülünce döndürmenin
            // kendisi gereksiz kalıyor: "kimlik uzayında dolaşma" faydası ZATEN sağlanıyor,
            // çünkü `crawl_loop` her sorguda hedefi (`target`) rastgele seçiyor — düğüm
            // kimliğini de döndürmek örnekleme çeşitliliğine ek bir şey katmıyor, yalnız
            // yerleşikliği bozuyor. Deneme yapmak isteyen bu alanı sıfırdan farklı verir.
            id_rotation: Duration::ZERO,
            dedup_capacity: 1 << 18,
            // Her BEP-51 örneğinden sonra daha çok doğrudan get_peers: peer ipuçlu (yani
            // CANLI olduğu bilinen) aday üretmenin en ucuz yolu. Ölçüm: kuyruğun %98'i
            // soğuk örnekleme, çekim başarısı ~%2; ipuçlu adaylarda peer zaten bilinir.
            followups_per_sample: 8,
            node_queue_capacity: 8192,
            state_path: None,
        }
    }
}

/// Kalıcı hasatçı durumu: düğüm kimliği + son bilinen düğümler.
///
/// Biçim (küçük ve elle okunabilir olması gerekmiyor): `DGN1` sihirli sözcüğü,
/// 20 bayt kimlik, ardından 6 baytlık compact IPv4 düğüm kayıtları.
mod state {
    use super::{ID_LEN, STATE_NODES};
    use std::net::{Ipv4Addr, SocketAddrV4};
    use std::path::Path;

    const MAGIC: &[u8; 4] = b"DGN1";

    /// Durumu okur; dosya yoksa/bozuksa `None` (çağıran sıfırdan başlar).
    pub fn load(path: &Path) -> Option<([u8; ID_LEN], Vec<SocketAddrV4>)> {
        let buf = std::fs::read(path).ok()?;
        if buf.len() < MAGIC.len() + ID_LEN || &buf[..4] != MAGIC {
            return None;
        }
        let id: [u8; ID_LEN] = buf[4..4 + ID_LEN].try_into().ok()?;
        let nodes = buf[4 + ID_LEN..]
            .chunks_exact(6)
            .map(|c| {
                SocketAddrV4::new(
                    Ipv4Addr::new(c[0], c[1], c[2], c[3]),
                    u16::from_be_bytes([c[4], c[5]]),
                )
            })
            .filter(|a| a.port() != 0 && !a.ip().is_unspecified())
            .collect();
        Some((id, nodes))
    }

    /// Durumu atomik yazar (önce geçici dosya, sonra yerine taşı) — yarıda kesilirse
    /// eski durum bozulmadan kalır.
    pub fn save(path: &Path, id: &[u8; ID_LEN], nodes: &[SocketAddrV4]) -> std::io::Result<()> {
        let mut buf = Vec::with_capacity(4 + ID_LEN + nodes.len().min(STATE_NODES) * 6);
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(id);
        for n in nodes.iter().take(STATE_NODES) {
            buf.extend_from_slice(&n.ip().octets());
            buf.extend_from_slice(&n.port().to_be_bytes());
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &buf)?;
        std::fs::rename(&tmp, path)
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
    /// Takip get_peers ile `values` (taze peer) alınan örnek sayısı (Faz E).
    pub peer_hints: AtomicU64,
    /// Soket hataları. Windows'ta bunlar sessizce yutulduğunda hasat, sebebi görünmeden
    /// durur: bir hedef ICMP "port unreachable" döndürünce Windows bunu SONRAKİ soket
    /// çağrısında `WSAECONNRESET` olarak verir (bkz. `disable_conn_reset`).
    pub send_errors: AtomicU64,
    pub recv_errors: AtomicU64,
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
    pub peer_hints: u64,
    pub send_errors: u64,
    pub recv_errors: u64,
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
            peer_hints: self.peer_hints.load(Ordering::Relaxed),
            send_errors: self.send_errors.load(Ordering::Relaxed),
            recv_errors: self.recv_errors.load(Ordering::Relaxed),
        }
    }
}

/// Bir infohash görülmesinin kaynağı. Pasif kaynaklar (`GetPeers`, `Announce`) "şu anda
/// birileri bunu arıyor/sunuyor" demektir → metadata çekiminde öncelik sinyali ("sıcak").
/// `Sample` (BEP-51) düğüm deposundan örnek: eski/ölü olabilir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SightingSource {
    Sample,
    /// BEP-51 örneğini veren düğüme doğrudan `get_peers` sorulup `values` alındı:
    /// infohash için **şu anda** peer var (taze) — `Sighting::peers` dolu.
    SamplePeers,
    GetPeers,
    Announce,
}

impl SightingSource {
    /// Sıcak sinyal mi (şu anda peer/ilgi var)?
    pub fn is_hot(self) -> bool {
        !matches!(self, Self::Sample)
    }
}

/// Hasat akışının bir öğesi: infohash + kaynak (+ varsa peer ipuçları).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sighting {
    pub infohash: InfoHash,
    pub source: SightingSource,
    /// Doğrudan bilinen peer adresleri (fetcher önce bunları dener; DHT aramasını atlar).
    pub peers: Vec<SocketAddrV4>,
    /// Bu infohash'in dedup penceresinde kaç kez DAHA görüldüğü (toplu popülerlik
    /// sayacı; periyodik flush'ta gelir, `source = Sample`). 0 = normal sighting.
    pub repeats: u32,
}

/// Çalışan hasatçı. `infohashes` alanından benzersiz infohash akışı okunur.
/// Bırakılınca (drop) arka plan görevleri de durur.
pub struct Harvester {
    /// Benzersiz infohash akışı (sınırlı kanal).
    pub infohashes: mpsc::Receiver<Sighting>,
    /// Aynı kanalın yazan ucu (ek kimliklerin akışını buraya aktarmak için).
    sink: mpsc::Sender<Sighting>,
    stats: Arc<Stats>,
    tasks: Vec<JoinHandle<()>>,
    local_addr: SocketAddr,
}

impl Harvester {
    /// Bu hasatçının infohash kanalına yazan uç. Çoklu kimlik çalıştırıldığında ek
    /// kimliklerin akışını tek bir tüketiciye aktarmak için kullanılır (F9).
    pub fn sink(&self) -> mpsc::Sender<Sighting> {
        self.sink.clone()
    }

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
    /// Yanıtlardaki `ip` alanından öğrenilen dış adresimiz (BEP-42). Bilinince düğüm
    /// kimliği bu IP'den türetilir: BEP-42 uyumsuz kimlikleri modern istemciler
    /// yönlendirme tablosuna ALMAZ, dolayısıyla bize pasif trafik (announce/get_peers)
    /// gelmez — hasadın en canlı kaynağı budur.
    public_ip: Mutex<Option<std::net::Ipv4Addr>>,
    nodes: Mutex<VecDeque<SocketAddrV4>>,
    limiter: Mutex<TokenBucket>,
    dedup: Mutex<RecentSet>,
    /// Pasif (sıcak) sighting'ler ana dedup'ı atlar ama kısa pencerede tekrarları bastırır.
    hot_dedup: Mutex<RecentSet>,
    /// Dedup'un yuttuğu tekrarların sayacı (popülerlik): periyodik olarak `repeats`
    /// sighting'leri olarak akıtılır — BEP-51'de popüler torrent'ler çok düğümde saklandığı
    /// için örneklerde defalarca görünür; bu sinyal çekim önceliğine gider.
    dup_counts: Mutex<HashMap<[u8; ID_LEN], u32>>,
    stats: Arc<Stats>,
    sink: mpsc::Sender<Sighting>,
    /// Giden yanıtlar (ping/find_node/get_peers ack) — ayrı gönderici task drenajlar,
    /// böylece yanıt gönderimi paket ALIMINI bloklamaz (yük altında UDP kaybını azaltır).
    reply_tx: mpsc::Sender<(Vec<u8>, SocketAddrV4)>,
    node_queue_capacity: usize,
    txid: AtomicU32,
    /// Gönderdiğimiz `get_peers` sorguları: txid → (infohash, zaman). Yanıt gelince
    /// `values` bu infohash'e bağlanır. Eskiler (>30 s) budanır.
    pending_gp: Mutex<HashMap<u16, ([u8; ID_LEN], Instant)>>,
    /// Örnek yanıtı başına en fazla kaç takip get_peers (rate-limit içinden harcanır).
    followups_per_sample: usize,
}

impl Shared {
    fn our_id(&self) -> [u8; ID_LEN] {
        *self.our_id.lock().unwrap()
    }

    /// Öğrenilen düğümleri kuyruğa ekler (kapasiteyle sınırlı). Eklenen sayısını döner.
    /// Öğrenilen düğümleri kuyruğa ekler. Kuyruk doluysa **en eskisi düşürülür** —
    /// yeni gelen atılmaz.
    ///
    /// Neden: eskiden kapasiteye ulaşınca yeni düğümler atılıyordu, yani kuyruk bir kez
    /// dolduktan sonra **hep aynı düğümler** dolaşıyordu. BEP-51 örneklemesinde bu,
    /// aynı düğümlere tekrar tekrar sorup aynı infohash'leri almak demek: ölçümde
    /// saniyede ~90 örnek geliyordu ama bunların %99,7'si dedup'a takılıyor, yeni kayıt
    /// 0,24/sn'de kalıyordu. Kuyruğun tazelenmesi keşif çeşitliliğinin ön koşuludur.
    fn push_nodes(&self, learned: &[SocketAddrV4]) -> u64 {
        let mut q = self.nodes.lock().unwrap();
        let mut added = 0;
        for &n in learned {
            while q.len() >= self.node_queue_capacity {
                q.pop_front();
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
    let socket = match bind_socket(SocketAddrV4::new(config.bind_address, config.port)).await {
        Ok(s) => s,
        Err(e) if config.port != 0 => {
            warn!(
                port = config.port,
                error = %e,
                "harvester portu bağlanamadı (başka uygulama kullanıyor olabilir, \
                 örn. qBittorrent 6881); efemer porta düşülüyor"
            );
            bind_socket(SocketAddrV4::new(config.bind_address, 0)).await?
        }
        Err(e) => return Err(e),
    };
    let local_addr = socket.local_addr()?;
    info!(%local_addr, "dragnet-dht dinliyor");

    let (tx, rx) = mpsc::channel(config.channel_capacity);
    let (reply_tx, reply_rx) = mpsc::channel::<(Vec<u8>, SocketAddrV4)>(512);
    let stats = Arc::new(Stats::default());

    // Kalıcı durum: önceki oturumun kimliği ve son bilinen düğümleri. Kimliğin
    // korunması pasif hasat için kritiktir — ağın yönlendirme tablolarındaki yerimiz
    // ancak kimlik sabit kalırsa birikir.
    let persisted = config.state_path.as_deref().and_then(state::load);
    let (initial_id, cached_nodes) = match persisted {
        Some((id, nodes)) => {
            info!(
                nodes = nodes.len(),
                "önceki DHT kimliği ve düğümler geri yüklendi"
            );
            (id, nodes)
        }
        None => (*Id::random().as_bytes(), Vec::new()),
    };

    let shared = Arc::new(Shared {
        socket,
        our_id: Mutex::new(initial_id),
        // BEP-42 (aşağıda): dış IP öğrenilince kimlik ondan türetilir.
        public_ip: Mutex::new(None),
        nodes: Mutex::new(VecDeque::with_capacity(config.node_queue_capacity)),
        limiter: Mutex::new(TokenBucket::new(config.max_queries_per_sec)),
        dedup: Mutex::new(RecentSet::new(config.dedup_capacity)),
        hot_dedup: Mutex::new(RecentSet::new(4096)),
        dup_counts: Mutex::new(HashMap::new()),
        stats: Arc::clone(&stats),
        sink: tx,
        reply_tx,
        node_queue_capacity: config.node_queue_capacity,
        txid: AtomicU32::new(0),
        pending_gp: Mutex::new(HashMap::new()),
        followups_per_sample: config.followups_per_sample,
    });

    // Önce önbellekteki düğümler (DNS beklemeden ağa dön), sonra bootstrap.
    if !cached_nodes.is_empty() {
        shared.push_nodes(&cached_nodes);
    }
    seed_bootstrap(&shared).await;

    let mut tasks = vec![
        tokio::spawn(recv_loop(Arc::clone(&shared))),
        tokio::spawn(reply_loop(Arc::clone(&shared), reply_rx)),
        tokio::spawn(crawl_loop(Arc::clone(&shared), config.clone())),
        tokio::spawn(flush_repeats_loop(Arc::clone(&shared))),
        tokio::spawn(rebootstrap_loop(Arc::clone(&shared))),
    ];
    // Kimlik döndürme yalnız açıkça istenirse (0 = kapalı; varsayılan kapalı).
    if !config.id_rotation.is_zero() {
        tasks.push(tokio::spawn(rotate_loop(
            Arc::clone(&shared),
            config.id_rotation,
        )));
    }
    if let Some(path) = config.state_path.clone() {
        tasks.push(tokio::spawn(save_state_loop(Arc::clone(&shared), path)));
    }

    Ok(Harvester {
        infohashes: rx,
        sink: shared.sink.clone(),
        stats,
        tasks,
        local_addr,
    })
}

/// Windows'ta UDP soketinin `WSAECONNRESET` davranışını kapatır.
///
/// **Neden gerekli:** Bir DHT hedefi kapalıysa yönlendirici/işletim sistemi ICMP
/// "port unreachable" döndürür. Windows bunu, bağlantısız bir UDP soketinde bile,
/// **bir sonraki** `recv_from`/`send_to` çağrısını `WSAECONNRESET` (10054) ile
/// başarısız kılarak bildirir. Bir crawler ise doğası gereği sürekli ölü hedeflere
/// paket yollar, dolayısıyla soket sürekli hata verir; hatalar yutulduğu için de
/// hasat **sebebi görünmeden** yavaşlar: gelen yanıtlar kaybolur, düğüm kuyruğu kurur,
/// giden sorgu bütçesi kullanılamaz.
///
/// `SIO_UDP_CONNRESET = false` bu bildirimi kapatır ve soket, UNIX'teki gibi ICMP
/// hatalarını yok sayar. Windows dışında bu işlev bir şey yapmaz.
#[cfg(windows)]
fn disable_conn_reset(socket: &std::net::UdpSocket) {
    use std::os::windows::io::AsRawSocket;
    use windows::Win32::Networking::WinSock::{WSAIoctl, SIO_UDP_CONNRESET, SOCKET};

    let mut enable: u32 = 0;
    let mut returned: u32 = 0;
    // SAFETY: geçerli bir soket tanıtıcısı ve doğru boyutlu bir `u32` tamponu veriliyor;
    // çağrı yalnız soketin ICMP-hata bildirim bayrağını değiştirir.
    let rc = unsafe {
        WSAIoctl(
            SOCKET(socket.as_raw_socket() as usize),
            SIO_UDP_CONNRESET,
            Some(&mut enable as *mut u32 as *mut std::ffi::c_void),
            std::mem::size_of::<u32>() as u32,
            None,
            0,
            &mut returned,
            None,
            None,
        )
    };
    if rc != 0 {
        warn!("SIO_UDP_CONNRESET ayarlanamadı; ICMP kaynaklı soket hataları görülebilir");
    }
}

#[cfg(not(windows))]
fn disable_conn_reset(_socket: &std::net::UdpSocket) {}

/// İstenen porta bağlanır ve platforma özgü soket ayarlarını uygular.
async fn bind_socket(addr: SocketAddrV4) -> std::io::Result<UdpSocket> {
    // Önce std soketi: `SIO_UDP_CONNRESET` ioctl'i bağlanmadan hemen sonra, tokio'ya
    // devretmeden uygulanmalı.
    let std_socket = std::net::UdpSocket::bind(addr)?;
    disable_conn_reset(&std_socket);
    std_socket.set_nonblocking(true)?;
    UdpSocket::from_std(std_socket)
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
        // DNS geçici olarak çalışmıyorsa (ölçüm: bu makinede gün içinde birkaç kez
        // yaşandı) hasat TAMAMEN durur — düğüm kuyruğu boş kalır, sorgu gidemez,
        // dolayısıyla kimse bizi tanımaz ve pasif trafik de gelmez. Bu yüzden bilinen
        // bootstrap düğümlerinin IP'leri gömülü yedek olarak kullanılır.
        warn!("bootstrap adları çözülemedi; gömülü IP yedekleri kullanılıyor");
        shared.push_nodes(&FALLBACK_BOOTSTRAP);
    } else {
        shared.push_nodes(&resolved);
    }
}

/// DNS çalışmadığında kullanılacak bootstrap düğümü IP'leri (router.utorrent.com,
/// dht.transmissionbt.com, router.bittorrent.com). Adresler değişebilir; yalnız
/// DNS başarısız olduğunda devreye girerler.
const FALLBACK_BOOTSTRAP: [SocketAddrV4; 3] = [
    SocketAddrV4::new(Ipv4Addr::new(82, 221, 103, 244), 6881),
    SocketAddrV4::new(Ipv4Addr::new(87, 98, 162, 88), 6881),
    SocketAddrV4::new(Ipv4Addr::new(67, 215, 246, 10), 6881),
];

/// Düğüm kuyruğu boşaldıysa (bootstrap başarısız ya da ağ kesintisi) yeniden tohumlar.
/// Hasadın sessizce ölmesini engeller: ölçümde bir oturum boyunca BEP-51 örnek/sn = 0
/// ve gelen sorgu = 0 kaldı, çünkü açılışta bootstrap çözülememişti.
async fn rebootstrap_loop(shared: Arc<Shared>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(60));
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let empty = shared.nodes.lock().unwrap().is_empty();
        if empty {
            warn!("düğüm kuyruğu boş — bootstrap yeniden deneniyor");
            seed_bootstrap(&shared).await;
        }
    }
}

/// Gelen UDP paketlerini dinleyip işleyen ana pasif hasat döngüsü.
async fn recv_loop(shared: Arc<Shared>) {
    let mut buf = [0u8; 2048];
    loop {
        let (len, from) = match shared.socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                // Sayılır: Windows'ta bu hatalar sessizce yutulunca hasat sebebi
                // görünmeden durur (bkz. `disable_conn_reset`).
                shared.stats.recv_errors.fetch_add(1, Ordering::Relaxed);
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
                        // ANNOUNCE EDEN DÜĞÜM O TORRENT'İN PEER'İDİR ve bize paket
                        // gönderebildiğine göre erişilebilirdir — metadata çekimi için
                        // elimize geçen en kaliteli adres budur. Ölçüm: peer
                        // denemelerinin %97'si zaman aşımı (DHT'den gelen bayat/NAT'lı
                        // adresler); announce eden peer ise az önce canlıydı.
                        // BEP-5: implied_port=1 ise gönderenin UDP kaynak portu, değilse
                        // sorgudaki `port` kullanılır.
                        let port = if q.implied_port {
                            Some(from.port())
                        } else {
                            q.announce_port
                        };
                        let peer = port
                            .filter(|p| *p != 0)
                            .map(|p| SocketAddrV4::new(*from.ip(), p));
                        harvest_with_peers(
                            shared,
                            ih,
                            SightingSource::Announce,
                            peer.into_iter().collect(),
                        );
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
            // BEP-42: karşı düğüm bize dış adresimizi bildirdiyse ve henüz uyumlu bir
            // kimliğimiz yoksa, kimliği hemen o IP'den türet. Uyumsuz kimlikli düğümler
            // modern istemcilerin yönlendirme tablosuna alınmaz; alınmayınca da bize
            // announce/get_peers gelmez (ölçüm: gelen announce = 0). Bu yüzden bu, pasif
            // hasadın en kritik ayarıdır.
            if let Some(ip) = r.reported_ip {
                if !ip.is_private() && !ip.is_loopback() && !ip.is_unspecified() {
                    let mut cur = shared.public_ip.lock().unwrap();
                    if *cur != Some(ip) {
                        *cur = Some(ip);
                        drop(cur);
                        // Geri yüklenen kimlik bu IP için HÂLÂ geçerliyse KORUNUR: ağdaki
                        // yönlendirme tablolarında biriken yerimiz ancak kimlik sabit
                        // kalırsa yaşar. Yalnız geçersizse (ör. IP değişmiş) yeniden türetilir.
                        let mut id = shared.our_id.lock().unwrap();
                        let already_valid = Id::from_bytes(*id)
                            .map(|cur| cur.is_valid_for_ip(ip))
                            .unwrap_or(false);
                        if already_valid {
                            info!(%ip, "dış adres öğrenildi; mevcut kimlik BEP-42 uyumlu, korunuyor");
                        } else {
                            *id = *Id::from_ipv4(ip).as_bytes();
                            info!(%ip, "dış adres öğrenildi → BEP-42 uyumlu düğüm kimliği kuruldu");
                        }
                    }
                }
            }
            // YANIT VEREN DÜĞÜMÜ KUYRUĞA GERİ KOY. `pop_nodes` sorguladığı düğümü
            // kuyruktan çıkarır ve eskiden bir daha geri koymazdı; yani KANITLANMIŞ
            // canlı düğümler bir kez kullanılıp atılıyor, yerlerini yanıtlarda gelen
            // kanıtlanmamış (çoğu ölü) adresler alıyordu. Kuyruk böyle kurur ve
            // `crawl_loop` sorgulayacak düğüm bulamaz: ölçümde giden sorgu 50/sn
            // bütçeye karşılık 1,5/sn'ye düşmüş, hasat pratikte durmuştu.
            // Sona eklenir: sıra tekrar gelene kadar kuyruğun tamamı dolaşılır.
            shared.push_nodes(&[from]);
            if !r.nodes.is_empty() {
                let added = shared.push_nodes(&r.nodes);
                shared
                    .stats
                    .nodes_learned
                    .fetch_add(added, Ordering::Relaxed);
            }
            // Bizim get_peers sorgumuzun yanıtı mı? (`values` → taze peer ipuçları)
            if r.txid.len() == 2 {
                let key = u16::from_be_bytes([r.txid[0], r.txid[1]]);
                let hit = shared.pending_gp.lock().unwrap().remove(&key);
                if let Some((ih, _)) = hit {
                    if !r.values.is_empty() {
                        shared.stats.peer_hints.fetch_add(1, Ordering::Relaxed);
                        emit(shared, ih, SightingSource::SamplePeers, r.values.clone(), 0);
                    }
                }
            }
            // BEP-51: yanıttaki infohash örneklerini aktif olarak hasat et. Örneği veren
            // düğüm bu infohash'ler için peer saklıyordur → birkaçı için hemen ona
            // get_peers sor (takip); yanıtı `values` ile döner.
            if !r.samples.is_empty() {
                shared
                    .stats
                    .samples_seen
                    .fetch_add(r.samples.len() as u64, Ordering::Relaxed);
                let mut followups = 0usize;
                for ih in r.samples {
                    let is_new = harvest(shared, ih, SightingSource::Sample);
                    if is_new && followups < shared.followups_per_sample {
                        if shared.limiter.lock().unwrap().try_take() {
                            followups += 1;
                            let txid = shared.next_txid();
                            let key = u16::from_be_bytes(txid);
                            {
                                let mut p = shared.pending_gp.lock().unwrap();
                                if p.len() > 4096 {
                                    let cutoff = Instant::now() - Duration::from_secs(30);
                                    p.retain(|_, (_, t)| *t > cutoff);
                                }
                                p.insert(key, (ih, Instant::now()));
                            }
                            let pkt = krpc::build_get_peers(&txid, &shared.our_id(), &ih);
                            let _ = shared.reply_tx.try_send((pkt, from));
                            shared.stats.queries_sent.fetch_add(1, Ordering::Relaxed);
                        } else {
                            shared.stats.rate_limited.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
        }
        Message::Other => {}
    }
}

/// Bir infohash görülmesini dedup'tan geçirip kanala yayar. Örnekler (Sample) büyük
/// dedup'tan geçer; pasif kaynaklar (sıcak) daha önce örneklenmiş olsa da yayılır — yalnız
/// kısa pencerede tekrarları bastırılır — çünkü "şu anda aranıyor" sinyali değerlidir.
/// Döndürdüğü: dedup'a göre yeni miydi.
fn harvest(shared: &Shared, ih: [u8; ID_LEN], source: SightingSource) -> bool {
    harvest_with_peers(shared, ih, source, Vec::new())
}

/// `harvest` + bilinen peer adresleri (announce edenin adresi gibi). Peer'li sighting
/// tekrarlı olsa bile yayılır: adres tazedir ve çekim için doğrudan kullanılır.
fn harvest_with_peers(
    shared: &Shared,
    ih: [u8; ID_LEN],
    source: SightingSource,
    peers: Vec<SocketAddrV4>,
) -> bool {
    let is_new = if source.is_hot() {
        shared.hot_dedup.lock().unwrap().insert(ih)
    } else {
        shared.dedup.lock().unwrap().insert(ih)
    };
    if !is_new && peers.is_empty() {
        shared.stats.duplicates.fetch_add(1, Ordering::Relaxed);
        if !source.is_hot() {
            let mut m = shared.dup_counts.lock().unwrap();
            if m.len() < 200_000 {
                *m.entry(ih).or_insert(0) += 1;
            }
        }
        return false;
    }
    if !peers.is_empty() {
        shared.stats.peer_hints.fetch_add(1, Ordering::Relaxed);
    }
    emit(shared, ih, source, peers, 0);
    true
}

/// Tekrar sayaçlarını `repeats` sighting'leri olarak akıtır (periyodik görev).
async fn flush_repeats_loop(shared: Arc<Shared>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(15));
    loop {
        ticker.tick().await;
        let drained: Vec<([u8; ID_LEN], u32)> = {
            let mut m = shared.dup_counts.lock().unwrap();
            m.drain().collect()
        };
        for (ih, n) in drained {
            emit(&shared, ih, SightingSource::Sample, Vec::new(), n);
            // Kanal dolarsa emit düşürür (backpressure); tokio'ya nefes ver.
            if shared.sink.capacity() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    }
}

/// Sighting'i kanala yazar (dedup'suz).
fn emit(
    shared: &Shared,
    ih: [u8; ID_LEN],
    source: SightingSource,
    peers: Vec<SocketAddrV4>,
    repeats: u32,
) {
    match shared.sink.try_send(Sighting {
        infohash: InfoHash::from_bytes(ih),
        source,
        peers,
        repeats,
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
    // Kuyruk boşaldığında yeniden tohumlama YALNIZ ara ara denenir. Eskiden her tıkta
    // (100 ms) çağrılıyordu: kuyruk kurumuşsa bu, saniyede 10 kez 4 bootstrap adının DNS
    // çözümlemesi demekti ve `seed_bootstrap` bir `await` olduğu için crawl döngüsünün
    // kendisini DNS'e kilitliyordu — yani kuyruk boşken hasat toparlanmak yerine
    // tamamen duruyordu. (Ayrıca `rebootstrap_loop` zaten 60 sn'de bir aynı işi yapıyor.)
    let mut last_reseed = Instant::now() - RESEED_MIN_INTERVAL;
    loop {
        ticker.tick().await;

        // Kuyruk boşaldıysa bootstrap'tan yeniden tohumla (hız sınırlı).
        if shared.queue_len() == 0 && last_reseed.elapsed() >= RESEED_MIN_INTERVAL {
            last_reseed = Instant::now();
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
                Err(e) => {
                    shared.stats.send_errors.fetch_add(1, Ordering::Relaxed);
                    debug!(error = %e, "crawl sorgusu gönderilemedi");
                }
            }
        }
    }
}

/// Kimliği ve öğrenilen düğümleri periyodik olarak diske yazar.
async fn save_state_loop(shared: Arc<Shared>, path: std::path::PathBuf) {
    let mut ticker = tokio::time::interval(STATE_SAVE_INTERVAL);
    ticker.tick().await; // ilk anlık tık: hemen yazma.
    loop {
        ticker.tick().await;
        let id = shared.our_id();
        let nodes: Vec<SocketAddrV4> = {
            let q = shared.nodes.lock().unwrap();
            q.iter().take(STATE_NODES).copied().collect()
        };
        if let Err(e) = state::save(&path, &id, &nodes) {
            debug!(error = %e, "DHT durumu yazılamadı");
        }
    }
}

/// Düğüm kimliğini periyodik döndürür (kimlik uzayında konum değiştirir).
async fn rotate_loop(shared: Arc<Shared>, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    ticker.tick().await; // ilk anlık tık: hemen döndürme.
    loop {
        ticker.tick().await;
        // Dış IP biliniyorsa kimlik BEP-42 uyumlu türetilir (rastgele bileşen her
        // döndürmede değişir); bilinmiyorsa rastgele.
        let ip = *shared.public_ip.lock().unwrap();
        let new_id = match ip {
            Some(ip) => *Id::from_ipv4(ip).as_bytes(),
            None => *Id::random().as_bytes(),
        };
        *shared.our_id.lock().unwrap() = new_id;
        debug!(bep42 = ip.is_some(), "düğüm kimliği döndürüldü");
    }
}

/// `get_peers` yanıtı için basit, adrese bağlı token üretir (doğrulamıyoruz).
fn token_for(addr: SocketAddrV4) -> [u8; 4] {
    addr.ip().octets()
}

impl StatsSnapshot {
    /// İki sayaç görüntüsünü toplar. Çoklu düğüm kimliği (BEP-42 bir IP için 8 geçerli
    /// kimliğe izin verir) çalıştırıldığında panoda tek bir toplam gösterilir.
    pub fn merge(self, o: Self) -> Self {
        Self {
            received_packets: self.received_packets + o.received_packets,
            queries_seen: self.queries_seen + o.queries_seen,
            get_peers_seen: self.get_peers_seen + o.get_peers_seen,
            announce_seen: self.announce_seen + o.announce_seen,
            responses_seen: self.responses_seen + o.responses_seen,
            nodes_learned: self.nodes_learned + o.nodes_learned,
            unique_infohashes: self.unique_infohashes + o.unique_infohashes,
            duplicates: self.duplicates + o.duplicates,
            dropped_channel_full: self.dropped_channel_full + o.dropped_channel_full,
            queries_sent: self.queries_sent + o.queries_sent,
            samples_seen: self.samples_seen + o.samples_seen,
            rate_limited: self.rate_limited + o.rate_limited,
            peer_hints: self.peer_hints + o.peer_hints,
            send_errors: self.send_errors + o.send_errors,
            recv_errors: self.recv_errors + o.recv_errors,
        }
    }
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

    /// Kimlik ve düğüm önbelleği diskte gidip gelmeli. Bu sessizce bozulursa hasat
    /// yavaşlar ama hiçbir hata görünmez — bu yüzden test edilir.
    #[test]
    fn state_roundtrips_id_and_nodes() {
        let dir = std::env::temp_dir().join("dragnet-dht-state-test");
        std::fs::create_dir_all(&dir).expect("dizin");
        let path = dir.join("state.bin");
        let id = [0x5au8; ID_LEN];
        let nodes = vec![
            "1.2.3.4:6881".parse::<SocketAddrV4>().unwrap(),
            "5.6.7.8:1337".parse::<SocketAddrV4>().unwrap(),
        ];
        state::save(&path, &id, &nodes).expect("yazılmalı");
        let (got_id, got_nodes) = state::load(&path).expect("okunmalı");
        assert_eq!(got_id, id);
        assert_eq!(got_nodes, nodes);

        // Bozuk/yabancı dosya sessizce yok sayılır (sıfırdan başlanır), çökmez.
        std::fs::write(&path, b"bozuk").expect("yaz");
        assert!(state::load(&path).is_none());
        assert!(state::load(&dir.join("yok.bin")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Geri yüklenen kimlik, dış IP için hâlâ BEP-42 uyumluysa korunmalıdır — pasif
    /// hasadın birikimi buna bağlı.
    #[test]
    fn bep42_id_stays_valid_across_restart() {
        let ip = Ipv4Addr::new(159, 146, 35, 97);
        let id = *Id::from_ipv4(ip).as_bytes();
        assert!(
            Id::from_bytes(id).unwrap().is_valid_for_ip(ip),
            "türetilen kimlik kendi IP'si için geçerli olmalı"
        );
        // Başka bir IP'ye taşınırsa geçersizleşir → yeniden türetilmesi beklenir.
        assert!(!Id::from_bytes(id)
            .unwrap()
            .is_valid_for_ip(Ipv4Addr::new(8, 8, 8, 8)));
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
