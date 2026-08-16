// SPDX-License-Identifier: AGPL-3.0-only
//! Sıralama harmanı: **Reciprocal Rank Fusion** (RRF). Sözcüksel (FTS) ve semantik aday
//! listelerini skor ölçeklerinden bağımsız birleştirir; iki listede de görünen kayıtları
//! öne çıkarır. `dragnet-store` (hibrit arama) ve `dragnet-semantic` ortak kullanır.

use std::collections::HashMap;
use std::hash::Hash;

/// RRF sabiti (literatürde 60; küçük k üst sıraları daha agresif ödüllendirir).
pub const RRF_K: f32 = 60.0;

/// Sıralı aday listelerini RRF ile harmanlar; `(kimlik, skor)` azalan skorla döner.
/// `weights` boşsa hepsi 1.0. Kararlı: eşit skorda ilk görülen önce.
pub fn rrf<T: Eq + Hash + Clone>(lists: &[Vec<T>], weights: &[f32], k: f32) -> Vec<(T, f32)> {
    let mut score: HashMap<T, f32> = HashMap::new();
    let mut first_seen: HashMap<T, usize> = HashMap::new();
    let mut order = 0usize;
    for (li, list) in lists.iter().enumerate() {
        let w = weights.get(li).copied().unwrap_or(1.0);
        for (rank, id) in list.iter().enumerate() {
            *score.entry(id.clone()).or_insert(0.0) += w / (k + rank as f32 + 1.0);
            first_seen.entry(id.clone()).or_insert_with(|| {
                order += 1;
                order
            });
        }
    }
    let mut out: Vec<(T, f32)> = score.into_iter().collect();
    out.sort_by(|a, b| {
        b.1.total_cmp(&a.1)
            .then_with(|| first_seen[&a.0].cmp(&first_seen[&b.0]))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_in_both_lists_rank_first() {
        let fused = rrf(&[vec!["a", "b", "c"], vec!["c", "d", "a"]], &[], RRF_K);
        let ids: Vec<&str> = fused.iter().map(|(i, _)| *i).collect();
        assert_eq!(&ids[..2], &["a", "c"]);
        assert_eq!(ids.len(), 4);
    }

    #[test]
    fn empty_lists_are_fine() {
        let fused: Vec<(u32, f32)> = rrf(&[Vec::new(), vec![1, 2]], &[1.0, 1.0], RRF_K);
        assert_eq!(fused.iter().map(|x| x.0).collect::<Vec<_>>(), vec![1, 2]);
        assert!(rrf::<u32>(&[], &[], RRF_K).is_empty());
    }
}
