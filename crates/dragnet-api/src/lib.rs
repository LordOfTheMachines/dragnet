// SPDX-License-Identifier: AGPL-3.0-only
//! dragnet-api — HTTP arama API'si (Faz 4).
//!
//! `axum` tabanlı REST. Uç noktalar:
//! - `GET /search?q=<sorgu>&cat=<kategori>&limit=<n>` → JSON sonuç listesi.
//! - `GET /healthz` → sağlık kontrolü.
//! - `GET /stats` → indeks büyüklüğü.
//!
//! Bu, qBittorrent plugin'inin konuştuğu tek yüzeydir; JSON sözleşmesi
//! `docs/INTEGRATION.md` ile hizalıdır. Varsayılan bind `127.0.0.1`'dir;
//! opsiyonel bir bearer token ile korunabilir.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tracing::info;

use dragnet_store::{Filter, SortKey, Store};

pub mod search;
pub use search::{SearchMode, SemanticSlot};

/// API yapılandırması.
#[derive(Debug, Clone)]
pub struct ApiConfig {
    /// Dinlenecek adres. Varsayılan `127.0.0.1:8080`.
    pub bind: SocketAddr,
    /// Ayarlanırsa `/search` ve `/stats` için `Authorization: Bearer <token>` gerekir.
    pub token: Option<String>,
    /// `limit` parametresi için üst sınır.
    pub max_limit: usize,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 8080)),
            token: None,
            max_limit: 500,
        }
    }
}

/// Paylaşılan uygulama durumu.
#[derive(Clone)]
struct AppState {
    store: Store,
    semantic: SemanticSlot,
    token: Option<String>,
    max_limit: usize,
}

/// `/search` sorgu parametreleri.
#[derive(Debug, Deserialize)]
struct SearchParams {
    #[serde(default)]
    q: String,
    /// Kategori (qBittorrent kategorisi ya da doğrudan bizim kategori adımız).
    #[serde(default)]
    cat: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    /// Sayfalama ofseti (sonsuz-scroll).
    #[serde(default)]
    offset: Option<usize>,
    /// Sıralama anahtarı (name/size/seed/files/date/added/seen; boş = alaka).
    #[serde(default)]
    sort: Option<String>,
    /// Azalan sıra (varsayılan true).
    #[serde(default)]
    desc: Option<bool>,
    /// Yalnız canlı (peer > 0) sonuçlar.
    #[serde(default)]
    alive: Option<bool>,
    /// Yetişkin içeriği gizle.
    #[serde(default)]
    hide_adult: Option<bool>,
    /// Bozuk (çözülemeyen kodlama) adları gizle (varsayılan true).
    #[serde(default)]
    hide_garbled: Option<bool>,
    /// Arama modu: `fts` | `semantic` | `hybrid` (boş/bilinmeyen = otomatik: semantik
    /// hazırsa hibrit, değilse FTS). Plugin göndermez → eski davranış korunur.
    #[serde(default)]
    mode: Option<String>,
    /// Güven kapısını atla: karşılığı zayıf sorguda da en yakın sonuçları döndür.
    #[serde(default)]
    weak: Option<bool>,
}

/// qBittorrent kategorisini (ya da bizim adımızı) iç kategoriye eşler. `all`/boş → None.
fn map_category(cat: Option<String>) -> Option<String> {
    let c = cat?.to_lowercase();
    let mapped = match c.as_str() {
        "all" | "" => return None,
        "movies" | "tv" | "anime" | "video" => "video",
        "music" | "audio" => "audio",
        "games" | "game" => "game",
        "software" | "apps" => "software",
        "books" | "book" => "book",
        "adult" => "adult",
        "archive" => "archive",
        "other" => "other",
        _ => return None,
    };
    Some(mapped.to_string())
}

/// Tek bir arama sonucu (INTEGRATION.md JSON sözleşmesi).
#[derive(Debug, Serialize)]
struct SearchItem {
    infohash: String,
    name: String,
    size: u64,
    /// DHT crawl'ından seed/leech gelmez; -1 = bilinmiyor.
    seeds: i64,
    leech: i64,
    /// Son görülme (unix ts) — yayın tarihi vekili.
    pub_date: i64,
    /// İçerik kategorisi (video/audio/software/game/book/adult/archive/other).
    category: String,
}

#[derive(Debug, Serialize)]
struct SearchResponse {
    results: Vec<SearchItem>,
    /// Gerçekte kullanılan mod (`fts`/`semantic`/`hybrid`).
    mode: &'static str,
    /// Sorgunun korpusta karşılığı yok (sonuç listesi bilerek boş).
    weak: bool,
    /// Yazım düzeltmesi uygulandıysa aranan düzeltilmiş sorgu.
    #[serde(skip_serializing_if = "Option::is_none")]
    corrected: Option<String>,
}

