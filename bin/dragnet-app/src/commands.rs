// SPDX-License-Identifier: AGPL-3.0-only
//! Frontend'in çağırdığı Tauri komutları.

use std::net::TcpStream;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tauri::{AppHandle, State};

use dragnet_api::SearchMode;
use dragnet_engine::Engine;
use dragnet_store::{Filter, Overview, SortKey, TorrentSummary};

use crate::settings::Settings;
use crate::{autostart, updater, AppState};

fn summary_json(s: &TorrentSummary) -> Value {
    json!({
        "infohash": s.infohash.to_hex(),
        "name": s.name,
        "size": s.total_size,
        "files": s.file_count,
        "seen": s.seen_count,
        "peers": s.peer_count, // null = henüz kontrol edilmedi, 0 = ölü, N = canlı
        "category": s.category,
        "first_seen": s.first_seen,
        "last_seen": s.last_seen,
        "magnet": s.infohash.to_magnet(Some(&s.name)),
    })
}

fn overview_json(o: &Overview) -> Value {
    json!({
        "fetched": o.fetched,
        "total_infohashes": o.total_infohashes,
        "total_size": o.total_size,
        "total_files": o.total_files,
        "total_peers": o.total_peers,
        "alive": o.alive,
        "dead": o.dead,
        "unchecked": o.unchecked,
        "categories": o.categories.iter()
            .map(|(c, n, s)| json!({ "category": c, "count": n, "size": s }))
            .collect::<Vec<_>>(),
    })
}

fn make_filter(
    hide_adult: bool,
    only_alive: bool,
    category: Option<String>,
    block_keywords: Vec<String>,
) -> Filter {
    Filter {
        only_alive,
        hide_adult,
        category: category.filter(|c| !c.is_empty() && c != "all"),
        block_keywords,
    }
}

/// Ayarlardaki engel kelimelerini (normalize edilmiş) döndürür.
fn block_keywords(state: &AppState) -> Vec<String> {
    let s = state.settings.lock().unwrap_or_else(|p| p.into_inner());
    s.block_keywords
        .iter()
        .map(|k| k.trim().to_lowercase())
        .filter(|k| !k.is_empty())
        .collect()
}

fn summaries_json(v: &[TorrentSummary]) -> Vec<Value> {
    v.iter().map(summary_json).collect()
}

/// Sürüm bilgisi.
#[tauri::command]
pub fn app_info() -> Value {
    json!({ "name": "Dragnet", "version": env!("CARGO_PKG_VERSION") })
}

/// Canlı durum: tarama açık mı, indeks sayaçları, harvester metrikleri, hız.
#[tauri::command]
pub async fn get_stats(state: State<'_, AppState>) -> Result<Value, String> {
    let (scanning, harvester, addr, fetch, queue) = {
        let guard = state.engine.lock().await;
        match guard.as_ref() {
            Some(e) => {
                let snap = e.snapshot().await;
                (
                    true,
                    Some(snap.harvester),
                    Some(e.harvester_addr().to_string()),
                    Some(snap.fetch),
                    Some(snap.queue),
                )
            }
            None => (false, None, None, None, None),
        }
    };

    let fetched = state.store.count_fetched().await.unwrap_or(0);
    let total = state.store.count_total().await.unwrap_or(0);

    // BEP-51 örnek/sn (delta).
    let samples = harvester.as_ref().map(|h| h.samples_seen).unwrap_or(0);
    let sample_rate = {
        let mut prev = state.rate_prev.lock().unwrap_or_else(|p| p.into_inner());
        let dt = prev.1.elapsed().as_secs_f64();
        let rate = if dt > 0.1 && samples >= prev.0 {
            (samples - prev.0) as f64 / dt
        } else {
            0.0
        };
        *prev = (samples, Instant::now());
        rate.round() as u64
    };

    Ok(json!({
        "scanning": scanning,
        "fetched": fetched,
        "total": total,
        "harvester_addr": addr,
        "queries_sent": harvester.as_ref().map(|h| h.queries_sent).unwrap_or(0),
        "responses": harvester.as_ref().map(|h| h.responses_seen).unwrap_or(0),
        "samples": samples,
        "unique": harvester.as_ref().map(|h| h.unique_infohashes).unwrap_or(0),
        "peer_hints": harvester.as_ref().map(|h| h.peer_hints).unwrap_or(0),
        "sample_rate": sample_rate,
        // Faz E: çekim boru hattı sayaçları + kuyruk (pano "Metadata çekimi" kartı).
        "fetch": fetch.map(|f| json!({
            "attempts": f.attempts, "ok": f.ok, "no_peers": f.no_peers,
            "all_peers_failed": f.all_peers_failed, "avg_ms": f.avg_ms, "avg_peers": f.avg_peers,
        })),
        "queue": queue.map(|(p, h, u, r)| json!({ "pending": p, "hot": h, "unreachable": u, "fetched_last_hour": r })),
        "semantic": state.semantic.status_json().await,
    }))
}

