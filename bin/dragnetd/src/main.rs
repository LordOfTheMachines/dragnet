// SPDX-License-Identifier: AGPL-3.0-only
//! dragnetd — tüm bileşenleri tek süreçte birleştiren daemon (Faz 5).
//!
//! Boru hattı:
//! ```text
//! dragnet-dht (harvester) ──infohash──► [record_sighting] ──► fetcher havuzu (sem)
//!                                                                    │ metadata
//!                                                                    ▼
//!                                                         dragnet-store (SQLite+FTS5)
//!                                                                    ▲
//!                                                     dragnet-api (axum) ── /search
//! ```
//! Harvester hızlıdır, metadata çekimi yavaş; fetcher havuzu bir Semaphore ile
//! sınırlıdır ve dolduğunda o infohash'in çekimi düşürülür (sighting yine yazılır)
//! — bounded/backpressure ilkesi (ARCHITECTURE §1.4).

mod config;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::Semaphore;
use tracing::{debug, error, info, warn};

use dragnet_api::ApiConfig;
use dragnet_core::InfoHash;
use dragnet_dht::HarvesterConfig;
use dragnet_meta::{FetchConfig, MetadataFetcher};
use dragnet_store::Store;

use config::Config;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "info,dragnetd=info,dragnet_dht=info,dragnet_meta=info,mainline=error".into()
            }),
        )
        .init();

    // İsteğe bağlı ilk argüman: ek yapılandırma dosyası yolu.
    let extra_config = std::env::args().nth(1);
    let cfg = Config::load(extra_config.as_deref())?;
    let api_addr: std::net::SocketAddr = cfg.api_bind.parse()?;
    info!(
        db = %cfg.db_path,
        api = %api_addr,
        harvester_port = cfg.harvester_port,
        fetch_workers = cfg.fetch_workers,
        "dragnetd başlıyor"
    );

    // Depolama.
    let store = Store::open(&cfg.db_path).await?;

    // Metadata fetcher (kendi DHT istemcisi).
    let fetcher = Arc::new(MetadataFetcher::new(FetchConfig {
        concurrency: cfg.fetch_peer_concurrency,
        ..Default::default()
    })?);

    // DHT harvester.
    let mut harvester = dragnet_dht::spawn(HarvesterConfig {
        port: cfg.harvester_port,
        max_queries_per_sec: cfg.harvester_max_queries_per_sec,
        ..Default::default()
    })
    .await?;
    info!(addr = %harvester.local_addr(), "harvester çalışıyor");

    // Arama API'si (ayrı görev).
    let api_task = {
        let api_cfg = ApiConfig {
            bind: api_addr,
            token: cfg.api_token.clone(),
            ..Default::default()
        };
        let api_store = store.clone();
        tokio::spawn(async move {
            if let Err(e) = dragnet_api::serve(api_cfg, api_store).await {
                error!(error = %e, "API sunucusu durdu");
            }
        })
    };

    // Periyodik durum logu (her 30 sn) — DHT'nin canlı olup olmadığını gösterir.
    // `sent`/`nodes`/`responses` artıyorsa DHT çalışıyor; hepsi 0 ise dış UDP engelli.
    {
        let hstats = harvester.stats();
        let store = store.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30));
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let s = hstats.snapshot();
                let indexed = store.count_fetched().await.unwrap_or(0);
                let known = store.count_total().await.unwrap_or(0);
                info!(
                    dht_gonderilen = s.queries_sent,
                    dht_yanit = s.responses_seen,
                    bep51_ornek = s.samples_seen,
                    hasat_benzersiz = s.unique_infohashes,
                    hasat_get_peers = s.get_peers_seen,
                    indekslenen = indexed,
                    bilinen = known,
                    "durum"
                );
                if s.queries_sent > 0 && s.responses_seen == 0 && s.nodes_learned == 0 {
                    warn!(
                        "DHT'den hiç yanıt yok — dış UDP trafiği engelleniyor olabilir \
                         (Windows Güvenlik Duvarı'nda dragnetd.exe'ye izin verin)"
                    );
                }
            }
        });
    }

    // Fetcher havuzu: aynı anda en fazla `fetch_workers` çekim.
    let sem = Arc::new(Semaphore::new(cfg.fetch_workers.max(1)));

    // Başlangıç seed infohash'leri: indeksi ısıtmak/sabitlemek için hemen çek.
    for hex in &cfg.seed_infohashes {
        match InfoHash::from_hex(hex) {
            Some(ih) => {
                info!(infohash = %ih, "seed infohash çekiliyor");
                let store = store.clone();
                let fetcher = Arc::clone(&fetcher);
                tokio::spawn(async move {
                    fetch_and_store(ih, &store, &fetcher).await;
                });
            }
            None => warn!(hex, "geçersiz seed infohash (40-hex olmalı), atlanıyor"),
        }
    }

    // Ana boru hattı döngüsü.
    info!("boru hattı çalışıyor — infohash akışı bekleniyor (Ctrl+C ile durur)");
    loop {
        tokio::select! {
            maybe = harvester.infohashes.recv() => {
                let Some(infohash) = maybe else {
                    warn!("harvester kanalı kapandı");
                    break;
                };

                // Her görülen infohash için görülme kaydı (ucuz, popülerlik vekili).
                if let Err(e) = store.record_sighting(infohash, now_unix()).await {
                    debug!(error = %e, "record_sighting hatası");
                }

                // Zaten metadata'sı varsa yeniden çekme.
                match store.has_metadata(infohash).await {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(e) => { debug!(error = %e, "has_metadata hatası"); continue; }
                }

                // Havuzda yer varsa çekimi başlat; yoksa düşür (backpressure).
                if let Ok(permit) = Arc::clone(&sem).try_acquire_owned() {
                    let store = store.clone();
                    let fetcher = Arc::clone(&fetcher);
                    tokio::spawn(async move {
                        let _permit = permit; // düştüğünde havuz izni serbest kalır
                        fetch_and_store(infohash, &store, &fetcher).await;
                    });
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("kapanış sinyali alındı, durduruluyor…");
                break;
            }
        }
    }

    api_task.abort();
    let fetched = store.count_fetched().await.unwrap_or(0);
    let total = store.count_total().await.unwrap_or(0);
    info!(fetched, total, "dragnetd durdu");
    Ok(())
}

/// Bir infohash için metadata çeker ve başarılıysa depoya yazar.
async fn fetch_and_store(infohash: InfoHash, store: &Store, fetcher: &MetadataFetcher) {
    match fetcher.fetch(infohash).await {
        Ok(record) => {
            let files = record.files.len();
            let name = record.name.clone();
            match store.upsert_torrent(&record).await {
                Ok(()) => {
                    info!(infohash = %infohash, name = %name, files, "metadata indekslendi")
                }
                Err(e) => error!(error = %e, "torrent yazılamadı"),
            }
        }
        Err(e) => debug!(infohash = %infohash, error = %e, "metadata çekilemedi"),
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
