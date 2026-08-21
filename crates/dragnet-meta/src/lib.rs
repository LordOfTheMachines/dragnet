// SPDX-License-Identifier: AGPL-3.0-only
//! dragnet-meta — Metadata fetcher (Faz 2).
//!
//! Bir infohash alır, DHT'den (`get_peers`) peer bulur, peer'lere bağlanıp
//! BEP-10 extension handshake + BEP-9 `ut_metadata` ile torrent metadata'sını
//! **tracker'sız** çeker, SHA-1 ile doğrular ve bir [`dragnet_core::TorrentRecord`]
//! üretir.
//!
//! Wire protokolü [`wire`] modülündedir; bu modül peer bulma, eşzamanlı deneme ve
//! info sözlüğünü `TorrentRecord`'a çözme işini yapar.

mod error;
pub mod wire;

use std::collections::HashSet;
use std::net::SocketAddrV4;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_lite::StreamExt;
use tracing::debug;

use dragnet_core::{InfoHash, TorrentFile, TorrentRecord};
use mainline::{Dht, Id};

pub mod text;

pub use error::{FetchError, PeerError};

/// Metadata çekim davranışını ayarlar. [`Default`] makul değerler verir.
#[derive(Debug, Clone)]
pub struct FetchConfig {
    /// Canlılık scrape'inde (`count_peers`) peer toplama süresi ve `fetch` için peer
    /// akışını dinleme üst sınırı (genelde `overall_timeout` daha önce biter).
    pub peer_gather_timeout: Duration,
    /// Bir çekimde denenecek azami peer sayısı.
    pub max_peers: usize,
    /// Tek bir peer denemesi için zaman aşımı.
    pub per_peer_timeout: Duration,
    /// Aynı anda denenecek peer sayısı (çekim başına eşzamanlı TCP).
    pub concurrency: usize,
    /// Bir çekimin toplam süre bütçesi (peer bulma + deneme). Faz E ölçümü: başarılı
    /// çekimlerin medyanı ~14 s, kuyruğu 40 s+.
    pub overall_timeout: Duration,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            peer_gather_timeout: Duration::from_secs(20),
            // Nazik varsayılanlar: çekim başına eşzamanlı TCP peer bağlantısını sınırlı
            // tutarak router bağlantı-izleme tablosunu korur; toplam yük çekim işçisi
            // sayısı × concurrency'dir.
            max_peers: 40,
            // Peer başına toplam bütçe (bağlan + handshake + metadata): bağlantı 1,8 sn'de
            // kesildiği için 8 sn gereksiz uzundu.
            per_peer_timeout: Duration::from_secs(8),
            concurrency: 12,
            // Çekim başına toplam bütçe: ortalama çekim 3,1 sn sürüyor; 45 sn yalnız nadir
            // kuyruk durumlarında dolduruluyordu ve işçiyi boşuna tutuyordu.
            overall_timeout: Duration::from_secs(45),
        }
    }
}

/// İpucu adresleri denenirken DHT aramasının bekletileceği süre (F13). İpuçları taze
/// olduğu için çekimlerin çoğu bu pencerede biter ve arama hiç yapılmaz; bitmezse
/// normal yola dönülür. TCP bağlanma bütçesi (3,5 sn) ile aynı mertebede tutuldu.
pub const HINT_GRACE: Duration = Duration::from_secs(3);

/// DHT üzerinden metadata çeken fetcher. İçinde bir mainline DHT istemcisi tutar.
pub struct MetadataFetcher {
    dht: mainline::async_dht::AsyncDht,
    config: FetchConfig,
    stats: Arc<FetchStats>,
    /// uTP soketi (F12): TCP zaman aşımından sonra ikinci yol. `None` = açılamadı,
    /// yalnız TCP kullanılır.
    utp: Option<Arc<librqbit_utp::UtpSocketUdp>>,
}