/// Semantik arama durumu (aşama, indirme ilerlemesi, model/cihaz, indekslenen sayısı).
#[tauri::command]
pub async fn semantic_status(state: State<'_, AppState>) -> Result<Value, String> {
    Ok(state.semantic.status_json().await)
}

/// Birleşik arama/gözat (tek sıralanabilir tablo). Sorgu boşsa FTS yerine tüm
/// indeksi listeler (gözat). `sort`/`desc`/`offset` ile sunucu-tarafı sıralama +
/// sonsuz-scroll sayfalama; engel kelimeleri ayarlardan uygulanır.
// Tauri komutu: parametreler frontend'den tek tek gelir (bir struct'a sarmak
// invoke sözleşmesini gereksiz karmaşıklaştırır).
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn search(
    state: State<'_, AppState>,
    query: String,
    limit: Option<i64>,
    offset: Option<i64>,
    sort: Option<String>,
    desc: Option<bool>,
    hide_adult: Option<bool>,
    only_alive: Option<bool>,
    category: Option<String>,
    mode: Option<String>,
) -> Result<Value, String> {
    let blocks = block_keywords(&state);
    let filter = make_filter(
        hide_adult.unwrap_or(false),
        only_alive.unwrap_or(false),
        category,
        blocks,
    );
    let q = query.trim();
    let limit = limit.unwrap_or(60);
    let offset = offset.unwrap_or(0);
    let sort_key = SortKey::parse(sort.as_deref().unwrap_or(""));
    let desc = desc.unwrap_or(true);

    // Ortak arama yolu (API ile aynı): boş sorgu → gözat; semantik yuvası doluysa hibrit.
    let outcome = dragnet_api::search::search(
        &state.store,
        &state.semantic.slot,
        q,
        SearchMode::parse(mode.as_deref().unwrap_or("")),
        limit,
        offset,
        sort_key,
        desc,
        &filter,
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(
        json!({ "results": summaries_json(&outcome.rows), "count": outcome.rows.len(), "mode": outcome.used.as_str() }),
    )
}

/// Dashboard: genel görünüm + keşif zaman serisi. `bucket` "hour"|"day",
/// `points` kova sayısı (grafik gün/saat seçimi).
#[tauri::command]
pub async fn dashboard(
    state: State<'_, AppState>,
    bucket: Option<String>,
    points: Option<i64>,
) -> Result<Value, String> {
    let is_day = bucket.as_deref() == Some("day");
    let bucket_secs = if is_day { 86_400 } else { 3_600 };
    let pts = points.unwrap_or(if is_day { 30 } else { 48 }).clamp(1, 365);
    let series = state
        .store
        .discovery(bucket_secs, pts)
        .await
        .map_err(|e| e.to_string())?;
    let overview = state.store.overview().await.map_err(|e| e.to_string())?;
    Ok(json!({
        "series": series.iter().map(|(t, n)| json!({ "t": t, "count": n })).collect::<Vec<_>>(),
        "bucket": if is_day { "day" } else { "hour" },
        "overview": overview_json(&overview),
    }))
}

/// Ağ sağlığı: birkaç hedefe TCP bağlantı gecikmesi (ICMP admin gerektirdiğinden TCP).
#[tauri::command]
pub async fn network_health() -> Result<Value, String> {
    const TARGETS: [(&str, &str); 3] = [
        ("Google", "google.com:443"),
        ("Cloudflare", "1.1.1.1:443"),
        ("GitHub", "github.com:443"),
    ];
    let mut probes = Vec::new();
    for (name, addr) in TARGETS {
        let (ms, ok) = tauri::async_runtime::spawn_blocking(move || probe(addr))
            .await
            .unwrap_or((0, false));
        probes.push(json!({ "name": name, "target": addr, "ms": ms, "ok": ok }));
    }
    Ok(json!({ "probes": probes }))
}

fn probe(addr: &str) -> (u64, bool) {
    use std::net::ToSocketAddrs;
    let start = Instant::now();
    match addr.to_socket_addrs().ok().and_then(|mut a| a.next()) {
        Some(sa) => match TcpStream::connect_timeout(&sa, Duration::from_secs(2)) {
            Ok(_) => (start.elapsed().as_millis() as u64, true),
            Err(_) => (start.elapsed().as_millis() as u64, false),
        },
        None => (0, false),
    }
}

/// Taramayı başlat (çekirdeği ayarlarla ayağa kaldır).
#[tauri::command]
pub async fn start_scan(state: State<'_, AppState>) -> Result<Value, String> {
    let mut guard = state.engine.lock().await;
    if guard.is_none() {
        let cfg = {
            let s = state.settings.lock().unwrap_or_else(|p| p.into_inner());
            s.to_engine_config(state.db_path.clone())
        };
        let engine = Engine::start(cfg).await.map_err(|e| e.to_string())?;
        *guard = Some(engine);
    }
    Ok(json!({ "scanning": true }))
}

/// Taramayı durdur (çekirdeği bırak).
#[tauri::command]
pub async fn stop_scan(state: State<'_, AppState>) -> Result<Value, String> {
    let mut guard = state.engine.lock().await;
    *guard = None; // drop → tüm görevler durur
    Ok(json!({ "scanning": false }))
}

/// Mevcut ayarlar.
#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> Value {
    let s = state
        .settings
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    serde_json::to_value(s).unwrap_or(Value::Null)
}

