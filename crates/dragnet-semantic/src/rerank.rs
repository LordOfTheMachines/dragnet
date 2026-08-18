// SPDX-License-Identifier: AGPL-3.0-only
//! Cross-encoder yeniden sıralayıcı (bge-reranker-v2-m3, ONNX). Sorgu ve adayı **birlikte**
//! okuyup tek bir alaka skoru üretir; bi-encoder (embedding) aramanın kaçırdığı ince ayrımı
//! ("zombi oyunu" mu "zombi filmi" mi, TR sorgu ↔ EN başlık) yakalar. Pahalıdır (çift
//! başına bir ileri geçiş) → yalnız ilk N aday (varsayılan 30) yeniden sıralanır; GPU'da
//! (DirectML) onlarca ms, CPU'da yüzlerce ms.
//!
//! Model: XLM-RoBERTa-large tabanlı, çok dilli (Türkçe dahil), MIT lisans; girdi
//! `<s> sorgu </s></s> aday </s>` (tokenizer çift kodlama), çıktı `logits[b,1]` (sigmoid
//! öncesi; sıralama için ham skor yeterli).

use std::path::Path;
use std::sync::Mutex;

use ndarray::Array2;
use ort::session::{builder::GraphOptimizationLevel, Session, SessionInputValue};
use ort::value::Tensor;
use tokenizers::{EncodeInput, Tokenizer, TruncationParams};
use tracing::{info, warn};

use crate::models::{Device, ModelFile};
use crate::SemanticError;

/// Yeniden sıralayıcı model tanımı (tek model; kademe seçimi yok).
pub struct RerankSpec {
    pub id: &'static str,
    pub files: &'static [ModelFile],
    pub onnx_file: &'static str,
    pub max_tokens: usize,
}

pub static BGE_RERANKER_V2_M3: RerankSpec = RerankSpec {
    id: "bge-reranker-v2-m3",
    files: &[
        ModelFile {
            name: "tokenizer.json",
            url: "https://huggingface.co/onnx-community/bge-reranker-v2-m3-ONNX/resolve/main/tokenizer.json",
            approx_bytes: 17_100_000,
        },
        ModelFile {
            name: "model_int8.onnx",
            url: "https://huggingface.co/onnx-community/bge-reranker-v2-m3-ONNX/resolve/main/onnx/model_int8.onnx",
            approx_bytes: 570_000_000,
        },
    ],
    onnx_file: "model_int8.onnx",
    max_tokens: 96,
};

impl RerankSpec {
    pub fn dir(&self, models_dir: &Path) -> std::path::PathBuf {
        models_dir.join(self.id)
    }
    pub fn is_downloaded(&self, models_dir: &Path) -> bool {
        let d = self.dir(models_dir);
        self.files.iter().all(|f| {
            let p = d.join(f.name);
            p.is_file()
                && std::fs::metadata(&p).map(|m| m.len() > 0).unwrap_or(false)
                && !d.join(format!("{}.part", f.name)).exists()
        })
    }
    pub fn approx_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.approx_bytes).sum()
    }
}

/// Yüklü yeniden sıralayıcı.
pub struct Reranker {
    session: Mutex<Session>,
    tokenizer: Tokenizer,
    input_names: Vec<String>,
    device: &'static str,
    max_tokens: usize,
}