/// Çekim sayaçları (teşhis/pano): boru hattının gerçek verimini görünür kılar.
#[derive(Debug, Default)]
pub struct FetchStats {
    pub attempts: AtomicU64,
    pub ok: AtomicU64,
    pub no_peers: AtomicU64,
    pub all_peers_failed: AtomicU64,
    /// Toplam çekim süresi (ms) — ortalama için.
    pub total_ms: AtomicU64,
    /// Toplam bulunan peer sayısı.
    pub peers_found: AtomicU64,
    // Peer denemesi sonuçları (neden başarısız? teşhis):
    pub peer_ok: AtomicU64,
    pub peer_io: AtomicU64,
    pub peer_timeout: AtomicU64,
    pub peer_bad_handshake: AtomicU64,
    pub peer_no_metadata_ext: AtomicU64,
    pub peer_other: AtomicU64,
    /// Genel internet adresi olmadığı için hiç denenmeyen peer (F8-3 politikası).
    pub peer_not_public: AtomicU64,
    /// TCP zaman aşımından sonra uTP ile denenip BAŞARILI olan peer sayısı (F12 ölçümü).
    pub peer_utp_ok: AtomicU64,
    /// uTP ile de başarısız olanlar.
    pub peer_utp_fail: AtomicU64,
}

/// Anlık kopya.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct FetchStatsSnapshot {
    pub attempts: u64,
    pub ok: u64,
    pub no_peers: u64,
    pub all_peers_failed: u64,
    pub avg_ms: u64,
    pub avg_peers: f32,
    pub peer_ok: u64,
    pub peer_io: u64,
    pub peer_timeout: u64,
    pub peer_bad_handshake: u64,
    pub peer_no_metadata_ext: u64,
    pub peer_other: u64,
    pub peer_not_public: u64,
    pub peer_utp_ok: u64,
    pub peer_utp_fail: u64,
}

impl FetchStats {
    pub fn snapshot(&self) -> FetchStatsSnapshot {
        let attempts = self.attempts.load(Ordering::Relaxed);
        FetchStatsSnapshot {
            attempts,
            ok: self.ok.load(Ordering::Relaxed),
            no_peers: self.no_peers.load(Ordering::Relaxed),
            all_peers_failed: self.all_peers_failed.load(Ordering::Relaxed),
            avg_ms: self
                .total_ms
                .load(Ordering::Relaxed)
                .checked_div(attempts)
                .unwrap_or(0),
            avg_peers: if attempts > 0 {
                self.peers_found.load(Ordering::Relaxed) as f32 / attempts as f32
            } else {
                0.0
            },
            peer_ok: self.peer_ok.load(Ordering::Relaxed),
            peer_io: self.peer_io.load(Ordering::Relaxed),
            peer_timeout: self.peer_timeout.load(Ordering::Relaxed),
            peer_bad_handshake: self.peer_bad_handshake.load(Ordering::Relaxed),
            peer_no_metadata_ext: self.peer_no_metadata_ext.load(Ordering::Relaxed),
            peer_other: self.peer_other.load(Ordering::Relaxed),
            peer_not_public: self.peer_not_public.load(Ordering::Relaxed),
            peer_utp_ok: self.peer_utp_ok.load(Ordering::Relaxed),
            peer_utp_fail: self.peer_utp_fail.load(Ordering::Relaxed),
        }
    }
}

impl MetadataFetcher {
    /// Yeni bir fetcher oluşturur (kendi DHT istemci düğümünü açar).
    pub fn new(config: FetchConfig) -> std::io::Result<Self> {
        // Bootstrap düğümleri normalde alan adıyla çözülür; DNS'in kapalı olduğu
        // ortamlarda (ölçüm araçları, kısıtlı kabuklar) `DRAGNET_BOOTSTRAP` ortam
        // değişkeniyle doğrudan IP:port listesi verilebilir.
        // uTP soketi (F12): TCP zaman aşımından sonraki yedek yol. Açılamazsa yalnız TCP
        // kullanılır — uTP isteğe bağlı bir iyileştirmedir, zorunlu değil.
        let dht = match std::env::var("DRAGNET_BOOTSTRAP") {
            Ok(list) if !list.trim().is_empty() => {
                let nodes: Vec<String> = list.split(',').map(|s| s.trim().to_string()).collect();
                mainline::Dht::builder()
                    .bootstrap(&nodes)
                    .build()?
                    .as_async()
            }
            _ => Dht::client()?.as_async(),
        };
        Ok(Self {
            dht,
            config,
            stats: Arc::new(FetchStats::default()),
            utp: None,
        })
    }