/// Ayarları güncelle, kaydet, autostart uygula ve (tarama açıksa) çekirdeği yeniden başlat.
#[tauri::command]
pub async fn set_settings(state: State<'_, AppState>, settings: Settings) -> Result<Value, String> {
    {
        let mut s = state.settings.lock().unwrap_or_else(|p| p.into_inner());
        *s = settings.clone();
        let _ = s.save();
    }
    let _ = autostart::set(settings.autostart);

    // Semantik ayarları anında uygula (aç/kapa/kademe değişimi; yeniden başlatma yok).
    state.semantic.apply(state.store.clone(), &settings).await;

    // Tarama açıksa yeni ayarlarla yeniden başlat. Yeni çekirdeği bırakmadan-ÖNCE
    // ayağa kaldır: başlatma başarısız olursa eski tarama çalışmaya devam etsin.
    // (db_path açılışta sabitlendiğinden değişse bile etkin depo tutarlı kalır;
    // yeni yol yeniden başlatınca geçerli olur.)
    let mut guard = state.engine.lock().await;
    if guard.is_some() {
        let cfg = settings.to_engine_config(state.db_path.clone());
        let new_engine = Engine::start(cfg).await.map_err(|e| e.to_string())?;
        *guard = Some(new_engine); // eski çekirdek burada drop olur
    }
    Ok(json!({ "ok": true }))
}

/// Yalnızca başlangıçta-başlat ayarını değiştir.
#[tauri::command]
pub fn set_autostart(state: State<'_, AppState>, enabled: bool) -> Result<Value, String> {
    autostart::set(enabled).map_err(|e| e.to_string())?;
    let mut s = state.settings.lock().unwrap_or_else(|p| p.into_inner());
    s.autostart = enabled;
    let _ = s.save();
    Ok(json!({ "autostart": enabled }))
}

/// Güncelleme var mı?
#[tauri::command]
pub async fn check_update() -> Result<Value, String> {
    let r = tauri::async_runtime::spawn_blocking(updater::check)
        .await
        .map_err(|e| e.to_string())?;
    match r {
        Ok(Some(i)) => Ok(json!({ "available": true, "version": i.version, "notes": i.notes })),
        Ok(None) => Ok(json!({ "available": false })),
        Err(e) => Ok(json!({ "available": false, "error": e })),
    }
}

/// Güncellemeyi indir, imzayı doğrula, exe'yi değiştir ve yeniden başlat.
#[tauri::command]
pub async fn install_update(app: AppHandle) -> Result<Value, String> {
    let info = tauri::async_runtime::spawn_blocking(updater::check)
        .await
        .map_err(|e| e.to_string())??
        .ok_or_else(|| "güncelleme bulunamadı".to_string())?;
    tauri::async_runtime::spawn_blocking(move || updater::install(&info))
        .await
        .map_err(|e| e.to_string())??;

    // Yeni exe'yi başlat ve bu süreçten çık.
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
    app.exit(0);
    Ok(json!({ "restarted": true }))
}
