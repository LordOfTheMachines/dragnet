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

    // Semantik arama (opt-in): model indir → yükle → kalıcı indeksi RAM'e al → arka plan
    // indeksleyici. Yuva API ile paylaşılır; başarısızlıkta arama saf FTS olarak sürer.
    let semantic_slot = dragnet_api::search::empty_slot();
    if cfg.semantic_enabled {
        let tier = if cfg.semantic_tier.trim().eq_ignore_ascii_case("auto") {
            let (t, why) = dragnet_semantic::hw::recommend_tier();
            info!(tier = t.as_str(), %why, "semantik kademe otomatik seçildi");
            t
        } else {
            dragnet_semantic::Tier::parse(&cfg.semantic_tier)
        };
        let scfg = dragnet_semantic::SemanticConfig {
            tier,
            device: dragnet_semantic::Device::parse(&cfg.semantic_device),
            models_dir: std::path::PathBuf::from(&cfg.semantic_models_dir),
        };
        let store = engine.store();
        let slot = semantic_slot.clone();
        let cfg_rerank = cfg.semantic_rerank;
        tokio::spawn(async move {
            let scfg2 = scfg.clone();
            let scfg3 = scfg.clone();
            let dl = tokio::task::spawn_blocking(move || {
                dragnet_semantic::Semantic::ensure_model(&scfg2, &|f, d, t| {
                    if t > 0 && d == t {
                        info!(file = f, bytes = t, "model dosyası hazır");
                    }
                })
            })
            .await;
            if let Err(e) = dl
                .map_err(|e| e.to_string())
                .and_then(|r| r.map_err(|e| e.to_string()))
            {
                tracing::error!(error = %e, "semantik model indirilemedi; arama FTS ile sürüyor");
                return;
            }
            let loaded =
                tokio::task::spawn_blocking(move || dragnet_semantic::Semantic::load(&scfg3)).await;
            let sem = match loaded
                .map_err(|e| e.to_string())
                .and_then(|r| r.map_err(|e| e.to_string()))
            {
                Ok(s) => std::sync::Arc::new(s),
                Err(e) => {
                    tracing::error!(error = %e, "semantik model yüklenemedi; arama FTS ile sürüyor");
                    return;
                }
            };
            if let Err(e) = dragnet_engine::semantic_indexer::load_index(&store, &sem).await {
                tracing::error!(error = %e, "semantik indeks yüklenemedi");
                return;
            }
            if cfg_rerank {
                let dir = scfg.models_dir.clone();
                let ok = tokio::task::spawn_blocking(move || {
                    dragnet_semantic::rerank::Reranker::ensure_model(&dir, &|_, _, _| {})
                })
                .await;
                if let Ok(Ok(())) = ok {
                    let dir = scfg.models_dir.clone();
                    if let Ok(Ok(r)) = tokio::task::spawn_blocking(move || {
                        dragnet_semantic::rerank::Reranker::load(
                            &dir,
                            dragnet_semantic::Device::Cpu,
                        )
                    })
                    .await
                    {
                        sem.set_reranker(Some(std::sync::Arc::new(r)));
                        info!("yeniden sıralayıcı hazır");
                    }
                }
            }
            *slot.write().await = Some(std::sync::Arc::clone(&sem));
            info!(
                model = sem.model_id(),
                device = sem.device(),
                "semantik arama hazır"
            );
            // İndeksleyici daemon ömrünce çalışır (görev tutucu bırakılır).
            let _indexer = dragnet_engine::semantic_indexer::spawn_indexer(store, sem);
            std::future::pending::<()>().await;
        });
    }

    // Arama API'si çekirdekten AYRI (uzun ömürlü) — indeks deposuna karşı sunar.
    {
        let api_cfg = dragnet_api::ApiConfig {
            bind: api_bind,
            token: cfg.api_token.clone(),
            ..Default::default()
        };
        let api_store = engine.store();
        let api_slot = semantic_slot.clone();
        tokio::spawn(async move {
            if let Err(e) = dragnet_api::serve_with_semantic(api_cfg, api_store, api_slot).await {
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
                    cekim_deneme = s.fetch.attempts,
                    cekim_ok = s.fetch.ok,
                    cekim_peer_yok = s.fetch.no_peers,
                    cekim_peer_basarisiz = s.fetch.all_peers_failed,
                    cekim_ort_ms = s.fetch.avg_ms,
                    peer_ipucu = s.harvester.peer_hints,
                    p_ok = s.fetch.peer_ok,
                    p_io = s.fetch.peer_io,
                    p_timeout = s.fetch.peer_timeout,
                    p_hs = s.fetch.peer_bad_handshake,
                    p_noext = s.fetch.peer_no_metadata_ext,
                    p_other = s.fetch.peer_other,
                    kuyruk_sicak = s.queue.1,
                    son_saat_fetched = s.queue.3,
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
    info!(
        fetched = s.fetched_torrents,
        total = s.total_infohashes,
        "dragnetd durdu"
    );
    Ok(())
}
