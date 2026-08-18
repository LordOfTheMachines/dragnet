// SPDX-License-Identifier: AGPL-3.0-only
//! Model kataloğu (3 kademe) + bir kerelik indirme.
//!
//! Modeller `<models_dir>/<spec.id>/<dosya>` düzeninde **düz ve kısa** yolda tutulur —
//! ORT Windows'ta ~230+ karakterlik derin yolları "File doesn't exist" diye reddediyor
//! (ARCHITECTURE §7.3). Kaynak MVP'de HuggingFace `resolve/main` URL'leri; kendi
//! release'imizden dağıtım (damıtılmış model) açık karar (§7.4). İndirme sonrası her şey
//! çevrimdışı çalışır.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::SemanticError;

/// Hız ↔ kalite kademesi (bkz. ARCHITECTURE §7.3 bake-off).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// potion-multilingual-128M (model2vec, statik) — 58k ad/sn, zayıf makineler.
    Light,
    /// paraphrase-multilingual-MiniLM-L12-v2 int8 ONNX — orta CPU, küçük indirme.
    Balanced,
    /// EmbeddingGemma-300m Q4 ONNX — en iyi kalite; GPU'da 2–3× hızlı indeksleme.
    #[default]
    Quality,
}

impl Tier {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "light" | "hafif" => Self::Light,
            "balanced" | "dengeli" => Self::Balanced,
            _ => Self::Quality,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Balanced => "balanced",
            Self::Quality => "quality",
        }
    }
    pub fn spec(self) -> &'static ModelSpec {
        match self {
            Self::Light => &POTION,
            Self::Balanced => &MINILM,
            Self::Quality => &GEMMA,
        }
    }
}

/// Hesaplama cihazı tercihi.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Device {
    /// GPU (DirectML) dene, olmazsa CPU.
    #[default]
    Auto,
    Gpu,
    Cpu,
}

impl Device {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "gpu" | "directml" => Self::Gpu,
            "cpu" => Self::Cpu,
            _ => Self::Auto,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Gpu => "gpu",
            Self::Cpu => "cpu",
        }
    }
}

/// Motor türü.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// model2vec statik embedding (safetensors).
    Model2Vec,
    /// ONNX Runtime.
    Onnx,
}

/// ONNX havuzlama.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pooling {
    /// Model `sentence_embedding` çıkışı verir (Gemma) — doğrudan kullan.
    SentenceOutput,
    /// `last_hidden_state` üzerinde maskeli ortalama.
    Mean,
    /// Son gerçek token'ın gizli durumu (Qwen3-Embedding; sağa pad).
    LastToken,
}

/// İndirilecek dosya: yerel ad + URL + yaklaşık boyut (ilerleme çubuğu için).
#[derive(Debug, Clone, Copy)]
pub struct ModelFile {
    pub name: &'static str,
    pub url: &'static str,
    pub approx_bytes: u64,
}

/// Bir kademenin model tanımı.
#[derive(Debug, Clone)]
pub struct ModelSpec {
    pub tier: Tier,
    /// Kısa, dosya sistemi dostu kimlik; indeks uyumluluğu bununla izlenir.
    pub id: &'static str,
    pub display_name: &'static str,
    pub engine: Engine,
    pub dim: usize,
    pub files: &'static [ModelFile],
    /// ONNX model dosyasının yerel adı (Model2Vec'te kullanılmaz).
    pub onnx_file: &'static str,
    pub pooling: Pooling,
    pub doc_prefix: &'static str,
    pub query_prefix: &'static str,
    /// Token sınırı (kırpma) — torrent adları kısa; GPU'da sabit pad uzunluğu.
    pub max_tokens: usize,
    /// Bu kosinüs benzerliğinin altındaki isabetler "alakasız" sayılır (aday kırpma).
    pub min_score: f32,
    /// DirectML (GPU) ile çalışabilir mi? (Qwen3 export'u DML'de Concat hatası veriyor.)
    pub gpu_ok: bool,
    pub license: &'static str,
}