    /// uTP soketini açar (F12). Başarısız olursa fetcher yalnız TCP ile çalışır.
    pub async fn enable_utp(&mut self) -> bool {
        // Varsayılan KAPALI (bkz. ölçüm notu `fetch` içinde); DRAGNET_UTP=1 ile açılır.
        if std::env::var("DRAGNET_UTP").unwrap_or_default() != "1" {
            return false;
        }
        match librqbit_utp::UtpSocket::new_udp("0.0.0.0:0".parse().expect("adres")).await {
            Ok(s) => {
                self.utp = Some(s);
                true
            }
            Err(e) => {
                debug!(error = %e, "uTP soketi açılamadı; yalnız TCP");
                false
            }
        }
    }

    /// Sayaçlar (paylaşımlı).
    pub fn stats(&self) -> Arc<FetchStats> {
        Arc::clone(&self.stats)
    }

    /// DHT istemcisinin bootstrap'ını bekler (yeni açılmış istemcide `get_peers`,
    /// yönlendirme tablosu dolmadan boş döner). `true` = başarılı.
    pub async fn wait_bootstrapped(&self) -> bool {
        self.dht.bootstrapped().await
    }

    /// DHT istemcisi: (güvenlik duvarı arkasında mı, dış adres, yerel port). Erişilebilirlik
    /// göstergesi için; `firewalled` mainline'ın gelen sorgu gözlemine dayanır.
    pub async fn dht_reachability(&self) -> (bool, Option<String>, u16) {
        let i = self.dht.info().await;
        (
            i.firewalled(),
            i.public_address().map(|a| a.to_string()),
            i.local_addr().port(),
        )
    }

    /// DHT istemci durumu (teşhis): yerel adres, güvenlik duvarı, tahmini ağ boyutu.
    pub async fn dht_info(&self) -> String {
        let i = self.dht.info().await;
        format!(
            "local={} public={:?} firewalled={} server_mode={} dht_size≈{}",
            i.local_addr(),
            i.public_address(),
            i.firewalled(),
            i.server_mode(),
            i.dht_size_estimate().0
        )
    }

    /// Bir infohash için metadata çeker ve `TorrentRecord` döner.
    ///
    /// **Boru hattı:** DHT `get_peers` akışı sürerken gelen her yeni peer'e hemen bağlanılır
    /// (sınırlı eşzamanlılık); ilk başarılı metadata kazanır ve kalanlar iptal edilir. Tek
    /// bir toplam süre bütçesi vardır (`overall_timeout`). Faz E ölçümü: önce 20 s peer
    /// biriktirip sonra denemek başarılı çekimleri (medyan ~14 s, kuyruk 40 s+) kesiyor ve
    /// başarısızları ~70 s sürüklüyordu.
    pub async fn fetch(&self, infohash: InfoHash) -> Result<TorrentRecord, FetchError> {
        self.fetch_with_hints(infohash, &[]).await
    }

