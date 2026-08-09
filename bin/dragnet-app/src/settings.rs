// SPDX-License-Identifier: AGPL-3.0-only
//! Uygulama ayarları — exe yanında `dragnet-settings.json` olarak saklanır (taşınabilir).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use dragnet_engine::EngineConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub db_path: String,
    pub api_bind: String,
    pub harvester_port: u16,
    pub harvester_max_queries_per_sec: f64,
    pub fetch_workers: usize,
    pub fetch_peer_concurrency: usize,
    /// Windows'ta başlangıçta başlat.
    pub autostart: bool,
    /// Uygulama açılınca taramayı otomatik başlat.
    pub auto_scan: bool,
    pub seed_infohashes: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            db_path: "dragnet.db".to_string(),
            api_bind: "127.0.0.1:8080".to_string(),
            harvester_port: 0,
            // Nazik varsayılan (router/internet dostu).
            harvester_max_queries_per_sec: 50.0,
            fetch_workers: 5,
            fetch_peer_concurrency: 6,
            autostart: false,
            auto_scan: true,
            seed_infohashes: vec![
                "08ada5a7a6183aae1e09d831df6748d566095a10".to_string(), // Sintel
                "dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c".to_string(), // Big Buck Bunny
                "209c8226b299b308beaf2b9cd3fb49212dbd13ec".to_string(), // Tears of Steel
            ],
        }
    }
}

/// Exe'nin bulunduğu dizin (taşınabilir veri konumu).
fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn settings_path() -> PathBuf {
    exe_dir().join("dragnet-settings.json")
}

impl Settings {
    pub fn load() -> Self {
        std::fs::read_to_string(settings_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self).unwrap_or_default();
        std::fs::write(settings_path(), json)
    }

    /// Göreli db yolunu exe dizinine göre mutlaklaştırır.
    fn resolved_db_path(&self) -> String {
        let p = PathBuf::from(&self.db_path);
        if p.is_absolute() {
            self.db_path.clone()
        } else {
            exe_dir().join(p).to_string_lossy().into_owned()
        }
    }

    pub fn to_engine_config(&self) -> Result<EngineConfig, String> {
        let api_bind = self
            .api_bind
            .parse()
            .map_err(|e| format!("geçersiz api_bind: {e}"))?;
        Ok(EngineConfig {
            db_path: self.resolved_db_path(),
            api_bind,
            api_token: None,
            harvester_port: self.harvester_port,
            harvester_max_queries_per_sec: self.harvester_max_queries_per_sec,
            fetch_workers: self.fetch_workers,
            fetch_peer_concurrency: self.fetch_peer_concurrency,
            seed_infohashes: self.seed_infohashes.clone(),
        })
    }

    /// Sorgu deposu için mutlak db yolu.
    pub fn db_path_abs(&self) -> String {
        self.resolved_db_path()
    }
}
