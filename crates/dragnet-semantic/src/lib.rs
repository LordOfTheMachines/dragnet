// SPDX-License-Identifier: AGPL-3.0-only
//! dragnet-semantic — yerel (çevrimdışı) embedding + anlamsal arama katmanı (Faz D).
//!
//! Bileşenler:
//! - [`Embedder`] trait'i ve motorlar: [`potion::PotionEmbedder`] (model2vec, hafif),
//!   [`onnx::OrtEmbedder`] (MiniLM / EmbeddingGemma; CPU ya da DirectML).
//! - [`models`]: 3 kademeli model kataloğu ([`Tier`]) + bir kerelik indirme.
//! - [`index::VecIndex`]: bellek-içi int8 brute-force kosinüs indeksi.
//! - [`hybrid::rrf`]: FTS + semantik aday listelerini harmanlama.
//! - [`Semantic`]: hepsini saran, `Arc` ile paylaşılan cephe.
//!
//! Kalıcılık bu crate'in işi değil: nicemlenmiş vektörler `dragnet-store`'da
//! (`torrent_embeddings`) saklanır; açılışta [`VecIndex`]'e yüklenir. Bu crate yalnız
//! `dragnet-core`'a bağımlıdır. Karar gerekçeleri: `docs/ARCHITECTURE.md` §7.3.

pub mod embedder;
pub mod hybrid;
pub mod index;
pub mod models;
pub mod onnx;
pub mod potion;
pub mod quant;
pub mod text;

use std::path::PathBuf;
use std::sync::RwLock;

use dragnet_core::InfoHash;

pub use embedder::{Embedder, MockEmbedder};
pub use index::{Hit, VecIndex};
pub use models::{Device, ModelSpec, Tier};

/// Semantik katman hataları.
#[derive(Debug, thiserror::Error)]
pub enum SemanticError {
    #[error("G/Ç hatası: {0}")]
    Io(#[from] std::io::Error),
    #[error("indirme hatası: {0}")]
    Http(String),
    #[error("model indirilmemiş: {0}")]
    NotDownloaded(String),
    #[error("model hatası: {0}")]
    Model(String),
    #[error("tokenizer hatası: {0}")]
    Tokenizer(String),
    #[error("onnxruntime hatası: {0}")]
    Ort(String),
}

/// Semantik katman yapılandırması.
#[derive(Debug, Clone)]
pub struct SemanticConfig {
    pub tier: Tier,
    pub device: Device,
    /// Modellerin indirildiği kök dizin (kısa/düz tutulmalı — bkz. `models`).
    pub models_dir: PathBuf,
}

/// Anlık durum (UI/`/stats` için).
#[derive(Debug, Clone, serde::Serialize)]
pub struct SemanticStatus {
    pub model_id: String,
    pub tier: String,
    pub device: String,
    pub dim: usize,
    pub indexed: usize,
    pub index_bytes: usize,
}

/// Motor + bellek-içi indeks. Engine (indeksleme), API ve uygulama (sorgu) arasında
/// `Arc<Semantic>` olarak paylaşılır; tarama kapalıyken de arama çalışır.
pub struct Semantic {
    embedder: Box<dyn Embedder>,
    index: RwLock<VecIndex>,
    tier: Tier,
    min_score: f32,
}

impl Semantic {
    /// Yapılandırmadaki kademenin modelini yükler (indirilmiş olmalı — bkz.
    /// [`Semantic::ensure_model`]). Bloklar (model yükleme saniyeler sürebilir);
    /// `spawn_blocking` ile çağır.
    pub fn load(cfg: &SemanticConfig) -> Result<Self, SemanticError> {
        let spec = cfg.tier.spec();
        let embedder: Box<dyn Embedder> = match spec.engine {
            models::Engine::Model2Vec => {
                Box::new(potion::PotionEmbedder::load(spec, &cfg.models_dir)?)
            }
            models::Engine::Onnx => {
                Box::new(onnx::OrtEmbedder::load(spec, &cfg.models_dir, cfg.device)?)
            }
        };
        Ok(Self::with_embedder(embedder, cfg.tier, spec.min_score))
    }

    /// Verilen motorla kurar (testler `MockEmbedder` ile).
    pub fn with_embedder(embedder: Box<dyn Embedder>, tier: Tier, min_score: f32) -> Self {
        let dim = embedder.dim();
        Self {
            embedder,
            index: RwLock::new(VecIndex::new(dim)),
            tier,
            min_score,
        }
    }

    /// Kademenin modeli indirilmiş mi?
    pub fn is_model_ready(cfg: &SemanticConfig) -> bool {
        cfg.tier.spec().is_downloaded(&cfg.models_dir)
    }

    /// Eksik model dosyalarını indirir (bloklar). İlerleme: `(dosya, indirilen, toplam)`.
    pub fn ensure_model(
        cfg: &SemanticConfig,
        progress: models::Progress<'_>,
    ) -> Result<(), SemanticError> {
        let spec = cfg.tier.spec();
        if spec.is_downloaded(&cfg.models_dir) {
            return Ok(());
        }
        models::download(spec, &cfg.models_dir, progress)
    }