    /// [`MetadataFetcher::fetch`] + bilinen peer ipuçları: ipuçları DHT aramasını
    /// beklemeden **hemen** denenir (BEP-51 takip `get_peers`'ten gelen taze adresler).
    pub async fn fetch_with_hints(
        &self,
        infohash: InfoHash,
        hints: &[SocketAddrV4],
    ) -> Result<TorrentRecord, FetchError> {
        let started = Instant::now();
        let res = self.fetch_inner(infohash, hints).await;
        self.stats.attempts.fetch_add(1, Ordering::Relaxed);
        self.stats
            .total_ms
            .fetch_add(started.elapsed().as_millis() as u64, Ordering::Relaxed);
        match &res {
            Ok(_) => {
                self.stats.ok.fetch_add(1, Ordering::Relaxed);
            }
            Err(FetchError::NoPeers) => {
                self.stats.no_peers.fetch_add(1, Ordering::Relaxed);
            }
            Err(FetchError::AllPeersFailed { .. }) => {
                self.stats.all_peers_failed.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {}
        }
        res
    }

    async fn fetch_inner(
        &self,
        infohash: InfoHash,
        hints: &[SocketAddrV4],
    ) -> Result<TorrentRecord, FetchError> {
        let ih_bytes = *infohash.as_bytes();
        let per_peer = self.config.per_peer_timeout;
        let conc = self.config.concurrency.max(1);
        let max_tries = self.config.max_peers.max(1);
        let deadline = Instant::now() + self.config.overall_timeout;

        let id = Id::from_bytes(infohash.as_bytes()).expect("infohash 20 bayttır");
        // DHT ARAMASINI GECİKTİR (F13). İpucu adresleri varsa (triyajdan ya da BEP-51
        // takip `get_peers`'ten gelen taze adresler) önce onlar denenir; arama ancak
        // ipuçları tükenirse ya da `HINT_GRACE` dolarsa başlatılır.
        //
        // Gerekçe ölçümle: bir `get_peers` araması medyan 2,7 sn sürüyor ve ~50 giden UDP
        // sorgusu harcıyor. Aynı adresler triyaj sırasında zaten bulunmuştu; aramayı
        // tekrarlamak hem bu süreyi ikinci kez ödemek hem de DHT bütçesini — asıl aday
        // arzını üreten triyajdan — çalmak demekti.
        let mut stream = hints.is_empty().then(|| self.dht.get_peers(id));
        let dht_at = Instant::now() + HINT_GRACE;
        let mut stream_done = false;
        let mut seen: HashSet<SocketAddrV4> = HashSet::new();
        let mut queue: std::collections::VecDeque<SocketAddrV4> = std::collections::VecDeque::new();
        for &h in hints {
            if seen.insert(h) {
                queue.push_back(h);
            }
        }
        let mut set = tokio::task::JoinSet::new();
        let mut tried = 0usize;
        let mut launched = 0usize;

        loop {
            // Kuyruktan boş yuvalara peer denemesi başlat.
            while set.len() < conc && launched < max_tries {
                let Some(addr) = queue.pop_front() else { break };
                launched += 1;
                let utp = self.utp.clone();
                let stats = Arc::clone(&self.stats);
                set.spawn(async move {
                    // ÖNCE TCP. Zaman aşımına uğrarsa (ölçüm: denemelerin %97'si böyle)
                    // aynı peer **uTP** (BEP-29) ile denenir: modern istemcilerin çoğu
                    // uTP'yi tercih eder ve NAT arkasındaki peer'ler pratikte yalnız uTP
                    // ile erişilebilir olur. Kazanç sayaçlarla ölçülür (peer_utp_ok).
                    let first = wire::fetch_info_from_peer(addr, ih_bytes, per_peer).await;
                    match (first, utp) {
                        (Err(PeerError::Timeout), Some(sock)) => {
                            let r = wire::fetch_info_from_peer_utp(&sock, addr, ih_bytes, per_peer)
                                .await;
                            if r.is_ok() {
                                stats.peer_utp_ok.fetch_add(1, Ordering::Relaxed);
                            } else {
                                stats.peer_utp_fail.fetch_add(1, Ordering::Relaxed);
                            }
                            r
                        }
                        (other, _) => other,
                    }
                });
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            // İpuçları tükendiyse ya da nezaket süresi dolduysa DHT aramasını devreye al.
            if stream.is_none() && (now >= dht_at || (queue.is_empty() && set.is_empty())) {
                stream = Some(self.dht.get_peers(id));
            }
            let all_done = stream.is_some()
                && stream_done
                && set.is_empty()
                && (queue.is_empty() || launched >= max_tries);
            if all_done {
                break;
            }
            let remaining = deadline - now;
            // DHT araması henüz başlamadıysa, başlatma anında uyanmak için kısa bekle.
            let wake = match stream {
                Some(_) => remaining,
                None => remaining.min(dht_at.saturating_duration_since(now)),
            };
            tokio::select! {
                // Yeni peer partisi (akış başladıysa ve bitmediyse).
                batch = async { stream.as_mut().expect("akış var").next().await },
                    if stream.is_some() && !stream_done => match batch {
                    Some(batch) => {
                        for p in batch {
                            if seen.insert(p) {
                                queue.push_back(p);
                            }
                        }
                    }
                    None => stream_done = true,
                },
                // Bir peer denemesi bitti.
                res = set.join_next(), if !set.is_empty() => {
                    tried += 1;
                    match res {
                        Some(Ok(Ok(info_bytes))) => match parse_info_dict(&info_bytes, infohash) {
                            Ok(record) => {
                                self.stats.peer_ok.fetch_add(1, Ordering::Relaxed);
                                // `attempts` her çekimde arttığı için `peers_found` da
                                // BAŞARI yolunda sayılmalı; yoksa `avg_peers` yalnız
                                // başarısızlıkların (yani peer'i az olanların) ortalamasını
                                // gösterir ve boru hattı olduğundan kötü görünür.
                                self.stats
                                    .peers_found
                                    .fetch_add(seen.len() as u64, Ordering::Relaxed);
                                return Ok(record); // set drop → kalanlar iptal
                            }
                            Err(e) => debug!(error = %e, "info sözlüğü çözülemedi"),
                        },
                        Some(Ok(Err(e))) => {
                            let c = match &e {
                                PeerError::Io(_) => &self.stats.peer_io,
                                PeerError::Timeout => &self.stats.peer_timeout,
                                PeerError::BadHandshake | PeerError::InfoHashMismatch => &self.stats.peer_bad_handshake,
                                PeerError::NoExtension | PeerError::NoUtMetadata => {
                                    &self.stats.peer_no_metadata_ext
                                }
                                PeerError::NotPublic => &self.stats.peer_not_public,
                                _ => &self.stats.peer_other,
                            };
                            c.fetch_add(1, Ordering::Relaxed);
                            debug!(error = %e, "peer denemesi başarısız");
                        }
                        _ => {}
                    }
                }
                // İki iş görür: bütçe dolduysa çıkış, DHT'yi devreye almak için uyanış.
                _ = tokio::time::sleep(wake) => {
                    if stream.is_some() || wake == remaining {
                        break;
                    }
                }
            }
        }
        self.stats
            .peers_found
            .fetch_add(seen.len() as u64, Ordering::Relaxed);
        if seen.is_empty() {
            Err(FetchError::NoPeers)
        } else {
            debug!(infohash = %infohash, peers = seen.len(), tried, "metadata çekilemedi");
            Err(FetchError::AllPeersFailed { tried })
        }
    }

    /// DHT `get_peers` akışını verilen süre/sayı sınırına kadar benzersiz peer'lere
    /// boşaltır. Hem metadata için peer bulma hem canlılık scrape'i bunu kullanır.
    async fn drain_peers(
        &self,
        infohash: InfoHash,
        deadline: Instant,
        max: usize,
    ) -> HashSet<SocketAddrV4> {
        let id = Id::from_bytes(infohash.as_bytes()).expect("infohash 20 bayttır");
        let mut stream = self.dht.get_peers(id);
        let mut seen = HashSet::new();
        loop {
            let now = Instant::now();
            if now >= deadline || seen.len() >= max {
                break;
            }
            match tokio::time::timeout(deadline - now, stream.next()).await {
                Ok(Some(batch)) => seen.extend(batch),
                Ok(None) | Err(_) => break, // sorgu bitti ya da zaman aşımı
            }
        }
        seen
    }

    /// Canlılık scrape'i: bir infohash için DHT'de `get_peers` yapıp benzersiz
    /// peer sayısını döner (canlı seeder/leecher vekili). Metadata çekmez.
    pub async fn count_peers(&self, infohash: InfoHash, timeout: Duration) -> usize {
        let deadline = Instant::now() + timeout;
        self.drain_peers(infohash, deadline, usize::MAX).await.len()
    }

    /// DHT'den peer adreslerini toplar (ölçüm araçları için; üretim yolu `fetch` içinde
    /// akışı boru hattıyla tüketir).
    pub async fn peers_of(
        &self,
        infohash: InfoHash,
        timeout: Duration,
        max: usize,
    ) -> Vec<std::net::SocketAddrV4> {
        let deadline = Instant::now() + timeout;
        self.drain_peers(infohash, deadline, max)
            .await
            .into_iter()
            .collect()
    }
}

/// Doğrulanmış ham info sözlüğü baytlarını `TorrentRecord`'a çözer.
///
/// BEP-3 info sözlüğü: `name`, ve ya `length` (tek dosya) ya da `files` (çok dosya).
pub fn parse_info_dict(info_bytes: &[u8], infohash: InfoHash) -> Result<TorrentRecord, PeerError> {
    use serde_bencode::value::Value;

    // Güvenilmeyen: saldırgan infohash=SHA1(M) seçip derin iç içe M sunabilir; SHA-1
    // doğrulaması geçse bile serde'ye vermeden ÖNCE derinlik/sınır doğrula (stack overflow
    // → süreç abort önlenir). Metadata tek bir info sözlüğüdür → tüm tamponu kaplamalı.
    if dragnet_core::bencode_value_len(info_bytes) != Some(info_bytes.len()) {
        return Err(PeerError::Bencode);
    }
    let value: Value = serde_bencode::from_bytes(info_bytes).map_err(|_| PeerError::Bencode)?;
    let Value::Dict(dict) = value else {
        return Err(PeerError::BadInfoDict("info bir sözlük değil"));
    };

    // Kodlama: BEP-3 UTF-8 ister ama eski/bölgesel torrent'lerde GBK/Shift-JIS/CP1251
    // yaygındır. `from_utf8_lossy` bunları `�` yapar; ad okunmaz olur, kayıt `garbled`
    // işaretlenip boşuna yeniden çekilir (yeniden çekmek kodlamayı düzeltmez). Bu yüzden
    // `text::get_text` kullanılır: önce `name.utf-8`, sonra kodlama tespiti (bkz. text.rs).
    let Some(name) = text::get_text(&dict, "name") else {
        return Err(PeerError::BadInfoDict("name"));
    };

    let (files, total_size) = if let Some(Value::List(list)) = dict.get(b"files".as_ref()) {
        // Çok dosyalı: her giriş {length, path:[bileşenler]}.
        let mut files = Vec::with_capacity(list.len());
        let mut total = 0u64;
        for entry in list {
            let Value::Dict(fd) = entry else {
                return Err(PeerError::BadInfoDict("files girişi sözlük değil"));
            };
            let size = match fd.get(b"length".as_ref()) {
                Some(Value::Int(n)) if *n >= 0 => *n as u64,
                _ => return Err(PeerError::BadInfoDict("files.length")),
            };
            let mut parts = vec![name.clone()];
            // `path.utf-8` (yaygın uzantı) varsa önceliklidir; yoksa `path` bileşenleri
            // ad ile aynı kodlama tespitinden geçer.
            let comps = match fd
                .get(b"path.utf-8".as_ref())
                .or_else(|| fd.get(b"path".as_ref()))
            {
                Some(Value::List(comps)) => comps,
                _ => return Err(PeerError::BadInfoDict("files.path")),
            };
            for c in comps {
                if let Value::Bytes(b) = c {
                    parts.push(text::decode_bytes(b));
                }
            }
            total = total.saturating_add(size);
            files.push(TorrentFile {
                path: parts.join("/"),
                size,
            });
        }
        (files, total)
    } else if let Some(Value::Int(n)) = dict.get(b"length".as_ref()) {
        // Tek dosyalı.
        if *n < 0 {
            return Err(PeerError::BadInfoDict("length"));
        }
        let size = *n as u64;
        (
            vec![TorrentFile {
                path: name.clone(),
                size,
            }],
            size,
        )
    } else {
        return Err(PeerError::BadInfoDict("length/files ikisi de yok"));
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    Ok(TorrentRecord {
        infohash,
        name,
        total_size,
        files,
        first_seen: now,
        last_seen: now,
        seen_count: 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bilinen içeriğe göre bir info sözlüğü kurar ve gerçek infohash'ini hesaplar.
    fn build_single_file_info() -> (Vec<u8>, InfoHash) {
        // d6:lengthi1024e4:name8:test.isoe
        let info = b"d6:lengthi1024e4:name8:test.isoe".to_vec();
        let digest = sha1_smol::Sha1::from(&info).digest().bytes();
        (info, InfoHash::from_bytes(digest))
    }

    #[test]
    fn parses_single_file_info() {
        let (info, ih) = build_single_file_info();
        let rec = parse_info_dict(&info, ih).expect("çözülmeli");
        assert_eq!(rec.name, "test.iso");
        assert_eq!(rec.total_size, 1024);
        assert_eq!(rec.files.len(), 1);
        assert_eq!(rec.files[0].path, "test.iso");
        assert_eq!(rec.files[0].size, 1024);
        assert_eq!(rec.seen_count, 1);
    }

    #[test]
    fn parses_multi_file_info() {
        // name=pack, files: [{length:10, path:[a.txt]}, {length:20, path:[sub, b.txt]}]
        let info =
            b"d5:filesld6:lengthi10e4:pathl5:a.txteed6:lengthi20e4:pathl3:sub5:b.txteee4:name4:packe"
                .to_vec();
        let ih = InfoHash::from_bytes([0u8; 20]);
        let rec = parse_info_dict(&info, ih).expect("çözülmeli");
        assert_eq!(rec.name, "pack");
        assert_eq!(rec.total_size, 30);
        assert_eq!(rec.files.len(), 2);
        assert_eq!(rec.files[0].path, "pack/a.txt");
        assert_eq!(rec.files[1].path, "pack/sub/b.txt");
    }

    /// UTF-8 OLMAYAN ad ve dosya yolları doğru kodlamayla çözülmeli. Bu yol bir kez
    /// regresyona uğradı: `text` modülü yazılmış ama `parse_info_dict` `from_utf8_lossy`
    /// kullanmaya devam etmişti — GBK/Shift-JIS adlar `���` olarak indeksleniyor,
    /// `garbled` işaretlenip boşuna yeniden çekiliyordu (yeniden çekmek kodlamayı düzeltmez).
    #[test]
    fn decodes_legacy_encodings_in_name_and_paths() {
        use serde_bencode::value::Value;
        // GBK kodlu ad ("电影") + GBK kodlu dosya yolu bileşeni.
        let (gbk_name, _, _) = encoding_rs::GB18030.encode("电影 高清版");
        let (gbk_path, _, _) = encoding_rs::GB18030.encode("第01集.mkv");
        let mut file = std::collections::HashMap::new();
        file.insert(b"length".to_vec(), Value::Int(10));
        file.insert(
            b"path".to_vec(),
            Value::List(vec![Value::Bytes(gbk_path.into_owned())]),
        );
        let mut dict = std::collections::HashMap::new();
        dict.insert(b"name".to_vec(), Value::Bytes(gbk_name.into_owned()));
        dict.insert(b"files".to_vec(), Value::List(vec![Value::Dict(file)]));
        let info = serde_bencode::to_bytes(&Value::Dict(dict)).expect("bencode");

        let rec = parse_info_dict(&info, InfoHash::from_bytes([0u8; 20])).expect("çözülmeli");
        assert_eq!(rec.name, "电影 高清版");
        assert_eq!(rec.files[0].path, "电影 高清版/第01集.mkv");
        assert!(!rec.name.contains('\u{FFFD}'), "� kalmamalı");
    }

    /// `name.utf-8` varsa ona öncelik verilir (istemcilerin yaygın uzantısı).
    #[test]
    fn prefers_utf8_variant_of_name() {
        use serde_bencode::value::Value;
        let (gbk, _, _) = encoding_rs::GB18030.encode("电影");
        let mut dict = std::collections::HashMap::new();
        dict.insert(b"name".to_vec(), Value::Bytes(gbk.into_owned()));
        dict.insert(
            b"name.utf-8".to_vec(),
            Value::Bytes("Movie (utf8)".as_bytes().to_vec()),
        );
        dict.insert(b"length".to_vec(), Value::Int(5));
        let info = serde_bencode::to_bytes(&Value::Dict(dict)).expect("bencode");
        let rec = parse_info_dict(&info, InfoHash::from_bytes([0u8; 20])).expect("çözülmeli");
        assert_eq!(rec.name, "Movie (utf8)");
    }

    #[test]
    fn rejects_missing_name() {
        let info = b"d6:lengthi1024ee".to_vec();
        let ih = InfoHash::from_bytes([0u8; 20]);
        assert!(matches!(
            parse_info_dict(&info, ih),
            Err(PeerError::BadInfoDict("name"))
        ));
    }

    #[test]
    fn default_config_is_sane() {
        let c = FetchConfig::default();
        assert!(c.max_peers > 0);
        assert!(c.concurrency > 0);
    }
}
