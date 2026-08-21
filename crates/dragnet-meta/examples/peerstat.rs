// SPDX-License-Identifier: AGPL-3.0-only
//! Teşhis: **peer hunisi**. Bir peer denemesi tam olarak hangi adımda ölüyor?
//!
//! Üretim sayaçları (`FetchStats`) TCP bağlanamadı ile bağlandı-ama-handshake-vermedi
//! durumlarının ikisini de `Timeout` sayıyor. Ayrım kritiktir:
//! - TCP bağlanmıyorsa → adres bayat / NAT arkasında / **bizim çıkışımız boğulmuş**.
//! - TCP bağlanıp handshake gelmiyorsa → karşı taraf şifreli bağlantı (MSE) bekliyor.
//! - Handshake gelip extension yoksa → eski istemci, metadata veremez.
//!
//! Kullanım:
//!   peerstat live <conc> <infohash...>  — verilen infohash'ler (bilinen-canlı test için)
//!   peerstat db <db yolu> <n> <conc>    — depodan sağlıklı (triyajdan geçmiş) adaylar
use std::net::SocketAddrV4;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dragnet_core::InfoHash;
use dragnet_meta::{wire, FetchConfig, MetadataFetcher};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Huninin her basamağı ayrı sayılır (üretimdeki tek `Timeout` kovasının açılımı).
#[derive(Default, Debug)]
struct Funnel {
    peers: AtomicU64,
    not_public: AtomicU64,
    connect_timeout: AtomicU64,
    connect_refused: AtomicU64,
    connected: AtomicU64,
    /// Bağlandı ama 68 baytlık handshake yanıtı gelmedi (şifreli bağlantı beklentisi?).
    hs_timeout: AtomicU64,
    /// Bağlantı handshake sırasında koptu (RST/EOF) — genelde "düz protokolü reddettim".
    hs_closed: AtomicU64,
    hs_ok: AtomicU64,
    no_ext: AtomicU64,
    meta_ok: AtomicU64,
    meta_fail: AtomicU64,
    connect_ms: AtomicU64,
    hs_ms: AtomicU64,
}

/// Bağlanma ve handshake için ayrı, ölçülebilir zaman aşımları (varsayılan tarama dışı).
const CONNECT_TO: Duration = Duration::from_millis(3500);
const HS_TO: Duration = Duration::from_millis(4500);

/// Tek peer'i adım adım dener ve her adımı ayrı sayar.
async fn probe_peer(addr: SocketAddrV4, ih: [u8; 20], f: &Funnel) {
    probe_peer_with(addr, ih, f, CONNECT_TO, HS_TO).await
}

