// SPDX-License-Identifier: AGPL-3.0-only
//! Bellek-içi vektör indeksi: int8 satırlar + ölçekler, brute-force kosinüs top-k.
//!
//! Neden ANN değil: 500k×768'de tek çekirdek tarama ~150 ms (native ~65 ms) — interaktif
//! için yeterli, sıfır bağımlılık, sıfır `unsafe`. >2M kayıtta HNSW'ye geçiş açık karar
//! (ARCHITECTURE §7.4). Kalıcılık indeksin işi değil: satırlar SQLite'tan yüklenir
//! (`torrent_embeddings`), yeni kayıtlar `add` ile artımlı eklenir.

use std::collections::HashMap;

use dragnet_core::InfoHash;

use crate::quant::{dot_i8, quantize};

/// Bir arama isabeti.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    pub infohash: InfoHash,
    /// Yaklaşık kosinüs benzerliği (int8 nicemlemeden ötürü ±0.01).
    pub score: f32,
}

/// int8 satır matrisi. `data.len() == ids.len() * dim`.
pub struct VecIndex {
    dim: usize,
    ids: Vec<InfoHash>,
    scales: Vec<f32>,
    data: Vec<i8>,
    pos: HashMap<InfoHash, usize>,
}

impl VecIndex {
    pub fn new(dim: usize) -> Self {
        Self {
            dim,
            ids: Vec::new(),
            scales: Vec::new(),
            data: Vec::new(),
            pos: HashMap::new(),
        }
    }

    pub fn with_capacity(dim: usize, n: usize) -> Self {
        Self {
            dim,
            ids: Vec::with_capacity(n),
            scales: Vec::with_capacity(n),
            data: Vec::with_capacity(n * dim),
            pos: HashMap::with_capacity(n),
        }
    }

    pub fn dim(&self) -> usize {
        self.dim
    }
    pub fn len(&self) -> usize {
        self.ids.len()
    }
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
    pub fn contains(&self, ih: &InfoHash) -> bool {
        self.pos.contains_key(ih)
    }
    /// Yaklaşık RAM kullanımı (bayt).
    pub fn memory_bytes(&self) -> usize {
        self.data.len() + self.scales.len() * 4 + self.ids.len() * (20 + 32)
    }

    /// Nicemlenmiş satır ekler (SQLite'tan yükleme yolu). Var olan infohash → üzerine yazar.
    /// `q.len() != dim` ise yok sayar (bozuk satır) ve `false` döner.
    pub fn add_quantized(&mut self, ih: InfoHash, q: &[i8], scale: f32) -> bool {
        if q.len() != self.dim {
            return false;
        }
        if let Some(&i) = self.pos.get(&ih) {
            self.data[i * self.dim..(i + 1) * self.dim].copy_from_slice(q);
            self.scales[i] = scale;
            return true;
        }
        self.pos.insert(ih, self.ids.len());
        self.ids.push(ih);
        self.scales.push(scale);
        self.data.extend_from_slice(q);
        true
    }

    /// f32 vektörü nicemleyip ekler; `(q, scale)` döner (kalıcılaştırmak için).
    pub fn add(&mut self, ih: InfoHash, v: &[f32]) -> (Vec<i8>, f32) {
        let (q, s) = quantize(v);
        self.add_quantized(ih, &q, s);
        (q, s)
    }

    /// Satırı kaldırır (swap-remove; O(dim)).
    pub fn remove(&mut self, ih: &InfoHash) -> bool {
        let Some(i) = self.pos.remove(ih) else {
            return false;
        };
        let last = self.ids.len() - 1;
        if i != last {
            let last_id = self.ids[last];
            self.ids.swap(i, last);
            self.scales.swap(i, last);
            let (head, tail) = self.data.split_at_mut(last * self.dim);
            head[i * self.dim..(i + 1) * self.dim].copy_from_slice(&tail[..self.dim]);
            self.pos.insert(last_id, i);
        }
        self.ids.pop();
        self.scales.pop();
        self.data.truncate(last * self.dim);
        true
    }

    /// Tümünü boşaltır (model değişimi).
    pub fn clear(&mut self) {
        self.ids.clear();
        self.scales.clear();
        self.data.clear();
        self.pos.clear();
    }

