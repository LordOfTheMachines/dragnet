// SPDX-License-Identifier: AGPL-3.0-only
//! dragnetd yapılandırması.
//!
//! Katmanlı: gömülü varsayılanlar → `dragnetd.toml` (varsa) → `DRAGNET_` env
//! değişkenleri. Örn. `DRAGNET_API_BIND=0.0.0.0:9000 dragnetd`.

use figment::providers::{Env, Format, Serialized, Toml};
use figment::Figment;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// SQLite veritabanı dosyası.
    pub db_path: String,
    /// Arama API'sinin dinleyeceği adres.
    pub api_bind: String,
    /// Ayarlanırsa API `/search` ve `/stats` için bearer token ister.
    pub api_token: Option<String>,
    /// DHT harvester UDP portu (0 = efemer).
    pub harvester_port: u16,
    /// Harvester'ın saniyedeki azami giden DHT sorgusu. **İnternetin kilitleniyorsa
    /// bu değeri düşür.** Nazik varsayılan: 50. Port-forward + iyi bağlantıda artırılabilir.
    pub harvester_max_queries_per_sec: f64,
    /// Aynı anda metadata çekilecek azami infohash sayısı (fetcher havuzu).
    pub fetch_workers: usize,
    /// Tek bir metadata çekiminde denenecek eşzamanlı peer sayısı.
    pub fetch_peer_concurrency: usize,
    /// Başlangıçta çekilip indekslenecek infohash'ler (40-hex). İndeksi ısıtmak
    /// veya bilinen torrent'leri sabitlemek için. Varsayılan: boş.
    pub seed_infohashes: Vec<String>,
    /// Semantik (anlamsal) arama — opt-in (Faz D). Açıksa model bir kez indirilir,
    /// indeks arka planda kurulur, `/search` varsayılan olarak hibrit çalışır.
    pub semantic_enabled: bool,
    /// Kademe: `light` | `balanced` | `quality`.
    pub semantic_tier: String,
    /// Cihaz: `auto` | `gpu` | `cpu`.
    pub semantic_device: String,
    /// Model dizini (kısa/düz bir yol; varsayılan `models`).
    pub semantic_models_dir: String,
    /// Cross-encoder yeniden sıralayıcı (bge-reranker-v2-m3, ~570 MB, CPU).
    pub semantic_rerank: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            db_path: "dragnet.db".to_string(),
            api_bind: "127.0.0.1:8080".to_string(),
            api_token: None,
            harvester_port: 0,
            harvester_max_queries_per_sec: 50.0,
            fetch_workers: 12,
            fetch_peer_concurrency: 12,
            seed_infohashes: Vec::new(),
            semantic_enabled: false,
            semantic_tier: "quality".to_string(),
            semantic_device: "auto".to_string(),
            semantic_models_dir: "models".to_string(),
            semantic_rerank: true,
        }
    }
}

impl Config {
    /// Katmanlı yapılandırmayı yükler: varsayılanlar → `dragnetd.toml` →
    /// (verilmişse) `extra_path` → `DRAGNET_` env değişkenleri.
    pub fn load(extra_path: Option<&str>) -> Result<Self, Box<figment::Error>> {
        let mut fig = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::file("dragnetd.toml"));
        if let Some(path) = extra_path {
            fig = fig.merge(Toml::file(path));
        }
        fig.merge(Env::prefixed("DRAGNET_"))
            .extract()
            .map_err(Box::new)
    }
}
