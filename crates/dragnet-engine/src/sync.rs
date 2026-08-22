// SPDX-License-Identifier: AGPL-3.0-only
//! Uzak indeks senkronizasyonu — sunucudaki `/changes` akışını yerel depoya aktarır.
//!
//! Dragnet'in üç çalışma modu vardır (bkz. [`SyncMode`]):
//! - **Yalnız yerel:** klasik davranış; her düğüm kendi DHT taramasını yapar.
//! - **Yalnız uzak:** hiç taranmaz, indeks sunucudan çekilir. Zayıf makineler ya da
//!   crawler trafiğini istemeyen ağlar için.
//! - **Hibrit:** ikisi birden. Aynı infohash iki kaynaktan gelirse `upsert_torrent`
//!   birleştirir (idempotent), dolayısıyla çakışma diye bir sorun yoktur.
//!
//! Neden sunucu: DHT taraması ancak **kesintisiz** çalıştığında verimlidir — ağın
//! yönlendirme tablolarında yer edinmek saatler alır ve her yeniden başlatma bu birikimi
//! zayıflatır (ölçüm: `docs/CEKIM-HIZI.md` §12). 7/24 çalışan tek bir düğüm, aralıklı
//! çalışan çok sayıda düğümden daha fazla ad üretir; istemciler de sonucu paylaşır.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use dragnet_core::TorrentRecord;
use dragnet_store::Store;

/// Uygulamanın indeksi nereden beslediği.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyncMode {
    /// Yalnız kendi DHT taraması (klasik davranış).
    #[default]
    Local,
    /// Yalnız uzak sunucu; yerel tarama çalışmaz.
    Remote,
    /// Hem uzak senkronizasyon hem kendi taraması.
    Hybrid,
}

impl SyncMode {
    /// Ayar dosyasındaki metinden çözer (bilinmeyen değer → `Local`).
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "remote" | "uzak" => Self::Remote,
            "hybrid" | "hibrit" => Self::Hybrid,
            _ => Self::Local,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
            Self::Hybrid => "hybrid",
        }
    }

    /// Yerel DHT taraması çalışmalı mı?
    pub fn crawls(self) -> bool {
        matches!(self, Self::Local | Self::Hybrid)
    }

    /// Uzak sunucudan çekilmeli mi?
    pub fn syncs(self) -> bool {
        matches!(self, Self::Remote | Self::Hybrid)
    }
}

/// Senkronizasyon yapılandırması.
#[derive(Debug, Clone)]
pub struct SyncConfig {
    /// Sunucu kökü, ör. `https://dragnet.example.com`.
    pub url: String,
    /// Sunucu token ister ise (`Authorization: Bearer …`).
    pub token: Option<String>,
    /// Bir partide istenecek kayıt sayısı (sunucu 1000'de sınırlar).
    pub batch: i64,
    /// Akış tükendiğinde beklenecek süre.
    pub idle: Duration,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            token: None,
            batch: 500,
            // Akış tükenince sunucuyu dövmeye gerek yok: sunucu tarafında yeni ad
            // üretimi saatte birkaç yüz mertebesinde, dolayısıyla dakikalık yoklama
            // fazlasıyla yeterli.
            idle: Duration::from_secs(60),
        }
    }
}

/// Sunucunun `/changes` yanıtı.
#[derive(Debug, Deserialize)]
struct ChangesResponse {
    records: Vec<TorrentRecord>,
    cursor: i64,
    more: bool,
}

/// Senkronizasyon sayaçları (pano/teşhis).
#[derive(Debug, Default)]
pub struct SyncStats {
    /// Uzaktan alınıp yerele yazılan kayıt.
    pub records: std::sync::atomic::AtomicU64,
    /// Başarısız istek (ağ ya da sunucu hatası).
    pub errors: std::sync::atomic::AtomicU64,
    /// En son işlenen imleç.
    pub cursor: std::sync::atomic::AtomicI64,
}

/// Uzak senkronizasyonu başlatır. Görev iptal edilene kadar çalışır.
///
/// İmleç kalıcıdır (`meta` tablosunda): uygulama yeniden başladığında kaldığı yerden
/// devam eder, baştan indirmez.
pub fn spawn(store: Store, cfg: SyncConfig, stats: Arc<SyncStats>) -> JoinHandle<()> {
    tokio::spawn(async move {
        if cfg.url.trim().is_empty() {
            warn!("senkronizasyon açık ama sunucu adresi boş; görev durdu");
            return;
        }
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "HTTP istemcisi kurulamadı; senkronizasyon yok");
                return;
            }
        };
        let base = cfg.url.trim_end_matches('/').to_string();
        let mut cursor = store.sync_cursor().await.unwrap_or(0);
        info!(url = %base, cursor, "uzak senkronizasyon başladı");

        loop {
            match fetch_batch(&client, &base, &cfg, cursor).await {
                Ok(batch) => {
                    let n = batch.records.len();
                    for rec in &batch.records {
                        if let Err(e) = store.upsert_torrent(rec).await {
                            warn!(error = %e, "uzak kayıt yazılamadı");
                        }
                    }
                    if n > 0 {
                        stats
                            .records
                            .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
                        debug!(n, cursor = batch.cursor, "uzak parti işlendi");
                    }
                    // İmleç ancak yazma bittikten SONRA ilerletilir: süreç ortada
                    // kapanırsa parti yeniden çekilir (yinelenen yazma zararsızdır,
                    // `upsert_torrent` idempotenttir) — ama kayıt ATLANMAZ.
                    if batch.cursor > cursor {
                        cursor = batch.cursor;
                        stats
                            .cursor
                            .store(cursor, std::sync::atomic::Ordering::Relaxed);
                        if let Err(e) = store.set_sync_cursor(cursor).await {
                            warn!(error = %e, "senkronizasyon imleci yazılamadı");
                        }
                    }
                    if !batch.more {
                        tokio::time::sleep(cfg.idle).await;
                    }
                }
                Err(e) => {
                    stats
                        .errors
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    debug!(error = %e, "senkronizasyon isteği başarısız");
                    tokio::time::sleep(cfg.idle).await;
                }
            }
        }
    })
}

/// Tek bir `/changes` partisi çeker.
async fn fetch_batch(
    client: &reqwest::Client,
    base: &str,
    cfg: &SyncConfig,
    cursor: i64,
) -> Result<ChangesResponse, String> {
    let url = format!("{base}/changes?since={cursor}&limit={}", cfg.batch);
    let mut req = client.get(&url);
    if let Some(t) = &cfg.token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("sunucu {} döndü", resp.status()));
    }
    resp.json::<ChangesResponse>()
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parse_and_capabilities() {
        assert_eq!(SyncMode::parse("remote"), SyncMode::Remote);
        assert_eq!(SyncMode::parse("hibrit"), SyncMode::Hybrid);
        assert_eq!(SyncMode::parse("saçma"), SyncMode::Local);
        assert_eq!(SyncMode::parse(""), SyncMode::Local);

        // Yalnız uzak modda YEREL TARAMA ÇALIŞMAMALI: kullanıcı bu modu tam da
        // kendi ağını yormamak için seçiyor.
        assert!(!SyncMode::Remote.crawls());
        assert!(SyncMode::Remote.syncs());
        // Yalnız yerel modda sunucuya hiç istek gitmemeli.
        assert!(SyncMode::Local.crawls());
        assert!(!SyncMode::Local.syncs());
        // Hibrit ikisini de yapar.
        assert!(SyncMode::Hybrid.crawls());
        assert!(SyncMode::Hybrid.syncs());
    }
}