impl Reranker {
    /// Modeli yükler (indirilmiş olmalı: [`crate::models::download`] benzeri —
    /// [`Reranker::ensure_model`]). `Device::Auto` → DirectML dene, olmazsa CPU.
    pub fn load(models_dir: &Path, device: Device) -> Result<Self, SemanticError> {
        let spec = &BGE_RERANKER_V2_M3;
        if !spec.is_downloaded(models_dir) {
            return Err(SemanticError::NotDownloaded(spec.id.to_string()));
        }
        let dir = spec.dir(models_dir);
        let mut tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|e| SemanticError::Tokenizer(e.to_string()))?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: spec.max_tokens,
                ..Default::default()
            }))
            .map_err(|e| SemanticError::Tokenizer(e.to_string()))?;
        tokenizer.with_padding(None);
        let model_path = dir.join(spec.onnx_file);
        let (session, dev) = match device {
            Device::Cpu => (build_session(&model_path, false)?, "cpu"),
            Device::Gpu => (build_session(&model_path, true)?, "directml"),
            Device::Auto => match build_session(&model_path, true) {
                Ok(s) => (s, "directml"),
                Err(e) => {
                    warn!(error = %e, "reranker: DirectML kullanılamadı, CPU'ya düşülüyor");
                    (build_session(&model_path, false)?, "cpu")
                }
            },
        };
        let input_names: Vec<String> = session
            .inputs()
            .iter()
            .map(|o| o.name().to_string())
            .collect();
        info!(model = spec.id, device = dev, "reranker yüklendi");
        Ok(Self {
            session: Mutex::new(session),
            tokenizer,
            input_names,
            device: dev,
            max_tokens: spec.max_tokens,
        })
    }

    /// Eksik dosyaları indirir (bloklar).
    pub fn ensure_model(
        models_dir: &Path,
        progress: crate::models::Progress<'_>,
    ) -> Result<(), SemanticError> {
        let spec = &BGE_RERANKER_V2_M3;
        if spec.is_downloaded(models_dir) {
            return Ok(());
        }
        crate::models::download_files(spec.id, spec.files, models_dir, progress)
    }

    pub fn device(&self) -> &str {
        self.device
    }
    pub fn model_id(&self) -> &'static str {
        BGE_RERANKER_V2_M3.id
    }

    /// Her aday için alaka skoru (ham logit; büyük = daha alakalı). Sıra korunur.
    /// Bloklar — `spawn_blocking` ile çağır.
    pub fn score(&self, query: &str, docs: &[String]) -> Result<Vec<f32>, SemanticError> {
        if docs.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(docs.len());
        // Parti: GPU'da 16, CPU'da 8 çift.
        let bs = if self.device == "directml" { 16 } else { 8 };
        for chunk in docs.chunks(bs) {
            let pairs: Vec<EncodeInput> = chunk
                .iter()
                .map(|d| EncodeInput::Dual(query.into(), d.as_str().into()))
                .collect();
            let encs = self
                .tokenizer
                .encode_batch(pairs, true)
                .map_err(|e| SemanticError::Tokenizer(e.to_string()))?;
            let b = encs.len();
            let sl = encs
                .iter()
                .map(|e| e.get_ids().len())
                .max()
                .unwrap_or(1)
                .max(1)
                .min(self.max_tokens);
            let mut ids = Array2::<i64>::zeros((b, sl));
            let mut mask = Array2::<i64>::zeros((b, sl));
            for (i, e) in encs.iter().enumerate() {
                let n = e.get_ids().len().min(sl);
                for j in 0..n {
                    ids[[i, j]] = e.get_ids()[j] as i64;
                    mask[[i, j]] = 1;
                }
            }
            let mut inputs: Vec<(String, SessionInputValue<'static>)> = Vec::with_capacity(3);
            for name in &self.input_names {
                match name.as_str() {
                    "input_ids" => inputs.push((
                        name.clone(),
                        Tensor::from_array(ids.clone()).map_err(ort_err)?.into(),
                    )),
                    "attention_mask" => inputs.push((
                        name.clone(),
                        Tensor::from_array(mask.clone()).map_err(ort_err)?.into(),
                    )),
                    "token_type_ids" => inputs.push((
                        name.clone(),
                        Tensor::from_array(Array2::<i64>::zeros((b, sl)))
                            .map_err(ort_err)?
                            .into(),
                    )),
                    other => {
                        return Err(SemanticError::Model(format!(
                            "reranker: beklenmeyen girdi `{other}`"
                        )))
                    }
                }
            }
            let mut sess = self.session.lock().unwrap_or_else(|p| p.into_inner());
            let outputs = sess.run(inputs).map_err(ort_err)?;
            let (shape, data) = outputs[0].try_extract_tensor::<f32>().map_err(ort_err)?;
            // logits [b, 1] (ya da [b]).
            let stride = if shape.len() >= 2 {
                shape[1] as usize
            } else {
                1
            }
            .max(1);
            for i in 0..b {
                out.push(data[i * stride]);
            }
        }
        Ok(out)
    }
}

fn ort_err<R>(e: ort::Error<R>) -> SemanticError {
    SemanticError::Ort(e.message().to_string())
}

fn build_session(model_path: &Path, gpu: bool) -> Result<Session, SemanticError> {
    let mut sb = Session::builder()
        .map_err(ort_err)?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(ort_err)?;
    if gpu {
        #[cfg(windows)]
        {
            use ort::ep::DirectML;
            sb = sb
                .with_execution_providers([DirectML::default().build().error_on_failure()])
                .map_err(ort_err)?;
        }
        #[cfg(not(windows))]
        {
            return Err(SemanticError::Model(
                "bu platformda GPU (DirectML) desteği yok".into(),
            ));
        }
    } else {
        let cores = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(2);
        sb = sb.with_intra_threads((cores / 2).max(1)).map_err(ort_err)?;
    }
    sb.commit_from_file(model_path).map_err(ort_err)
}