impl ModelSpec {
    /// Toplam yaklaşık indirme boyutu.
    pub fn approx_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.approx_bytes).sum()
    }
    /// Modelin yerel dizini.
    pub fn dir(&self, models_dir: &Path) -> PathBuf {
        // `id` indeks uyumluluk kimliğidir ("...:v2" şema soneki içerir); dizin adı iki nokta
        // öncesi kısımdır (Windows'ta `:` dosya adında geçersiz).
        models_dir.join(self.id.split(':').next().unwrap_or(self.id))
    }
    /// Tüm dosyalar (tam) indirilmiş mi?
    pub fn is_downloaded(&self, models_dir: &Path) -> bool {
        let d = self.dir(models_dir);
        self.files.iter().all(|f| {
            let p = d.join(f.name);
            p.is_file()
                && fs::metadata(&p).map(|m| m.len() > 0).unwrap_or(false)
                && !d.join(format!("{}.part", f.name)).exists()
        })
    }
}

pub static POTION: ModelSpec = ModelSpec {
    tier: Tier::Light,
    id: "potion-multilingual-128M:v2",
    display_name: "Potion multilingual 128M (model2vec, statik)",
    engine: Engine::Model2Vec,
    dim: 256,
    files: &[
        ModelFile { name: "config.json", url: "https://huggingface.co/minishlab/potion-multilingual-128M/resolve/main/config.json", approx_bytes: 1_000 },
        ModelFile { name: "tokenizer.json", url: "https://huggingface.co/minishlab/potion-multilingual-128M/resolve/main/tokenizer.json", approx_bytes: 18_000_000 },
        ModelFile { name: "model.safetensors", url: "https://huggingface.co/minishlab/potion-multilingual-128M/resolve/main/model.safetensors", approx_bytes: 512_000_000 },
    ],
    onnx_file: "",
    pooling: Pooling::Mean,
    doc_prefix: "",
    query_prefix: "",
    max_tokens: 128,
    min_score: 0.20,
    gpu_ok: false,
    license: "MIT",
};

pub static MINILM: ModelSpec = ModelSpec {
    tier: Tier::Balanced,
    id: "minilm-l12-multilingual-q:v2",
    display_name: "paraphrase-multilingual-MiniLM-L12-v2 (int8 ONNX)",
    engine: Engine::Onnx,
    dim: 384,
    files: &[
        ModelFile { name: "tokenizer.json", url: "https://huggingface.co/Qdrant/paraphrase-multilingual-MiniLM-L12-v2-onnx-Q/resolve/main/tokenizer.json", approx_bytes: 17_100_000 },
        ModelFile { name: "model_optimized.onnx", url: "https://huggingface.co/Qdrant/paraphrase-multilingual-MiniLM-L12-v2-onnx-Q/resolve/main/model_optimized.onnx", approx_bytes: 235_000_000 },
    ],
    onnx_file: "model_optimized.onnx",
    pooling: Pooling::Mean,
    doc_prefix: "",
    query_prefix: "",
    max_tokens: 64,
    min_score: 0.45,
    gpu_ok: true,
    license: "Apache-2.0",
};

pub static GEMMA: ModelSpec = ModelSpec {
    tier: Tier::Quality,
    id: "embeddinggemma-300m-q4:v2",
    display_name: "EmbeddingGemma 300M (Q4 ONNX)",
    engine: Engine::Onnx,
    dim: 768,
    files: &[
        ModelFile { name: "tokenizer.json", url: "https://huggingface.co/onnx-community/embeddinggemma-300m-ONNX/resolve/main/tokenizer.json", approx_bytes: 33_400_000 },
        ModelFile { name: "model_q4.onnx", url: "https://huggingface.co/onnx-community/embeddinggemma-300m-ONNX/resolve/main/onnx/model_q4.onnx", approx_bytes: 520_000 },
        ModelFile { name: "model_q4.onnx_data", url: "https://huggingface.co/onnx-community/embeddinggemma-300m-ONNX/resolve/main/onnx/model_q4.onnx_data", approx_bytes: 196_800_000 },
    ],
    onnx_file: "model_q4.onnx",
    pooling: Pooling::SentenceOutput,
    // EmbeddingGemma'nın resmî görev önekleri (retrieval).
    doc_prefix: "title: none | text: ",
    query_prefix: "task: search result | query: ",
    max_tokens: 64,
    min_score: 0.25,
    gpu_ok: true,
    license: "Gemma Terms of Use (kullanıcı çalışma anında indirir; koda gömülmez)",
};