    /// Kosinüs benzerliğine göre en iyi `k` satır (azalan). `min_score` altını atar.
    /// Büyük indekste iş parçacıklarına bölünür (std::thread::scope, bağımlılık yok).
    pub fn search(&self, query: &[f32], k: usize, min_score: f32) -> Vec<Hit> {
        if k == 0 || self.ids.is_empty() || query.len() != self.dim {
            return Vec::new();
        }
        let (qq, qs) = quantize(query);
        let n = self.ids.len();
        let threads = if n >= 50_000 {
            std::thread::available_parallelism()
                .map(|p| p.get())
                .unwrap_or(1)
                .clamp(1, 8)
        } else {
            1
        };
        let chunk = n.div_ceil(threads);
        let partials: Vec<Vec<(f32, usize)>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..threads)
                .map(|t| {
                    let start = t * chunk;
                    let end = ((t + 1) * chunk).min(n);
                    let qq = &qq;
                    s.spawn(move || self.scan_range(start, end, qq, qs, k, min_score))
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_default())
                .collect()
        });
        let mut all: Vec<(f32, usize)> = partials.into_iter().flatten().collect();
        all.sort_unstable_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        all.truncate(k);
        all.into_iter()
            .map(|(score, i)| Hit {
                infohash: self.ids[i],
                score,
            })
            .collect()
    }

    /// `[start, end)` aralığında top-k (eşik-kırpmalı basit seçim).
    fn scan_range(
        &self,
        start: usize,
        end: usize,
        qq: &[i8],
        qs: f32,
        k: usize,
        min_score: f32,
    ) -> Vec<(f32, usize)> {
        if start >= end {
            return Vec::new();
        }
        let mut top: Vec<(f32, usize)> = Vec::with_capacity(k * 2 + 1);
        let mut thresh = min_score;
        for i in start..end {
            let row = &self.data[i * self.dim..(i + 1) * self.dim];
            let score = qs * self.scales[i] * dot_i8(row, qq) as f32;
            if score >= thresh {
                top.push((score, i));
                if top.len() > 2 * k {
                    top.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));
                    top.truncate(k);
                    thresh = thresh.max(top[k - 1].0);
                }
            }
        }
        top.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));
        top.truncate(k);
        top
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::{Embedder, MockEmbedder};

    fn ih(n: u8) -> InfoHash {
        InfoHash::from_bytes([n; 20])
    }

    #[test]
    fn add_search_remove_roundtrip() {
        let m = MockEmbedder::new(64);
        let mut idx = VecIndex::new(64);
        let names = [
            "The Matrix Reloaded 2003",
            "Matrix Revolutions 2003",
            "ubuntu desktop iso",
            "debian netinst iso",
        ];
        for (i, n) in names.iter().enumerate() {
            idx.add(ih(i as u8), &m.embed_query(n).unwrap());
        }
        assert_eq!(idx.len(), 4);
        let q = m.embed_query("matrix 2003").unwrap();
        let hits = idx.search(&q, 2, -1.0);
        assert_eq!(hits.len(), 2);
        assert!(
            hits.iter()
                .all(|h| h.infohash == ih(0) || h.infohash == ih(1)),
            "{hits:?}"
        );
        assert!(hits[0].score >= hits[1].score);

        // Kaldır → artık gelmez; swap-remove sonrası diğerleri sağlam.
        assert!(idx.remove(&ih(0)));
        assert!(!idx.contains(&ih(0)));
        let hits = idx.search(&q, 4, -1.0);
        assert!(hits.iter().all(|h| h.infohash != ih(0)));
        assert_eq!(idx.len(), 3);
        let q2 = m.embed_query("debian iso").unwrap();
        assert_eq!(idx.search(&q2, 1, -1.0)[0].infohash, ih(3));

        // Üzerine yazma satır sayısını artırmaz.
        idx.add(ih(3), &m.embed_query("something else").unwrap());
        assert_eq!(idx.len(), 3);
        // Boyut uyumsuz satır reddedilir.
        assert!(!idx.add_quantized(ih(9), &[1i8; 10], 0.1));
    }

    #[test]
    fn multithreaded_scan_matches_single() {
        // 60k satır → çok iş parçacıklı yol; sonuç tek-parçalı tarama ile aynı olmalı.
        let dim = 32;
        let mut idx = VecIndex::with_capacity(dim, 60_000);
        let mut s = 0x9e3779b97f4a7c15u64;
        let mut rows = Vec::new();
        for i in 0..60_000u32 {
            let mut v: Vec<f32> = (0..dim)
                .map(|_| {
                    s ^= s << 13;
                    s ^= s >> 7;
                    s ^= s << 17;
                    ((s >> 40) as f32 / 8388608.0) - 1.0
                })
                .collect();
            crate::quant::l2_normalize(&mut v);
            let mut b = [0u8; 20];
            b[..4].copy_from_slice(&i.to_le_bytes());
            idx.add(InfoHash::from_bytes(b), &v);
            rows.push(v);
        }
        let q = rows[12_345].clone();
        let hits = idx.search(&q, 5, -1.0);
        assert_eq!(hits.len(), 5);
        // Kendisi en üstte (~1.0).
        let mut b = [0u8; 20];
        b[..4].copy_from_slice(&12_345u32.to_le_bytes());
        assert_eq!(hits[0].infohash, InfoHash::from_bytes(b));
        assert!(hits[0].score > 0.98, "{}", hits[0].score);
        // Tek parçalı referans.
        let (qq, qs) = quantize(&q);
        let single = idx.scan_range(0, idx.len(), &qq, qs, 5, -1.0);
        let single_ids: Vec<usize> = single.iter().map(|x| x.1).collect();
        let multi_ids: Vec<usize> = hits.iter().map(|h| idx.pos[&h.infohash]).collect();
        assert_eq!(single_ids, multi_ids);
    }
}