/// [`probe_peer`] — zaman aşımları dışarıdan verilir (tarama modu için).
async fn probe_peer_with(
    addr: SocketAddrV4,
    ih: [u8; 20],
    f: &Funnel,
    connect_to: Duration,
    hs_to: Duration,
) {
    f.peers.fetch_add(1, Ordering::Relaxed);
    if !wire::is_public_peer(&addr) {
        f.not_public.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let t = Instant::now();
    let stream = match tokio::time::timeout(connect_to, TcpStream::connect(addr)).await {
        Ok(Ok(s)) => s,
        Ok(Err(_)) => {
            f.connect_refused.fetch_add(1, Ordering::Relaxed);
            return;
        }
        Err(_) => {
            f.connect_timeout.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    f.connected.fetch_add(1, Ordering::Relaxed);
    f.connect_ms
        .fetch_add(t.elapsed().as_millis() as u64, Ordering::Relaxed);

    // BEP-3 handshake'i elle yolla; yalnız YANIT gelip gelmediğini ölç.
    let mut s = stream;
    let mut hs = [0u8; 68];
    hs[0] = 19;
    hs[1..20].copy_from_slice(b"BitTorrent protocol");
    hs[25] = 0x10;
    hs[28..48].copy_from_slice(&ih);
    hs[48..56].copy_from_slice(b"-DN0001-");
    if s.write_all(&hs).await.is_err() {
        f.hs_closed.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let t2 = Instant::now();
    let mut resp = [0u8; 68];
    match tokio::time::timeout(hs_to, s.read_exact(&mut resp)).await {
        Ok(Ok(_)) => {}
        Ok(Err(_)) => {
            f.hs_closed.fetch_add(1, Ordering::Relaxed);
            return;
        }
        Err(_) => {
            f.hs_timeout.fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
    f.hs_ms
        .fetch_add(t2.elapsed().as_millis() as u64, Ordering::Relaxed);
    if resp[0] != 19 || &resp[1..20] != b"BitTorrent protocol" {
        f.hs_closed.fetch_add(1, Ordering::Relaxed);
        return;
    }
    f.hs_ok.fetch_add(1, Ordering::Relaxed);
    if resp[25] & 0x10 == 0 {
        f.no_ext.fetch_add(1, Ordering::Relaxed);
        return;
    }
    // Buraya kadar geldiyse gerçek çekimi dene (kalan bütçeyle).
    drop(s);
    match wire::fetch_info_from_peer(addr, ih, Duration::from_secs(10)).await {
        Ok(_) => f.meta_ok.fetch_add(1, Ordering::Relaxed),
        Err(_) => f.meta_fail.fetch_add(1, Ordering::Relaxed),
    };
}

/// Bir (eşzamanlılık, zaman aşımı) ayarını verilen peer dilimiyle ölçer.
async fn run_setting(
    peers: &[(SocketAddrV4, [u8; 20])],
    conc: usize,
    connect_to: Duration,
    hs_to: Duration,
) -> (Funnel, Duration) {
    let funnel = Arc::new(Funnel::default());
    let sem = Arc::new(tokio::sync::Semaphore::new(conc));
    let t0 = Instant::now();
    let mut set = tokio::task::JoinSet::new();
    for (addr, ih) in peers.iter().copied() {
        let f = Arc::clone(&funnel);
        let s = Arc::clone(&sem);
        set.spawn(async move {
            let _g = s.acquire().await.unwrap();
            probe_peer_with(addr, ih, &f, connect_to, hs_to).await;
        });
    }
    while set.join_next().await.is_some() {}
    let elapsed = t0.elapsed();
    (Arc::try_unwrap(funnel).unwrap_or_default(), elapsed)
}

/// TARAMA: aynı peer havuzunu farklı ayarlarla dener.
///
/// Neden gerekli: üretimde peer denemelerinin %99,7'si zaman aşımına uğrarken aynı
/// adaylarda 32 eşzamanlılıkla yapılan ölçüm %10,5 bağlanma buluyordu. Tek değişken
/// eşzamanlılıktı (üretimde `fetch_workers × fetch_peer_concurrency` = 384 eşzamanlı
/// giden TCP). Bu, ev modeminin bağlantı-izleme tablosunu taşırıp KENDİ SYN
/// paketlerimizi düşürmesi anlamına gelir — yani hızlanmak için açtığımız eşzamanlılık,
/// çekimi tamamen öldürüyor olabilir. Havuz dilimlere bölünür ki her ayar TAZE peer
/// görsün (aynı peer'i tekrar denemek sonucu bozar).
async fn sweep(peers: Vec<(SocketAddrV4, [u8; 20])>) {
    // (etiket, eşzamanlılık, bağlanma zaman aşımı)
    let settings: Vec<(String, usize, Duration)> = vec![
        ("conc=8   to=3.5s".into(), 8, Duration::from_millis(3500)),
        ("conc=32  to=3.5s".into(), 32, Duration::from_millis(3500)),
        ("conc=96  to=3.5s".into(), 96, Duration::from_millis(3500)),
        ("conc=384 to=3.5s".into(), 384, Duration::from_millis(3500)),
        ("conc=32  to=10s".into(), 32, Duration::from_secs(10)),
        ("conc=32  to=20s".into(), 32, Duration::from_secs(20)),
    ];
    // DİLİMLEME ADİL OLMALI. Havuz infohash sırasına göre dolduğu için ardışık dilimler
    // FARKLI torrent kümelerine denk gelir; torrentlerin canlılığı çok değiştiğinden bu,
    // ayarın etkisini torrent şansıyla karıştırır (ilk denemede `to=10s` %5,0 verirken
    // `to=20s` %15,9 verdi — fark ayardan değil, dilimden geliyordu). Bunun yerine her
    // ayar `i`, `i+N`, `i+2N`, … peer'lerini alır: tüm torrentlerden eşit örnek.
    let n_set = settings.len().max(1);
    let slice_len = peers.len() / n_set;
    if slice_len < 20 {
        println!("UYARI: ayar başına yalnız {slice_len} peer düşüyor; sonuç gürültülü olacak.");
    }
    println!(
        "\n=== TARAMA: {} peer, ayar başına ~{slice_len} (her ayar tüm torrentlerden örnek alır) ===",
        peers.len()
    );
    println!(
        "{:<18} {:>7} {:>9} {:>9} {:>9} {:>8}",
        "AYAR", "peer", "bağlandı", "handshake", "metadata", "süre"
    );
    for (i, (label, conc, to)) in settings.iter().enumerate() {
        let slice: Vec<(SocketAddrV4, [u8; 20])> =
            peers.iter().skip(i).step_by(n_set).copied().collect();
        if slice.is_empty() {
            continue;
        }
        // Ayarlar arasında modemin bağlantı tablosunun boşalması için nefes payı;
        // yoksa bir önceki ayarın açtığı girdiler sonrakini cezalandırır.
        tokio::time::sleep(Duration::from_secs(10)).await;
        let (f, dt) = run_setting(&slice, *conc, *to, Duration::from_secs(5)).await;
        let g = |x: &AtomicU64| x.load(Ordering::Relaxed);
        let n = g(&f.peers).max(1);
        println!(
            "{label:<18} {:>7} {:>8.1}% {:>8.1}% {:>8.1}% {:>7.0}s",
            n,
            100.0 * g(&f.connected) as f64 / n as f64,
            100.0 * g(&f.hs_ok) as f64 / n as f64,
            100.0 * g(&f.meta_ok) as f64 / n as f64,
            dt.as_secs_f64()
        );
    }
    println!("\nYORUM: 'bağlandı' oranı eşzamanlılık arttıkça DÜŞÜYORSA, giden SYN'ler");
    println!("modem/ISS tarafında düşüyordur — çözüm eşzamanlılığı azaltmaktır, zaman");
    println!("aşımını uzatmak değil. Zaman aşımını uzatmak yalnız 'bağlandı' oranı");
    println!("uzun sürede ARTIYORSA işe yarar (peer'ler yavaş ama canlı demektir).");
}

/// EŞZAMANLI DHT ARAMASI taraması: aynı anda kaç `get_peers` araması yapılırsa arama
/// başına kaç peer bulunuyor?
///
/// Neden kritik: üretim çekim başına 2,5 peer görüyor, tek başına yapılan ölçüm aynı
/// adaylarda 25,6 peer buluyor — 10 kat fark. Üretimde aynı anda ~48 arama var
/// (triyaj + çekim) ve hepsi TEK bir `mainline` istemcisini paylaşıyor; o istemcinin
/// aktör döngüsü her turda yalnız bir mesaj işliyor. Aramalar birbirini aç bırakıyorsa
/// eşzamanlılığı artırmak toplam verimi DÜŞÜRÜR: daha çok arama, her biri daha eksik.
async fn lookup_sweep(fetcher: Arc<MetadataFetcher>, hashes: &[InfoHash]) {
    println!("\n=== EŞZAMANLI DHT ARAMASI TARAMASI ===");
    println!(
        "{:<10} {:>8} {:>12} {:>12} {:>10}",
        "eşzaman", "arama", "peer/arama", "peer/sn", "süre"
    );
    for conc in [1usize, 4, 8, 16, 32, 48] {
        // Her tur için taze bir dilim: aynı infohash'i tekrar aramak önbelleğe düşer.
        let slice: Vec<InfoHash> = hashes.iter().copied().take(conc.max(8)).collect();
        if slice.is_empty() {
            continue;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
        let t0 = Instant::now();
        let sem = Arc::new(tokio::sync::Semaphore::new(conc));
        let total = Arc::new(AtomicU64::new(0));
        let mut set = tokio::task::JoinSet::new();
        for ih in slice.iter().copied() {
            let s = Arc::clone(&sem);
            let tot = Arc::clone(&total);
            let f = Arc::clone(&fetcher);
            set.spawn(async move {
                let _g = s.acquire().await.unwrap();
                let peers = f.peers_of(ih, Duration::from_secs(6), 200).await;
                tot.fetch_add(peers.len() as u64, Ordering::Relaxed);
            });
        }
        while set.join_next().await.is_some() {}
        let dt = t0.elapsed();
        let n = slice.len() as f64;
        let tot = total.load(Ordering::Relaxed) as f64;
        println!(
            "{conc:<10} {:>8} {:>12.1} {:>12.1} {:>9.0}s",
            slice.len(),
            tot / n,
            tot / dt.as_secs_f64(),
            dt.as_secs_f64()
        );
    }
    println!("\nYORUM: 'peer/arama' eşzamanlılıkla DÜŞÜYORSA aramalar birbirini aç");
    println!("bırakıyordur (tek mainline istemcisi darboğaz). O zaman doğru ayar,");
    println!("'peer/sn' sütununun tepe yaptığı noktadır — daha fazlası verimi düşürür.");
}

/// ADAY SIRALAMASI testi: çekim kuyruğu adayları hangi sıraya göre seçmeli?
///
/// Üretim `probe_peers DESC` (en çok peer'i ölçülmüş) kullanıyor ve çekim başına 2,5
/// peer görüyor; aynı depoda `probe_at DESC` (en TAZE ölçüm) ile seçilen adaylarda
/// bağımsız ölçüm 25,6 peer buldu. Şüphe: yüksek `probe_peers` değerleri ESKİ
/// ölçümlerden geliyor ve o torrentler çoktan ölmüş; üstelik hep aynı eski adaylar
/// seçildiği için taze adaylar hiç sıra alamıyor (açlık). Bu test iki sıralamayı aynı
/// anda, aynı ağ koşulunda ölçer.
async fn order_test(fetcher: Arc<MetadataFetcher>, store: &dragnet_store::Store, n: i64) {
    let variants: [(&str, &str); 3] = [
        (
            "probe_peers DESC (üretim)",
            "SELECT infohash FROM torrents WHERE metadata_status='pending' AND probe_peers > 0
               ORDER BY probe_peers DESC LIMIT ?1",
        ),
        (
            "probe_at DESC (taze ölçüm)",
            "SELECT infohash FROM torrents WHERE metadata_status='pending' AND probe_peers > 0
               ORDER BY probe_at DESC LIMIT ?1",
        ),
        (
            "last_seen DESC (taze görülme)",
            "SELECT infohash FROM torrents WHERE metadata_status='pending' AND probe_peers > 0
               ORDER BY last_seen DESC LIMIT ?1",
        ),
    ];
    println!("\n=== ADAY SIRALAMASI: hangi sıra CANLI aday veriyor? ===");
    println!(
        "{:<30} {:>7} {:>12} {:>10}",
        "SIRA", "aday", "peer/aday", "0 peer"
    );
    for (label, sql) in variants {
        let rows: Vec<String> = sqlx::query_scalar(sql)
            .bind(n)
            .fetch_all(store.pool())
            .await
            .unwrap_or_default();
        let hashes: Vec<InfoHash> = rows.iter().filter_map(|h| InfoHash::from_hex(h)).collect();
        if hashes.is_empty() {
            continue;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
        let sem = Arc::new(tokio::sync::Semaphore::new(16));
        let total = Arc::new(AtomicU64::new(0));
        let zero = Arc::new(AtomicU64::new(0));
        let mut set = tokio::task::JoinSet::new();
        for ih in hashes.iter().copied() {
            let (s, tot, z, f) = (
                Arc::clone(&sem),
                Arc::clone(&total),
                Arc::clone(&zero),
                Arc::clone(&fetcher),
            );
            set.spawn(async move {
                let _g = s.acquire().await.unwrap();
                let peers = f.peers_of(ih, Duration::from_secs(6), 200).await;
                if peers.is_empty() {
                    z.fetch_add(1, Ordering::Relaxed);
                }
                tot.fetch_add(peers.len() as u64, Ordering::Relaxed);
            });
        }
        while set.join_next().await.is_some() {}
        let n_ih = hashes.len() as f64;
        println!(
            "{label:<30} {:>7} {:>12.1} {:>9.0}%",
            hashes.len(),
            total.load(Ordering::Relaxed) as f64 / n_ih,
            100.0 * zero.load(Ordering::Relaxed) as f64 / n_ih
        );
    }
    println!("\nYORUM: '0 peer' oranı yüksek olan sıralama ÖLÜ aday seçiyordur. Çekim");
    println!("kuyruğu o sırayı kullanıyorsa işçiler boşa yanar — ve hep aynı eski");
    println!("adaylar seçildiği için taze adaylar hiç sıra alamaz.");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<String> = std::env::args().collect();
    let mode = a.get(1).map(String::as_str).unwrap_or("live");

    let (hashes, conc): (Vec<InfoHash>, usize) = match mode {
        "db" | "sweep" | "lookups" | "ordertest" => {
            let store = dragnet_store::Store::open(&a[2]).await?;
            let n: i64 = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(30);
            let conc = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(32);
            // Triyajdan geçmiş, peer'i OLDUĞU BİLİNEN adaylar: üst sınırı bunlar gösterir.
            let rows: Vec<String> = sqlx::query_scalar(
                "SELECT infohash FROM torrents
                  WHERE metadata_status='pending' AND probe_peers > 0
                  ORDER BY probe_at DESC LIMIT ?1",
            )
            .bind(n)
            .fetch_all(store.pool())
            .await?;
            (
                rows.iter().filter_map(|h| InfoHash::from_hex(h)).collect(),
                conc,
            )
        }
        _ => {
            let conc = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(32);
            (
                a[3..]
                    .iter()
                    .filter_map(|h| InfoHash::from_hex(h))
                    .collect(),
                conc,
            )
        }
    };
    println!("mod={mode} infohash={} eszamanlilik={conc}", hashes.len());

    let fetcher = Arc::new(MetadataFetcher::new(FetchConfig::default())?);
    let t = Instant::now();
    println!(
        "bootstrap: {} ({:?})",
        fetcher.wait_bootstrapped().await,
        t.elapsed()
    );

    // Eşzamanlı DHT araması taraması: "aynı anda kaç arama" sorusunu ölçer ve hemen çıkar.
    if mode == "lookups" {
        lookup_sweep(Arc::clone(&fetcher), &hashes).await;
        return Ok(());
    }

    if mode == "ordertest" {
        let store = dragnet_store::Store::open(&a[2]).await?;
        order_test(Arc::clone(&fetcher), &store, 40).await;
        return Ok(());
    }

    let funnel = Arc::new(Funnel::default());
    let sem = Arc::new(tokio::sync::Semaphore::new(conc));
    let t0 = Instant::now();
    let mut lookup_ms = Vec::new();
    let mut peers_per_ih = Vec::new();
    let mut set = tokio::task::JoinSet::new();

    // Tarama modunda peer'ler denenmeden ÖNCE toplanır: her ayar taze bir dilim görsün.
    let mut pool: Vec<(SocketAddrV4, [u8; 20])> = Vec::new();

    for ih in hashes.iter().copied() {
        let tl = Instant::now();
        // Üretimdekiyle aynı bütçe: 20 sn peer toplama.
        let peers = fetcher.peers_of(ih, Duration::from_secs(20), 200).await;
        lookup_ms.push(tl.elapsed().as_millis());
        peers_per_ih.push(peers.len());
        println!(
            "  {ih}: {} peer ({} ms)",
            peers.len(),
            tl.elapsed().as_millis()
        );
        if mode == "sweep" {
            pool.extend(peers.into_iter().map(|p| (p, *ih.as_bytes())));
            continue;
        }
        for p in peers {
            let f = Arc::clone(&funnel);
            let s = Arc::clone(&sem);
            let ihb = *ih.as_bytes();
            set.spawn(async move {
                let _g = s.acquire().await.unwrap();
                probe_peer(p, ihb, &f).await;
            });
        }
    }
    while set.join_next().await.is_some() {}

    if mode == "sweep" {
        println!(
            "\ntoplanan peer havuzu: {} (infohash başına ort. {:.1})",
            pool.len(),
            pool.len() as f64 / hashes.len().max(1) as f64
        );
        sweep(pool).await;
        return Ok(());
    }

    let g = |x: &AtomicU64| x.load(Ordering::Relaxed);
    let peers = g(&funnel.peers).max(1);
    lookup_ms.sort();
    peers_per_ih.sort();
    let med = |v: &Vec<usize>| v.get(v.len() / 2).copied().unwrap_or(0);
    let med128 = |v: &Vec<u128>| v.get(v.len() / 2).copied().unwrap_or(0);
    let pct = |x: u64| 100.0 * x as f64 / peers as f64;

    println!("\n=== DHT ARAMA ===");
    println!(
        "  infohash basina peer: medyan {} / ortalama {:.1}",
        med(&peers_per_ih),
        peers_per_ih.iter().sum::<usize>() as f64 / peers_per_ih.len().max(1) as f64
    );
    println!("  lookup suresi       : medyan {} ms", med128(&lookup_ms));
    println!("\n=== PEER HUNISI (n={peers}) ===");
    println!(
        "  genel adres degil    : {:>6}  %{:.1}",
        g(&funnel.not_public),
        pct(g(&funnel.not_public))
    );
    println!(
        "  TCP zaman asimi      : {:>6}  %{:.1}",
        g(&funnel.connect_timeout),
        pct(g(&funnel.connect_timeout))
    );
    println!(
        "  TCP reddedildi/hata  : {:>6}  %{:.1}",
        g(&funnel.connect_refused),
        pct(g(&funnel.connect_refused))
    );
    println!(
        "  -> BAGLANDI          : {:>6}  %{:.1}  (ort {} ms)",
        g(&funnel.connected),
        pct(g(&funnel.connected)),
        g(&funnel.connect_ms)
            .checked_div(g(&funnel.connected))
            .unwrap_or(0)
    );
    println!(
        "     handshake yok     : {:>6}  %{:.1}  <- sifreli baglanti (MSE) beklentisi?",
        g(&funnel.hs_timeout),
        pct(g(&funnel.hs_timeout))
    );
    println!(
        "     handshake kopuk   : {:>6}  %{:.1}",
        g(&funnel.hs_closed),
        pct(g(&funnel.hs_closed))
    );
    println!(
        "     -> HANDSHAKE OK   : {:>6}  %{:.1}  (ort {} ms)",
        g(&funnel.hs_ok),
        pct(g(&funnel.hs_ok)),
        g(&funnel.hs_ms).checked_div(g(&funnel.hs_ok)).unwrap_or(0)
    );
    println!("        extension yok  : {:>6}", g(&funnel.no_ext));
    println!(
        "        METADATA OK    : {:>6}  %{:.2} (tum peer'lere gore)",
        g(&funnel.meta_ok),
        pct(g(&funnel.meta_ok))
    );
    println!("        metadata hata  : {:>6}", g(&funnel.meta_fail));
    println!("\ntoplam sure: {:?}", t0.elapsed());
    println!("\nYORUM: 'TCP zaman asimi' baskinsa peer adresleri olu/NAT arkasinda ya da");
    println!("bizim cikis baglantilarimiz boguluyor (eszamanliligi degistirip tekrarla).");
    println!("'handshake yok' baskinsa karsi taraf duz BitTorrent protokolunu kabul etmiyor.");
    Ok(())
}
