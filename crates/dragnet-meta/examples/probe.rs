// SPDX-License-Identifier: AGPL-3.0-only
//! Fetch başarı/süre deneyi: `probe <hash-dosyası> [eşzamanlılık] [gather_sn]`.
//! Her satır bir infohash; sonuçta başarı oranı, peer bulunma oranı ve süreler yazılır.
use std::sync::Arc;
use std::time::{Duration, Instant};

use dragnet_core::InfoHash;
use dragnet_meta::{FetchConfig, FetchError, MetadataFetcher};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("hash dosyası");
    let conc: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let gather: u64 = std::env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let hashes: Vec<InfoHash> = std::fs::read_to_string(&path)?
        .lines()
        .filter_map(InfoHash::from_hex)
        .collect();
    let fetcher = Arc::new(MetadataFetcher::new(FetchConfig {
        overall_timeout: Duration::from_secs(gather),
        ..Default::default()
    })?);
    let t = Instant::now();
    let ok = fetcher.wait_bootstrapped().await;
    println!(
        "bootstrap: {ok} in {:?}; {}",
        t.elapsed(),
        fetcher.dht_info().await
    );
    if let Ok(w) = std::env::var("WARMUP") {
        tokio::time::sleep(Duration::from_secs(w.parse().unwrap_or(0))).await;
        println!("ısınma sonrası: {}", fetcher.dht_info().await);
    }
    let sem = Arc::new(tokio::sync::Semaphore::new(conc));
    let t0 = Instant::now();
    let mut set = tokio::task::JoinSet::new();
    for ih in hashes.iter().copied() {
        let f = Arc::clone(&fetcher);
        let s = Arc::clone(&sem);
        set.spawn(async move {
            let _p = s.acquire().await.unwrap();
            let t = Instant::now();
            let r = f.fetch(ih).await;
            (r, t.elapsed())
        });
    }
    let (mut ok, mut nopeers, mut allfail, mut other) = (0, 0, 0, 0);
    let mut ok_ms = Vec::new();
    while let Some(r) = set.join_next().await {
        let (res, dt) = r?;
        match res {
            Ok(rec) => {
                ok += 1;
                ok_ms.push(dt.as_millis());
                println!("OK  {:>6}ms {}", dt.as_millis(), rec.name);
            }
            Err(FetchError::NoPeers) => nopeers += 1,
            Err(FetchError::AllPeersFailed { .. }) => allfail += 1,
            Err(_) => other += 1,
        }
    }
    ok_ms.sort();
    let med = ok_ms.get(ok_ms.len() / 2).copied().unwrap_or(0);
    println!("\n=== n={} ok={} ({:.1}%) no_peers={} all_peers_failed={} other={} | ok medyan {} ms | toplam {:?} (conc={conc}, gather={gather}s)",
        hashes.len(), ok, 100.0 * ok as f64 / hashes.len() as f64, nopeers, allfail, other, med, t0.elapsed());
    Ok(())
}
