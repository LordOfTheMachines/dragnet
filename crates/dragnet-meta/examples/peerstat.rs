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

/// Bağlanma ve handshake için ayrı, ölçülebilir zaman aşımları.
const CONNECT_TO: Duration = Duration::from_millis(3500);
const HS_TO: Duration = Duration::from_millis(4500);

/// Tek peer'i adım adım dener ve her adımı ayrı sayar.
async fn probe_peer(addr: SocketAddrV4, ih: [u8; 20], f: &Funnel) {
    f.peers.fetch_add(1, Ordering::Relaxed);
    if !wire::is_public_peer(&addr) {
        f.not_public.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let t = Instant::now();
    let stream = match tokio::time::timeout(CONNECT_TO, TcpStream::connect(addr)).await {
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
    match tokio::time::timeout(HS_TO, s.read_exact(&mut resp)).await {
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<String> = std::env::args().collect();
    let mode = a.get(1).map(String::as_str).unwrap_or("live");

    let (hashes, conc): (Vec<InfoHash>, usize) = match mode {
        "db" => {
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

    let fetcher = MetadataFetcher::new(FetchConfig::default())?;
    let t = Instant::now();
    println!(
        "bootstrap: {} ({:?})",
        fetcher.wait_bootstrapped().await,
        t.elapsed()
    );

    let funnel = Arc::new(Funnel::default());
    let sem = Arc::new(tokio::sync::Semaphore::new(conc));
    let t0 = Instant::now();
    let mut lookup_ms = Vec::new();
    let mut peers_per_ih = Vec::new();
    let mut set = tokio::task::JoinSet::new();

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
