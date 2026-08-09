// SPDX-License-Identifier: AGPL-3.0-only
//! dragnetd — Dragnet çekirdeğini (dragnet-engine) çalıştıran ince daemon.
//!
//! Tüm boru hattı mantığı `dragnet-engine`'dedir; bu binary yalnızca
//! yapılandırmayı yükler, çekirdeği başlatır, periyodik durum loglar ve Ctrl+C ile
//! zarifçe kapatır. Aynı çekirdeği Tauri masaüstü kabuğu da kullanır.

mod config;

use std::time::Duration;

use tracing::{info, warn};

use dragnet_engine::{Engine, EngineConfig};

use config::Config;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "info,dragnetd=info,dragnet_engine=info,dragnet_dht=info,dragnet_meta=info,mainline=error".into()
            }),
        )
        .init();

    let extra_config = std::env::args().nth(1);
    let cfg = Config::load(extra_config.as_deref())?;
    let api_bind: std::net::SocketAddr = cfg.api_bind.parse()?;
    info!(
        db = %cfg.db_path,
        api = %api_bind,
        harvester_port = cfg.harvester_port,
        fetch_workers = cfg.fetch_workers,
        "dragnetd başlıyor"
    );

    let engine = Engine::start(EngineConfig {
        db_path: cfg.db_path.clone(),
        harvester_port: cfg.harvester_port,
        harvester_max_queries_per_sec: cfg.harvester_max_queries_per_sec,
        fetch_workers: cfg.fetch_workers,
        fetch_peer_concurrency: cfg.fetch_peer_concurrency,
        seed_infohashes: cfg.seed_infohashes.clone(),
    })
    .await?;
    info!(addr = %engine.harvester_addr(), "boru hattı çalışıyor (Ctrl+C ile durur)");

    // Arama API'si çekirdekten AYRI (uzun ömürlü) — indeks deposuna karşı sunar.
    {
        let api_cfg = dragnet_api::ApiConfig {
            bind: api_bind,
            token: cfg.api_token.clone(),
            ..Default::default()
        };
        let api_store = engine.store();
        tokio::spawn(async move {
            if let Err(e) = dragnet_api::serve(api_cfg, api_store).await {
                tracing::error!(error = %e, "API sunucusu durdu");
            }
        });
    }

    // Periyodik durum logu + zarif kapanış.
    let mut ticker = tokio::time::interval(Duration::from_secs(30));
    ticker.tick().await;
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let s = engine.snapshot().await;
                info!(
                    dht_gonderilen = s.harvester.queries_sent,
                    dht_yanit = s.harvester.responses_seen,
                    bep51_ornek = s.harvester.samples_seen,
                    hasat_benzersiz = s.harvester.unique_infohashes,
                    indekslenen = s.fetched_torrents,
                    bilinen = s.total_infohashes,
                    "durum"
                );
                if s.harvester.queries_sent > 0 && s.harvester.responses_seen == 0 {
                    warn!("DHT'den hiç yanıt yok — dış UDP trafiği engelleniyor olabilir");
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("kapanış sinyali alındı, durduruluyor…");
                break;
            }
        }
    }

    let s = engine.snapshot().await;
    info!(fetched = s.fetched_torrents, total = s.total_infohashes, "dragnetd durdu");
    Ok(())
}
