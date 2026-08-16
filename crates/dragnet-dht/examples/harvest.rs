// SPDX-License-Identifier: AGPL-3.0-only
//! Faz 1 demo: Mainline DHT'den infohash hasat edip terminale basar.
//!
//! Çalıştırma:
//!   cargo run -p dragnet-dht --example harvest
//!
//! Birkaç dakika içinde gerçek infohash'ler akmaya başlamalıdır. Her 10 saniyede
//! bir özet sayaç satırı yazılır. Ctrl+C ile durdurulur.

use std::time::Duration;

use dragnet_dht::{spawn, HarvesterConfig};
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // RUST_LOG ile ayarlanabilir; varsayılan: info.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,dragnet_dht=info".into()),
        )
        .init();

    let mut harvester = spawn(HarvesterConfig::default()).await?;
    let stats = harvester.stats();
    info!(addr = %harvester.local_addr(), "hasat başladı — infohash'ler bekleniyor…");

    // Periyodik istatistik yazıcı.
    let stats_task = {
        let stats = stats.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(10));
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let s = stats.snapshot();
                info!(
                    unique = s.unique_infohashes,
                    dup = s.duplicates,
                    get_peers = s.get_peers_seen,
                    announce = s.announce_seen,
                    responses = s.responses_seen,
                    nodes = s.nodes_learned,
                    sent = s.queries_sent,
                    dropped = s.dropped_channel_full,
                    "özet"
                );
            }
        })
    };

    let mut count: u64 = 0;
    loop {
        tokio::select! {
            maybe = harvester.infohashes.recv() => {
                match maybe {
                    Some(ih) => {
                        count += 1;
                        // magnet linki qBittorrent'in tükettiği biçimdir.
                        println!("{count:>6}  {}  {}", ih.to_hex(), ih.to_magnet(None));
                    }
                    None => break,
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("kapatılıyor…");
                break;
            }
        }
    }

    stats_task.abort();
    let s = stats.snapshot();
    info!(toplam_benzersiz = s.unique_infohashes, "bitti");
    Ok(())
}