#[derive(Debug, Serialize)]
struct StatsResponse {
    fetched_torrents: i64,
    total_infohashes: i64,
    /// Semantik katman durumu (`None` = kapalı).
    semantic: Option<dragnet_semantic::SemanticStatus>,
}

/// Verilen store ve yapılandırmayla axum router'ı kurar (semantik kapalı).
pub fn router(store: Store, config: &ApiConfig) -> Router {
    router_with_semantic(store, config, search::empty_slot())
}

/// Semantik yuvasıyla router: yuva çalışma anında doldurulup boşaltılabilir.
pub fn router_with_semantic(store: Store, config: &ApiConfig, semantic: SemanticSlot) -> Router {
    let state = AppState {
        store,
        semantic,
        token: config.token.clone(),
        max_limit: config.max_limit,
    };
    Router::new()
        .route("/healthz", get(healthz))
        .route("/search", get(search))
        .route("/stats", get(stats))
        .with_state(Arc::new(state))
}

/// API'yi başlatır ve bloklar (sunucu sonlanana kadar).
pub async fn serve(config: ApiConfig, store: Store) -> std::io::Result<()> {
    serve_with_semantic(config, store, search::empty_slot()).await
}

/// Semantik yuvasıyla başlatır (uygulama/daemon).
pub async fn serve_with_semantic(
    config: ApiConfig,
    store: Store,
    semantic: SemanticSlot,
) -> std::io::Result<()> {
    let app = router_with_semantic(store, &config, semantic);
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    let addr = listener.local_addr()?;
    info!(%addr, "dragnet-api dinliyor");
    axum::serve(listener, app).await
}

async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// Opsiyonel bearer token doğrulaması. Token ayarlı değilse her istek geçer.
/// Reddedilirse `Some(<401 yanıtı>)`, geçerse `None` döner.
fn check_auth(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    let expected = state.token.as_ref()?;
    let provided = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match provided {
        Some(t) if t == expected => None,
        _ => Some((StatusCode::UNAUTHORIZED, "yetkisiz").into_response()),
    }
}

async fn search(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<SearchParams>,
) -> Response {
    if let Some(resp) = check_auth(&state, &headers) {
        return resp;
    }

    let limit = params.limit.unwrap_or(100).min(state.max_limit).max(1);
    let offset = params.offset.unwrap_or(0);
    let sort = SortKey::parse(params.sort.as_deref().unwrap_or(""));
    let desc = params.desc.unwrap_or(true);

    let filter = Filter {
        only_alive: params.alive.unwrap_or(false),
        hide_adult: params.hide_adult.unwrap_or(false),
        category: map_category(params.cat),
        block_keywords: Vec::new(),
        hide_garbled: params.hide_garbled.unwrap_or(true),
    };
    let mode = SearchMode::parse(params.mode.as_deref().unwrap_or(""));
    match search::search(
        &state.store,
        &state.semantic,
        &params.q,
        mode,
        limit as i64,
        offset as i64,
        sort,
        desc,
        &filter,
        params.weak.unwrap_or(false),
    )
    .await
    {
        Ok(outcome) => {
            let used = outcome.used.as_str();
            let weak = outcome.weak;
            let corrected = outcome.corrected.clone();
            let results = outcome
                .rows
                .into_iter()
                .map(|r| SearchItem {
                    // Canlılık scrape'inden peer sayısı; henüz kontrol edilmediyse -1.
                    seeds: r.peer_count.unwrap_or(-1),
                    infohash: r.infohash.to_hex(),
                    name: r.name,
                    size: r.total_size,
                    leech: -1,
                    pub_date: r.last_seen,
                    category: r.category,
                })
                .collect();
            Json(SearchResponse {
                results,
                mode: used,
                weak,
                corrected,
            })
            .into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "arama hatası");
            (StatusCode::INTERNAL_SERVER_ERROR, "arama hatası").into_response()
        }
    }
}

