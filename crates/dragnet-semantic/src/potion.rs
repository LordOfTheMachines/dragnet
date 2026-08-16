// SPDX-License-Identifier: AGPL-3.0-only
//! model2vec (statik embedding) motoru — "hafif" kademe. Saf Rust; 500k adı ~10 sn'de
//! embed eder, bu yüzden model değişiminde anında yeniden indeksleme mümkündür.

use std::path::Path;

use model2vec_rs::model::StaticModel;
use tracing::info;

use crate::embedder::Embedder;
use crate::models::ModelSpec;
use crate::quant::l2_normalize;
use crate::SemanticError;

pub struct PotionEmbedder {
    spec: &'static ModelSpec,
    model: StaticModel,
}

impl PotionEmbedder {
    pub fn load(spec: &'static ModelSpec, models_dir: &Path) -> Result<Self, SemanticError> {
        if !spec.is_downloaded(models_dir) {
            return Err(SemanticError::NotDownloaded(spec.id.to_string()));
        }
        let dir = spec.dir(models_dir);
        let model = StaticModel::from_pretrained(&dir, None, Some(true), None)
            .map_err(|e| SemanticError::Model(format!("{}: {e}", spec.id)))?;
        // Boyut doğrulaması: tek bir metin embed edip uzunluğa bak.
        let probe = model.encode(&["probe".to_string()]);
        let dim = probe.first().map(|v| v.len()).unwrap_or(0);
        if dim != spec.dim {
            return Err(SemanticError::Model(format!(
                "{}: boyut {dim} != {}",
                spec.id, spec.dim
            )));
        }
        info!(model = spec.id, dim, "model2vec embedder yüklendi");
        Ok(Self { spec, model })
    }

    fn encode(&self, texts: &[String]) -> Vec<Vec<f32>> {
        let mut out = self.model.encode(texts);
        for v in out.iter_mut() {
            l2_normalize(v);
        }
        out
    }
}

impl Embedder for PotionEmbedder {
    fn model_id(&self) -> &str {
        self.spec.id
    }
    fn dim(&self) -> usize {
        self.spec.dim
    }
    fn device(&self) -> &str {
        "cpu"
    }
    fn embed_docs(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, SemanticError> {
        Ok(self.encode(texts))
    }
    fn embed_query(&self, text: &str) -> Result<Vec<f32>, SemanticError> {
        self.encode(&[text.to_string()])
            .pop()
            .ok_or_else(|| SemanticError::Model("boş çıktı".into()))
    }
}