/// İndirme ilerleme geri çağrısı: (dosya adı, indirilen bayt, toplam bayt (bilinmiyorsa 0)).
pub type Progress<'a> = &'a (dyn Fn(&str, u64, u64) + Sync);

/// Kademenin tüm dosyalarını indirir (eksik olanları). Bloklar; `spawn_blocking` ile çağır.
/// Kısmi dosyalar `.part` olarak yazılır, bitince atomik yeniden adlandırılır.
pub fn download(
    spec: &ModelSpec,
    models_dir: &Path,
    progress: Progress<'_>,
) -> Result<(), SemanticError> {
    let dir = spec.dir(models_dir);
    download_files_to(spec.id, &dir, spec.files, progress)
}

/// Genel indirici: `models_dir/<dir_name>/` altına dosya listesini indirir (embedding
/// modelleri ve reranker ortak kullanır).
pub fn download_files(
    dir_name: &str,
    files: &[ModelFile],
    models_dir: &Path,
    progress: Progress<'_>,
) -> Result<(), SemanticError> {
    let dir = models_dir.join(dir_name.split(':').next().unwrap_or(dir_name));
    download_files_to(dir_name, &dir, files, progress)
}

fn download_files_to(
    model_id: &str,
    dir: &Path,
    files: &[ModelFile],
    progress: Progress<'_>,
) -> Result<(), SemanticError> {
    fs::create_dir_all(dir)?;
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("dragnet-semantic/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(60 * 60))
        .connect_timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| SemanticError::Http(e.to_string()))?;
    for f in files {
        let dest = dir.join(f.name);
        let part = dir.join(format!("{}.part", f.name));
        if dest.is_file()
            && !part.exists()
            && fs::metadata(&dest).map(|m| m.len() > 0).unwrap_or(false)
        {
            progress(f.name, f.approx_bytes, f.approx_bytes);
            continue;
        }
        tracing::info!(
            model = model_id,
            file = f.name,
            url = f.url,
            "model dosyası indiriliyor"
        );
        let mut resp = client
            .get(f.url)
            .send()
            .and_then(|r| r.error_for_status())
            .map_err(|e| SemanticError::Http(format!("{}: {e}", f.name)))?;
        let total = resp.content_length().unwrap_or(0);
        let mut out = fs::File::create(&part)?;
        let mut buf = vec![0u8; 1 << 20];
        let mut done = 0u64;
        loop {
            let n = resp
                .read(&mut buf)
                .map_err(|e| SemanticError::Http(format!("{}: {e}", f.name)))?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            done += n as u64;
            progress(f.name, done, total);
        }
        out.flush()?;
        drop(out);
        if total > 0 && done != total {
            let _ = fs::remove_file(&part);
            return Err(SemanticError::Http(format!(
                "{}: eksik indirme ({done}/{total} bayt)",
                f.name
            )));
        }
        fs::rename(&part, &dest)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_and_device_parse() {
        assert_eq!(Tier::parse("light"), Tier::Light);
        assert_eq!(Tier::parse("Dengeli"), Tier::Balanced);
        assert_eq!(Tier::parse("bogus"), Tier::Quality);
        assert_eq!(Device::parse("GPU"), Device::Gpu);
        assert_eq!(Device::parse(""), Device::Auto);
        for t in [Tier::Light, Tier::Balanced, Tier::Quality] {
            assert_eq!(Tier::parse(t.as_str()), t);
            assert!(t.spec().approx_bytes() > 0);
            assert!(!t.spec().files.is_empty());
        }
    }

    #[test]
    fn is_downloaded_requires_all_files_and_no_part() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = &MINILM;
        assert!(!spec.is_downloaded(tmp.path()));
        let d = spec.dir(tmp.path());
        fs::create_dir_all(&d).unwrap();
        for f in spec.files {
            fs::write(d.join(f.name), b"x").unwrap();
        }
        assert!(spec.is_downloaded(tmp.path()));
        fs::write(d.join("model_optimized.onnx.part"), b"").unwrap();
        assert!(!spec.is_downloaded(tmp.path()));
    }
}