    pub fn embedder(&self) -> &dyn Embedder {
        self.embedder.as_ref()
    }
    pub fn index(&self) -> &RwLock<VecIndex> {
        &self.index
    }
    pub fn tier(&self) -> Tier {
        self.tier
    }
    pub fn model_id(&self) -> &str {
        self.embedder.model_id()
    }
    pub fn dim(&self) -> usize {
        self.embedder.dim()
    }
    pub fn device(&self) -> &str {
        self.embedder.device()
    }
    pub fn min_score(&self) -> f32 {
        self.min_score
    }

    pub fn status(&self) -> SemanticStatus {
        let idx = self.index.read().unwrap_or_else(|p| p.into_inner());
        SemanticStatus {
            model_id: self.model_id().to_string(),
            tier: self.tier.as_str().to_string(),
            device: self.device().to_string(),
            dim: self.dim(),
            indexed: idx.len(),
            index_bytes: idx.memory_bytes(),
        }
    }

    /// Adları normalize edip embed eder ve indekse ekler. Kalıcılaştırılacak
    /// `(infohash, q_int8, scale)` üçlülerini döner. Bloklar (`spawn_blocking`).
    pub fn embed_and_add(
        &self,
        items: &[(InfoHash, String)],
    ) -> Result<Vec<(InfoHash, Vec<i8>, f32)>, SemanticError> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        let texts: Vec<String> = items.iter().map(|(_, n)| text::normalize_name(n)).collect();
        let vecs = self.embedder.embed_docs(&texts)?;
        let mut idx = self.index.write().unwrap_or_else(|p| p.into_inner());
        let mut out = Vec::with_capacity(items.len());
        for ((ih, _), v) in items.iter().zip(vecs) {
            let (q, s) = idx.add(*ih, &v);
            out.push((*ih, q, s));
        }
        Ok(out)
    }

    /// Doğal dil sorgusu → en yakın `k` kayıt (modelin `min_score` eşiği uygulanır).
    pub fn search(&self, query: &str, k: usize) -> Result<Vec<Hit>, SemanticError> {
        let q = query.trim();
        if q.is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let v = self.embedder.embed_query(q)?;
        let idx = self.index.read().unwrap_or_else(|p| p.into_inner());
        Ok(idx.search(&v, k, self.min_score))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ih(n: u8) -> InfoHash {
        InfoHash::from_bytes([n; 20])
    }

    #[test]
    fn facade_embed_add_search_with_mock() {
        let sem = Semantic::with_embedder(Box::new(MockEmbedder::new(64)), Tier::Light, 0.0);
        let rows = sem
            .embed_and_add(&[
                (ih(1), "The.Matrix.Reloaded.2003.1080p".into()),
                (ih(2), "ubuntu-24.04-desktop-amd64.iso".into()),
                (ih(3), "The.Matrix.1999.REMASTERED".into()),
            ])
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|(_, q, s)| q.len() == 64 && *s > 0.0));
        let hits = sem.search("matrix", 2).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits
            .iter()
            .all(|h| h.infohash == ih(1) || h.infohash == ih(3)));
        assert!(sem.search("   ", 5).unwrap().is_empty());
        let st = sem.status();
        assert_eq!(st.indexed, 3);
        assert_eq!(st.model_id, "mock");
    }

    /// Gerçek model gerekir: `DRAGNET_MODELS_DIR` altına indirilmiş `quality` kademesi.
    /// `cargo test -p dragnet-semantic -- --ignored` ile çalışır.
    #[test]
    #[ignore]
    fn real_model_smoke() {
        let dir = std::env::var("DRAGNET_MODELS_DIR")
            .unwrap_or_else(|_| "C:/dgcache/dragnet-models".into());
        let cfg = SemanticConfig {
            tier: Tier::parse(&std::env::var("DRAGNET_TIER").unwrap_or_default()),
            device: Device::Auto,
            models_dir: dir.into(),
        };
        Semantic::ensure_model(&cfg, &|f, d, t| eprintln!("{f}: {d}/{t}")).unwrap();
        let sem = Semantic::load(&cfg).unwrap();
        eprintln!(
            "model={} device={} dim={}",
            sem.model_id(),
            sem.device(),
            sem.dim()
        );
        sem.embed_and_add(&[
            (ih(1), "The.Matrix.Reloaded.2003.1080p.BluRay.x264".into()),
            (ih(2), "ubuntu-24.04.2-desktop-amd64.iso".into()),
            (ih(3), "Pride.and.Prejudice.2005.1080p".into()),
            (ih(4), "Pink.Floyd.The.Wall.1979.FLAC".into()),
        ])
        .unwrap();
        let top = sem.search("matriks filmi", 1).unwrap();
        assert_eq!(top[0].infohash, ih(1), "{top:?}");
        let top = sem.search("linux işletim sistemi", 1).unwrap();
        assert_eq!(top[0].infohash, ih(2), "{top:?}");
    }
}