async fn stats(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(resp) = check_auth(&state, &headers) {
        return resp;
    }
    let fetched = state.store.count_fetched().await.unwrap_or(0);
    let total = state.store.count_total().await.unwrap_or(0);
    let semantic = state.semantic.read().await.as_ref().map(|s| s.status());
    Json(StatsResponse {
        fetched_torrents: fetched,
        total_infohashes: total,
        semantic,
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use dragnet_core::{InfoHash, TorrentFile, TorrentRecord};
    use tower::ServiceExt; // oneshot

    fn record(hex: &str, name: &str, size: u64) -> TorrentRecord {
        TorrentRecord {
            infohash: InfoHash::from_hex(hex).unwrap(),
            name: name.to_string(),
            total_size: size,
            files: vec![TorrentFile {
                path: name.to_string(),
                size,
            }],
            first_seen: 1000,
            last_seen: 2000,
            seen_count: 5,
        }
    }

    async fn seeded_store() -> Store {
        let store = Store::in_memory().await.unwrap();
        store
            .upsert_torrent(&record(
                "1111111111111111111111111111111111111111",
                "Ubuntu 24.04 Desktop",
                4096,
            ))
            .await
            .unwrap();
        store
    }

    async fn body_json(resp: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    }

    #[tokio::test]
    async fn healthz_returns_ok() {
        let app = router(Store::in_memory().await.unwrap(), &ApiConfig::default());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn search_returns_matching_results() {
        let app = router(seeded_store().await, &ApiConfig::default());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/search?q=ubuntu&limit=10")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        let results = json["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["name"], "Ubuntu 24.04 Desktop");
        assert_eq!(results[0]["size"], 4096);
        assert_eq!(results[0]["seeds"], -1);
        assert_eq!(
            results[0]["infohash"],
            "1111111111111111111111111111111111111111"
        );
    }

    #[tokio::test]
    async fn search_empty_for_no_match() {
        let app = router(seeded_store().await, &ApiConfig::default());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/search?q=nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let json = body_json(resp).await;
        assert_eq!(json["results"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn stats_reports_counts() {
        let app = router(seeded_store().await, &ApiConfig::default());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/stats")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let json = body_json(resp).await;
        assert_eq!(json["fetched_torrents"], 1);
        assert_eq!(json["total_infohashes"], 1);
    }

    #[tokio::test]
    async fn mode_param_and_semantic_stats_over_http() {
        use dragnet_semantic::{MockEmbedder, Semantic, Tier};
        let store = seeded_store().await;
        store
            .upsert_torrent(&record(
                "2222222222222222222222222222222222222222",
                "Fedora Workstation 40",
                1,
            ))
            .await
            .unwrap();
        let slot = search::empty_slot();
        let app = || router_with_semantic(store.clone(), &ApiConfig::default(), slot.clone());

        // Kapalı: mode=hybrid istense de yanıt mode=fts, stats.semantic=null.
        let json = body_json(
            app()
                .oneshot(
                    Request::builder()
                        .uri("/search?q=ubuntu&mode=hybrid")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(json["mode"], "fts");
        assert_eq!(json["results"].as_array().unwrap().len(), 1);
        let st = body_json(
            app()
                .oneshot(
                    Request::builder()
                        .uri("/stats")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert!(st["semantic"].is_null());

        // Aç (mock) → hibrit; stats.semantic dolu; mode=fts hâlâ zorlanabilir.
        let sem = Arc::new(Semantic::with_embedder(
            Box::new(MockEmbedder::new(32)),
            Tier::Light,
            0.0,
        ));
        sem.embed_and_add(&[
            (
                InfoHash::from_hex("1111111111111111111111111111111111111111").unwrap(),
                "Ubuntu 24.04 Desktop".into(),
            ),
            (
                InfoHash::from_hex("2222222222222222222222222222222222222222").unwrap(),
                "Fedora Workstation 40".into(),
            ),
        ])
        .unwrap();
        *slot.write().await = Some(sem);
        let json = body_json(
            app()
                .oneshot(
                    Request::builder()
                        .uri("/search?q=ubuntu")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(json["mode"], "hybrid");
        assert_eq!(json["results"][0]["name"], "Ubuntu 24.04 Desktop");
        let json = body_json(
            app()
                .oneshot(
                    Request::builder()
                        .uri("/search?q=ubuntu&mode=fts")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(json["mode"], "fts");
        let st = body_json(
            app()
                .oneshot(
                    Request::builder()
                        .uri("/stats")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(st["semantic"]["indexed"], 2);
        assert_eq!(st["semantic"]["model_id"], "mock");
    }

    #[tokio::test]
    async fn auth_enforced_when_token_set() {
        let config = ApiConfig {
            token: Some("secret123".into()),
            ..Default::default()
        };
        let store = seeded_store().await;

        // Token yok → 401.
        let resp = router(store.clone(), &config)
            .oneshot(
                Request::builder()
                    .uri("/search?q=ubuntu")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Yanlış token → 401.
        let resp = router(store.clone(), &config)
            .oneshot(
                Request::builder()
                    .uri("/search?q=ubuntu")
                    .header("Authorization", "Bearer wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Doğru token → 200.
        let resp = router(store, &config)
            .oneshot(
                Request::builder()
                    .uri("/search?q=ubuntu")
                    .header("Authorization", "Bearer secret123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // healthz auth gerektirmez.
        let resp = router(seeded_store().await, &config)
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
