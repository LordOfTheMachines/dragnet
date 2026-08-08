// SPDX-License-Identifier: AGPL-3.0-only
//! Faz 2 demo: bir infohash için DHT'den metadata çeker ve yazdırır.
//!
//! Çalıştırma:
//!   cargo run -p dragnet-meta --example fetch -- <40-hex-infohash>
//!
//! Örnek (Ubuntu 22.04.4 desktop amd64 — genelde iyi seed'li):
//!   cargo run -p dragnet-meta --example fetch -- 9f9165d9a281a9b8e782cd5176bbcc8256fd1871

use std::time::Duration;

use dragnet_core::InfoHash;
use dragnet_meta::{FetchConfig, MetadataFetcher};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,dragnet_meta=debug,mainline=error".into()),
        )
        .init();

    let hex = std::env::args()
        .nth(1)
        .ok_or("kullanım: fetch <40-hex-infohash>")?;
    let infohash = InfoHash::from_hex(&hex).ok_or("geçersiz infohash (40 hex olmalı)")?;

    let fetcher = MetadataFetcher::new(FetchConfig {
        peer_gather_timeout: Duration::from_secs(30),
        ..Default::default()
    })?;

    println!("infohash {infohash} için metadata çekiliyor…");
    match fetcher.fetch(infohash).await {
        Ok(rec) => {
            println!("\n✅ BAŞARILI");
            println!("  isim      : {}", rec.name);
            println!("  boyut     : {} bayt", rec.total_size);
            println!("  dosya     : {}", rec.files.len());
            for (i, f) in rec.files.iter().take(20).enumerate() {
                println!("    {:>3}. {} ({} bayt)", i + 1, f.path, f.size);
            }
            if rec.files.len() > 20 {
                println!("    … ve {} dosya daha", rec.files.len() - 20);
            }
        }
        Err(e) => {
            println!("\n❌ BAŞARISIZ: {e}");
            std::process::exit(1);
        }
    }
    Ok(())
}
