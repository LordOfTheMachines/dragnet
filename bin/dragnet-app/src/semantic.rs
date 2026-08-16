// SPDX-License-Identifier: AGPL-3.0-only
//! Semantik aramanın uygulama-içi yaşam döngüsü: ayarlara göre aç/kapa, modeli (bir kez)
//! indir, yükle, kalıcı indeksi RAM'e al, arka plan indeksleyiciyi başlat; durumu UI'ya
//! raporla. Aç/kapa **yeniden başlatma gerektirmez** — `SemanticSlot` anında güncellenir.

use std::sync::{Arc, Mutex as StdMutex};

use serde::Serialize;
use tokio::task::JoinHandle;
use tracing::{error, info};

use dragnet_api::SemanticSlot;
use dragnet_engine::semantic_indexer;
use dragnet_semantic::Semantic;
use dragnet_store::Store;

use crate::settings::Settings;

/// UI'ya gösterilen aşama.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Off,
    Downloading,
    Loading,
    Ready,
    Error,
}

/// UI durumu (ilerleme + hata metni). `Semantic::status()` ile birleştirilerek sunulur.
#[derive(Debug, Clone, Serialize)]
pub struct UiState {
    pub phase: Phase,
    pub file: String,
    pub done: u64,
    pub total: u64,
    pub error: String,
    /// Yüklenen yapılandırmanın anahtarı (kademe+cihaz+dizin) — değişim tespiti.
    #[serde(skip)]
    pub key: String,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            phase: Phase::Off,
            file: String::new(),
            done: 0,
            total: 0,
            error: String::new(),
            key: String::new(),
        }
    }
}

/// Uygulamanın semantik yöneticisi.
pub struct SemanticManager {
    pub slot: SemanticSlot,
    pub ui: Arc<StdMutex<UiState>>,
    indexer: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    worker: tokio::sync::Mutex<Option<JoinHandle<()>>>,
}

fn cfg_key(s: &Settings) -> String {
    format!(
        "{}|{}|{}",
        s.semantic_tier.trim().to_lowercase(),
        s.semantic_device.trim().to_lowercase(),
        s.models_dir_abs().display()
    )
}

impl SemanticManager {
    pub fn new() -> Self {
        Self {
            slot: dragnet_api::search::empty_slot(),
            ui: Arc::new(StdMutex::new(UiState::default())),
            indexer: tokio::sync::Mutex::new(None),
            worker: tokio::sync::Mutex::new(None),
        }
    }

    pub fn ui_state(&self) -> UiState {
        self.ui.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn set_ui(&self, f: impl FnOnce(&mut UiState)) {
        let mut g = self.ui.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut g);
    }

    /// Ayarları uygular: kapalıysa söker; açıksa (ve yapılandırma değiştiyse) arka planda
    /// indir→yükle→indeksle akışını başlatır. Hemen döner.
    pub async fn apply(self: &Arc<Self>, store: Store, settings: &Settings) {
        // Önceki kurulum çalışanı varsa durdur.
        if let Some(h) = self.worker.lock().await.take() {
            h.abort();
        }
        if !settings.semantic_enabled {
            self.teardown().await;
            return;
        }
        let key = cfg_key(settings);
        {
            let cur = self.ui_state();
            if cur.phase == Phase::Ready && cur.key == key && self.slot.read().await.is_some() {
                return; // aynı yapılandırma zaten çalışıyor
            }
        }
        // Farklı yapılandırma → eskisini sök, yenisini kur.
        self.teardown().await;
        let cfg = settings.semantic_config();
        self.set_ui(|u| {
            *u = UiState {
                phase: Phase::Downloading,
                key: key.clone(),
                ..Default::default()
            };
        });
        let me = Arc::clone(self);
        let handle = tokio::spawn(async move {
            // 1) İndir (bloklayıcı, ilerlemeli).
            let ui = Arc::clone(&me.ui);
            let cfg2 = cfg.clone();
            let dl = tokio::task::spawn_blocking(move || {
                Semantic::ensure_model(&cfg2, &|file, done, total| {
                    if let Ok(mut g) = ui.lock() {
                        g.file = file.to_string();
                        g.done = done;
                        g.total = total;
                    }
                })
            })
            .await;
            if let Err(e) = flatten(dl) {
                error!(error = %e, "model indirilemedi");
                me.set_ui(|u| {
                    u.phase = Phase::Error;
                    u.error = format!("Model indirilemedi: {e}");
                });
                return;
            }
            // 2) Yükle.
            me.set_ui(|u| {
                u.phase = Phase::Loading;
                u.file.clear();
            });
            let cfg3 = cfg.clone();
            let loaded = tokio::task::spawn_blocking(move || Semantic::load(&cfg3)).await;
            let sem = match flatten(loaded) {
                Ok(s) => Arc::new(s),
                Err(e) => {
                    error!(error = %e, "semantik model yüklenemedi");
                    me.set_ui(|u| {
                        u.phase = Phase::Error;
                        u.error = format!("Model yüklenemedi: {e}");
                    });
                    return;
                }
            };
            // 3) Kalıcı indeksi RAM'e al, yuvaya tak, indeksleyiciyi başlat.
            if let Err(e) = semantic_indexer::load_index(&store, &sem).await {
                error!(error = %e, "semantik indeks yüklenemedi");
                me.set_ui(|u| {
                    u.phase = Phase::Error;
                    u.error = format!("İndeks yüklenemedi: {e}");
                });
                return;
            }
            *me.slot.write().await = Some(Arc::clone(&sem));
            *me.indexer.lock().await =
                Some(semantic_indexer::spawn_indexer(store, Arc::clone(&sem)));
            info!(
                model = sem.model_id(),
                device = sem.device(),
                "semantik arama hazır"
            );
            me.set_ui(|u| {
                u.phase = Phase::Ready;
            });
        });
        *self.worker.lock().await = Some(handle);
    }

    /// Her şeyi söker (indeksleyiciyi durdur, yuvayı boşalt → RAM/VRAM iade).
    pub async fn teardown(&self) {
        if let Some(h) = self.indexer.lock().await.take() {
            h.abort();
        }
        *self.slot.write().await = None;
        self.set_ui(|u| *u = UiState::default());
    }

    /// UI için birleşik durum JSON'u.
    pub async fn status_json(&self) -> serde_json::Value {
        let ui = self.ui_state();
        let sem = self.slot.read().await.clone();
        let st = sem.as_ref().map(|s| s.status());
        serde_json::json!({
            "phase": ui.phase,
            "file": ui.file,
            "done": ui.done,
            "total": ui.total,
            "error": ui.error,
            "model": st.as_ref().map(|s| s.model_id.clone()),
            "tier": st.as_ref().map(|s| s.tier.clone()),
            "device": st.as_ref().map(|s| s.device.clone()),
            "dim": st.as_ref().map(|s| s.dim),
            "indexed": st.as_ref().map(|s| s.indexed).unwrap_or(0),
            "index_mb": st.as_ref().map(|s| s.index_bytes / 1_048_576).unwrap_or(0),
        })
    }
}

fn flatten<T>(
    r: Result<Result<T, dragnet_semantic::SemanticError>, tokio::task::JoinError>,
) -> Result<T, String> {
    match r {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(e.to_string()),
        Err(e) => Err(format!("görev çöktü: {e}")),
    }
}
