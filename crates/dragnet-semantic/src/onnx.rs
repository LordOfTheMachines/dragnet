// SPDX-License-Identifier: AGPL-3.0-only
//! ONNX Runtime tabanlı embedder (MiniLM, EmbeddingGemma). Doğrudan `ort` + `tokenizers`:
//! sabit-pad/batch kontrolü (DirectML'de şekil değişimi yeniden derleme demek), model
//! `sentence_embedding` çıkışını kullanma, minimal bağımlılık (ARCHITECTURE §7.3).

use std::path::Path;
use std::sync::Mutex;

use ndarray::Array2;
use ort::session::{builder::GraphOptimizationLevel, Session, SessionInputValue};
use ort::value::Tensor;
use tokenizers::{Tokenizer, TruncationParams};
use tracing::{info, warn};

use crate::embedder::Embedder;
use crate::models::{Device, ModelSpec, Pooling};
use crate::quant::l2_normalize;
use crate::SemanticError;

/// CPU'da batch boyutu (küçük: sorgu gecikmesini bozmasın, RAM tepe değeri düşük).
const CPU_BATCH: usize = 32;
/// GPU'da batch boyutu (bake-off: 512 ile 3× hız; sabit pad ile tek derleme).
const GPU_BATCH: usize = 256;

pub struct OrtEmbedder {
    spec: &'static ModelSpec,
    session: Mutex<Session>,
    tokenizer: Tokenizer,
    input_names: Vec<String>,
    has_sentence_output: bool,
    device: &'static str,
}

impl OrtEmbedder {
    /// Modeli `models_dir/<id>/` dizininden yükler. `Device::Auto` → DirectML dene, olmazsa CPU.
    pub fn load(
        spec: &'static ModelSpec,
        models_dir: &Path,
        device: Device,
    ) -> Result<Self, SemanticError> {
        let dir = spec.dir(models_dir);
        if !spec.is_downloaded(models_dir) {
            return Err(SemanticError::NotDownloaded(spec.id.to_string()));
        }
        let model_path = dir.join(spec.onnx_file);
        let tokenizer = load_tokenizer(&dir.join("tokenizer.json"), spec.max_tokens)?;

        // Önce istenen cihaz; GPU başarısızsa (Auto) CPU'ya düş.
        let (session, dev) = match device {
            Device::Cpu => (build_session(&model_path, false)?, "cpu"),
            Device::Gpu => (build_session(&model_path, true)?, "directml"),
            Device::Auto => match build_session(&model_path, true) {
                Ok(s) => (s, "directml"),
                Err(e) => {
                    warn!(error = %e, "DirectML kullanılamadı, CPU'ya düşülüyor");
                    (build_session(&model_path, false)?, "cpu")
                }
            },
        };
        let input_names: Vec<String> = session
            .inputs()
            .iter()
            .map(|o| o.name().to_string())
            .collect();
        let has_sentence_output = session
            .outputs()
            .iter()
            .any(|o| o.name() == "sentence_embedding");
        if spec.pooling == Pooling::SentenceOutput && !has_sentence_output {
            return Err(SemanticError::Model(format!(
                "{}: `sentence_embedding` çıkışı yok",
                spec.id
            )));
        }
        info!(model = spec.id, device = dev, inputs = ?input_names, "ONNX embedder yüklendi");
        Ok(Self {
            spec,
            session: Mutex::new(session),
            tokenizer,
            input_names,
            has_sentence_output,
            device: dev,
        })
    }

    fn batch_size(&self) -> usize {
        if self.device == "directml" {
            GPU_BATCH
        } else {
            CPU_BATCH
        }
    }

