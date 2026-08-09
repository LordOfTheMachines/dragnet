// SPDX-License-Identifier: AGPL-3.0-only
//! Frontend'in çağırdığı Tauri komutları.

use std::net::TcpStream;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tauri::{AppHandle, State};

use dragnet_engine::{Engine, TorrentSummary};

use crate::settings::Settings;
use crate::{autostart, updater, AppState};

fn summary_json(s: &TorrentSummary) -> Value {
    json!({
        "infohash": s.infohash.to_hex(),
        "name": s.name,
        "size": s.total_size,
        "files": s.file_count,
        "seen": s.seen_count,
        "magnet": s.infohash.to_magnet(Some(&s.name)),
    })
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
    let (scanning, harvester, addr) = {
        let guard = state.engine.lock().await;
        match guard.as_ref() {
            Some(e) => {
                let snap = e.snapshot().await;
                (true, Some(snap.harvester), Some(e.harvester_addr().to_string()))
            }
            None => (false, None, None),
        }
    };

    let fetched = state.store.count_fetched().await.unwrap_or(0);
    let total = state.store.count_total().await.unwrap_or(0);

    // BEP-51 örnek/sn (delta).
    let samples = harvester.as_ref().map(|h| h.samples_seen).unwrap_or(0);
    let sample_rate = {
        let mut prev = state.rate_prev.lock().unwrap();
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
        "sample_rate": sample_rate,
    }))
}

/// FTS araması.
#[tauri::command]
pub async fn search(state: State<'_, AppState>, query: String, limit: Option<i64>) -> Result<Value, String> {
    let rows = state
        .store
        .search(&query, limit.unwrap_or(50))
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({ "results": summaries_json(&rows) }))
}

/// Dashboard verileri: en çok paylaşılan, en büyük, son eklenen, günlük keşif.
#[tauri::command]
pub async fn dashboard(state: State<'_, AppState>) -> Result<Value, String> {
    let top_seen = state.store.top_by_seen(12).await.map_err(|e| e.to_string())?;
    let top_size = state.store.top_by_size(12).await.map_err(|e| e.to_string())?;
    let recent = state.store.recent(12).await.map_err(|e| e.to_string())?;
    let daily = state.store.daily_discovery(30).await.map_err(|e| e.to_string())?;
    Ok(json!({
        "top_seen": summaries_json(&top_seen),
        "top_size": summaries_json(&top_size),
        "recent": summaries_json(&recent),
        "daily": daily.iter().map(|(d, n)| json!({ "day": d, "count": n })).collect::<Vec<_>>(),
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
            let s = state.settings.lock().unwrap();
            s.to_engine_config()?
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
    let s = state.settings.lock().unwrap().clone();
    serde_json::to_value(s).unwrap_or(Value::Null)
}

/// Ayarları güncelle, kaydet, autostart uygula ve (tarama açıksa) çekirdeği yeniden başlat.
#[tauri::command]
pub async fn set_settings(state: State<'_, AppState>, settings: Settings) -> Result<Value, String> {
    {
        let mut s = state.settings.lock().unwrap();
        *s = settings.clone();
        let _ = s.save();
    }
    let _ = autostart::set(settings.autostart);

    // Tarama açıksa yeni ayarlarla yeniden başlat.
    let mut guard = state.engine.lock().await;
    if guard.is_some() {
        *guard = None;
        let cfg = settings.to_engine_config()?;
        *guard = Some(Engine::start(cfg).await.map_err(|e| e.to_string())?);
    }
    Ok(json!({ "ok": true }))
}

/// Yalnızca başlangıçta-başlat ayarını değiştir.
#[tauri::command]
pub fn set_autostart(state: State<'_, AppState>, enabled: bool) -> Result<Value, String> {
    autostart::set(enabled).map_err(|e| e.to_string())?;
    let mut s = state.settings.lock().unwrap();
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
