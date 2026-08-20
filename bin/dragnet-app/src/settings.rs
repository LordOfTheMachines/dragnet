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
    /// Gelişmiş içerik filtresi: adı bu kelimelerden birini (küçük harfe duyarsız,
    /// alt-dize) içeren torrent'ler arama/gözat sonuçlarında gizlenir. Yıkıcı değil.
    #[serde(default)]
    pub block_keywords: Vec<String>,
    /// Semantik (anlamsal) arama — opt-in. Açılınca model indirilir (bir kez), indeks
    /// arka planda kurulur; kapalıyken davranış birebir eski FTS.
    #[serde(default)]
    pub semantic_enabled: bool,
    /// Kademe: `auto` (donanıma göre) | `light` | `balanced` | `quality` (bkz. ARCHITECTURE §7.3).
    #[serde(default = "default_tier")]
    pub semantic_tier: String,
    /// Cihaz: `auto` | `gpu` | `cpu`.
    #[serde(default = "default_device")]
    pub semantic_device: String,
    /// Cross-encoder yeniden sıralayıcı (bge-reranker-v2-m3, ~570 MB, CPU): ilk 30 adayı
    /// sorguyla birlikte puanlar; daha isabetli, sorgu başına ~0,25 s ek gecikme.
    #[serde(default = "default_true")]
    pub semantic_rerank: bool,
    /// Model dizini (boş = exe yanında `models`). Kısa/düz bir yol olmalı.
    #[serde(default)]
    pub semantic_models_dir: String,
    /// Kaç DHT düğüm kimliğiyle dinlensin (1-8). BEP-42 bir IP için 8 geçerli kimliğe
    /// izin verir; her kimlik `harvester_port + i` portunda dinler. Pasif hasat kimlik
    /// sayısıyla ölçeklenir — ama her portun modemde yönlendirilmesi gerekir.
    #[serde(default = "default_instances")]
    pub harvester_instances: usize,
    /// Veritabanı bütçesi (GB; 0 = sınırsız). Aşılınca **büyüme durur**, arama sürer.
    #[serde(default)]
    pub db_max_gb: f64,
    /// Diskte bırakılacak boş alan rezervi (GB; 0 = kapalı). Altına inilirse büyüme durur.
    #[serde(default = "default_disk_reserve")]
    pub disk_reserve_gb: f64,
}

/// Varsayılan disk rezervi: 5 GB. Crawler süresiz büyür; diski tamamen doldurup
/// sistemi zora sokmasın (F8-4).
/// Varsayılan düğüm kimliği sayısı: 1 (tek port). Kullanıcı modeminde ek portları
/// yönlendirdiyse artırabilir.
fn default_instances() -> usize {
    1
}

fn default_disk_reserve() -> f64 {
    5.0
}

fn default_tier() -> String {
    "auto".to_string()
}
fn default_device() -> String {
    "auto".to_string()
}
fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            db_path: "dragnet.db".to_string(),
            api_bind: "127.0.0.1:8080".to_string(),
            harvester_port: 6881,
            // Nazik varsayılan (router/internet dostu).
            harvester_max_queries_per_sec: 50.0,
            // Ölçüm (2026-08-20): 12 işçi ile saatte ~165 ad. Deneme başına ort. 3,1 sn ve
            // başarı ~%2 olduğu için isim üretimi doğrudan eşzamanlılıkla ölçekleniyor.
            fetch_workers: 24,
            fetch_peer_concurrency: 16,
            autostart: false,
            auto_scan: true,
            block_keywords: Vec::new(),
            semantic_enabled: false,
            semantic_tier: default_tier(),
            harvester_instances: default_instances(),
            db_max_gb: 0.0,
            disk_reserve_gb: default_disk_reserve(),
            semantic_device: default_device(),
            semantic_rerank: true,
            semantic_models_dir: String::new(),
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
        // Serileştirme hatasında dosyayı ASLA boş yazma (veri kaybı) — hatayı yükselt.
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        // Atomik yaz: önce geçici dosyaya, sonra yerine taşı — yarıda kesilirse
        // eski ayar bozulmadan kalır.
        let path = settings_path();
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)
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

    /// Motor yapılandırması. `db_path` çağırandan gelir (uygulama açılışta sabitler;
    /// çalışan depo/API ile tutarlılığı korur — ayar değişse de yeniden başlatana dek
    /// aynı dosyaya yazılır). API alanı YOK: arama API'si çekirdekten ayrı sunulur.
    pub fn to_engine_config(&self, db_path: String) -> EngineConfig {
        EngineConfig {
            db_path,
            harvester_port: self.harvester_port,
            harvester_max_queries_per_sec: self.harvester_max_queries_per_sec,
            fetch_workers: self.fetch_workers,
            fetch_peer_concurrency: self.fetch_peer_concurrency,
            seed_infohashes: self.seed_infohashes.clone(),
            harvester_instances: self.harvester_instances.clamp(1, 8),
            db_max_bytes: self.storage_limits().0,
            disk_reserve_bytes: self.storage_limits().1,
        }
    }

    /// Arama API'sinin dinleyeceği adresi çözer.
    pub fn api_addr(&self) -> Result<std::net::SocketAddr, String> {
        self.api_bind
            .parse()
            .map_err(|e| format!("geçersiz api_bind: {e}"))
    }

    /// Depolama sınırları (bayt): (veritabanı bütçesi, disk rezervi). 0 = sınırsız.
    pub fn storage_limits(&self) -> (u64, u64) {
        const GB: f64 = 1_073_741_824.0;
        (
            (self.db_max_gb.max(0.0) * GB) as u64,
            (self.disk_reserve_gb.max(0.0) * GB) as u64,
        )
    }

    /// Sorgu deposu için mutlak db yolu.
    pub fn db_path_abs(&self) -> String {
        self.resolved_db_path()
    }

    /// Model dizini (mutlak). Boşsa exe yanında `models`.
    pub fn models_dir_abs(&self) -> PathBuf {
        let p = PathBuf::from(self.semantic_models_dir.trim());
        if self.semantic_models_dir.trim().is_empty() {
            exe_dir().join("models")
        } else if p.is_absolute() {
            p
        } else {
            exe_dir().join(p)
        }
    }

    /// Semantik katman yapılandırması.
    pub fn semantic_config(&self) -> dragnet_semantic::SemanticConfig {
        dragnet_semantic::SemanticConfig {
            tier: dragnet_semantic::Tier::parse(&self.semantic_tier),
            device: dragnet_semantic::Device::parse(&self.semantic_device),
            models_dir: self.models_dir_abs(),
        }
    }
}