    /// Bir grup metni embed eder. GPU'da sabit `max_tokens` pad (şekil sabit → tek
    /// derleme); CPU'da batch'in en uzununa pad.
    fn run_batch(&self, texts: &[String], fixed_pad: bool) -> Result<Vec<Vec<f32>>, SemanticError> {
        let encs = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| SemanticError::Tokenizer(e.to_string()))?;
        let b = encs.len();
        let longest = encs
            .iter()
            .map(|e| e.get_ids().len())
            .max()
            .unwrap_or(1)
            .max(1);
        let sl = if fixed_pad {
            self.spec.max_tokens.max(longest)
        } else {
            longest
        };
        let mut ids = Array2::<i64>::zeros((b, sl));
        let mut mask = Array2::<i64>::zeros((b, sl));
        let mut lens = vec![0usize; b];
        for (i, e) in encs.iter().enumerate() {
            let n = e.get_ids().len().min(sl);
            for j in 0..n {
                ids[[i, j]] = e.get_ids()[j] as i64;
                mask[[i, j]] = 1;
            }
            lens[i] = n.max(1);
        }
        let mut inputs: Vec<(String, SessionInputValue<'static>)> = Vec::with_capacity(4);
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
                "position_ids" => {
                    let mut pos = Array2::<i64>::zeros((b, sl));
                    for i in 0..b {
                        for j in 0..sl {
                            pos[[i, j]] = j as i64;
                        }
                    }
                    inputs.push((
                        name.clone(),
                        Tensor::from_array(pos).map_err(ort_err)?.into(),
                    ));
                }
                other => {
                    return Err(SemanticError::Model(format!(
                        "{}: beklenmeyen girdi `{other}`",
                        self.spec.id
                    )))
                }
            }
        }
        let mut sess = self.session.lock().unwrap_or_else(|p| p.into_inner());
        let outputs = sess.run(inputs).map_err(ort_err)?;
        let dim = self.spec.dim;
        let mut out = Vec::with_capacity(b);
        if self.has_sentence_output && self.spec.pooling == Pooling::SentenceOutput {
            let (shape, data) = outputs["sentence_embedding"]
                .try_extract_tensor::<f32>()
                .map_err(ort_err)?;
            let hid = *shape.last().unwrap_or(&0) as usize;
            if hid != dim {
                return Err(SemanticError::Model(format!(
                    "{}: boyut {hid} != {dim}",
                    self.spec.id
                )));
            }
            for i in 0..b {
                let mut v = data[i * hid..(i + 1) * hid].to_vec();
                l2_normalize(&mut v);
                out.push(v);
            }
        } else {
            let (shape, data) = outputs[0].try_extract_tensor::<f32>().map_err(ort_err)?;
            if shape.len() != 3 {
                return Err(SemanticError::Model(format!(
                    "{}: beklenmeyen çıktı şekli {shape:?}",
                    self.spec.id
                )));
            }
            let (osl, hid) = (shape[1] as usize, shape[2] as usize);
            if hid != dim {
                return Err(SemanticError::Model(format!(
                    "{}: boyut {hid} != {dim}",
                    self.spec.id
                )));
            }
            for (i, &len) in lens.iter().enumerate() {
                let mut v = vec![0f32; hid];
                let n = len.min(osl);
                for j in 0..n {
                    let off = (i * osl + j) * hid;
                    for (k, x) in v.iter_mut().enumerate() {
                        *x += data[off + k];
                    }
                }
                let inv = 1.0 / n as f32;
                for x in v.iter_mut() {
                    *x *= inv;
                }
                l2_normalize(&mut v);
                out.push(v);
            }
        }
        Ok(out)
    }
}

fn ort_err<R>(e: ort::Error<R>) -> SemanticError {
    SemanticError::Ort(e.message().to_string())
}

fn load_tokenizer(path: &Path, max_tokens: usize) -> Result<Tokenizer, SemanticError> {
    let mut tok =
        Tokenizer::from_file(path).map_err(|e| SemanticError::Tokenizer(e.to_string()))?;
    tok.with_truncation(Some(TruncationParams {
        max_length: max_tokens,
        ..Default::default()
    }))
    .map_err(|e| SemanticError::Tokenizer(e.to_string()))?;
    // Padding'i kendimiz yapıyoruz (sabit/dinamik seçimi için).
    tok.with_padding(None);
    Ok(tok)
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
        // Arka plan indeksleme uygulamayı boğmasın: çekirdeklerin yarısı.
        let cores = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(2);
        sb = sb.with_intra_threads((cores / 2).max(1)).map_err(ort_err)?;
    }
    sb.commit_from_file(model_path).map_err(ort_err)
}

impl Embedder for OrtEmbedder {
    fn model_id(&self) -> &str {
        self.spec.id
    }
    fn dim(&self) -> usize {
        self.spec.dim
    }
    fn device(&self) -> &str {
        self.device
    }
    fn embed_docs(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, SemanticError> {
        let fixed = self.device == "directml";
        let prefixed: Vec<String> = texts
            .iter()
            .map(|t| format!("{}{t}", self.spec.doc_prefix))
            .collect();
        let mut out = Vec::with_capacity(texts.len());
        for chunk in prefixed.chunks(self.batch_size()) {
            out.extend(self.run_batch(chunk, fixed)?);
        }
        Ok(out)
    }
    fn embed_query(&self, text: &str) -> Result<Vec<f32>, SemanticError> {
        // Tek sorgu: dinamik pad (kısa → hızlı). DirectML'de sorgu için de sabit pad
        // kullanıyoruz ki her farklı uzunlukta yeniden derlenmesin.
        let fixed = self.device == "directml";
        let mut v = self.run_batch(&[format!("{}{text}", self.spec.query_prefix)], fixed)?;
        v.pop()
            .ok_or_else(|| SemanticError::Model("boş çıktı".into()))
    }
}
