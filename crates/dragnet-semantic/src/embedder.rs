// SPDX-License-Identifier: AGPL-3.0-only
//! `Embedder` trait'i — tüm motorların (model2vec, ONNX) ortak yüzeyi — ve
//! çevrimdışı testler için deterministik `MockEmbedder`.

use crate::SemanticError;

/// Metni sabit boyutlu, L2-normalize vektöre çeviren motor. Senkron ve CPU/GPU-yoğun:
/// çağıran taraf `spawn_blocking` kullanmalıdır. `Send + Sync` — `Arc` ile paylaşılır.
pub trait Embedder: Send + Sync {
    /// Model kimliği (indeks uyumluluğu için; değişirse indeks yeniden kurulur).
    fn model_id(&self) -> &str;
    /// Vektör boyutu.
    fn dim(&self) -> usize;
    /// Aktif cihaz: `"cpu"` | `"directml"`.
    fn device(&self) -> &str;
    /// Doküman (torrent adı) vektörleri — metinler önceden normalize edilmiş olmalı
    /// (bkz. [`crate::text::normalize_name`]). Çıktı L2-normalize.
    fn embed_docs(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, SemanticError>;
    /// Sorgu vektörü (bazı modeller sorgu/doküman için farklı önek kullanır).
    fn embed_query(&self, text: &str) -> Result<Vec<f32>, SemanticError>;
}

/// Deterministik sahte motor: metni kelime-hash torbasına dönüştürür. Gerçek anlam
/// taşımaz ama **aynı kelimeleri paylaşan adlar birbirine yakın** çıkar; böylece
/// store/engine/api testleri model indirmeden çalışır.
pub struct MockEmbedder {
    dim: usize,
}

impl MockEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }

    fn embed_one(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0f32; self.dim];
        for word in text.to_lowercase().split(|c: char| !c.is_alphanumeric()) {
            if word.is_empty() {
                continue;
            }
            // FNV-1a — kelime başına iki bileşen (işaretli), çakışmalar önemsiz.
            let mut h: u64 = 0xcbf29ce484222325;
            for b in word.bytes() {
                h ^= b as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
            let i = (h % self.dim as u64) as usize;
            let j = ((h >> 32) % self.dim as u64) as usize;
            v[i] += 1.0;
            v[j] += if (h >> 63) == 0 { 0.5 } else { -0.5 };
        }
        crate::quant::l2_normalize(&mut v);
        v
    }
}

impl Embedder for MockEmbedder {
    fn model_id(&self) -> &str {
        "mock"
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn device(&self) -> &str {
        "cpu"
    }
    fn embed_docs(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, SemanticError> {
        Ok(texts.iter().map(|t| self.embed_one(t)).collect())
    }
    fn embed_query(&self, text: &str) -> Result<Vec<f32>, SemanticError> {
        Ok(self.embed_one(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quant::cosine;

    #[test]
    fn mock_is_deterministic_and_word_sensitive() {
        let m = MockEmbedder::new(64);
        let a = m.embed_query("The Matrix Reloaded 2003").unwrap();
        let a2 = m.embed_query("The Matrix Reloaded 2003").unwrap();
        let b = m.embed_query("Matrix Revolutions 2003").unwrap();
        let c = m.embed_query("ubuntu desktop iso").unwrap();
        assert_eq!(a, a2);
        assert!(
            cosine(&a, &b) > cosine(&a, &c),
            "ortak kelimeli ad daha yakın olmalı"
        );
        assert!((a.iter().map(|x| x * x).sum::<f32>() - 1.0).abs() < 1e-4);
    }
}
