// SPDX-License-Identifier: AGPL-3.0-only
//! Hibrit harman: FTS (sözcüksel) ve semantik aday listelerini **Reciprocal Rank Fusion**
//! ile birleştirir. RRF skor ölçeklerinden bağımsızdır (FTS'in bm25'i ile kosinüs
//! kıyaslanamaz), iki listede de görünen kayıtları öne çıkarır.

use std::collections::HashMap;
use std::hash::Hash;

/// RRF sabiti (literatürde 60; küçük k üst sıraları daha agresif ödüllendirir).
pub const RRF_K: f32 = 60.0;

/// Sıralı aday listelerini RRF ile harmanlar; `(kimlik, skor)` azalan skorla döner.
/// Listeler ağırlıklandırılabilir (`weights` — boşsa hepsi 1.0). Kararlı: eşit skorda
/// ilk listede önce görülen kazanır.
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
        let fts = vec!["a", "b", "c"];
        let sem = vec!["c", "d", "a"];
        let fused = rrf(&[fts, sem], &[], RRF_K);
        let ids: Vec<&str> = fused.iter().map(|(i, _)| *i).collect();
        // a: 1/61 + 1/63; c: 1/63 + 1/61 → eşit; a ilk görüldü → önce.
        assert_eq!(&ids[..2], &["a", "c"]);
        assert!(ids.contains(&"d") && ids.contains(&"b"));
        assert_eq!(ids.len(), 4);
    }

    #[test]
    fn empty_lists_are_fine() {
        let fused: Vec<(u32, f32)> = rrf(&[Vec::new(), vec![1, 2]], &[1.0, 1.0], RRF_K);
        assert_eq!(fused.iter().map(|x| x.0).collect::<Vec<_>>(), vec![1, 2]);
        let none: Vec<(u32, f32)> = rrf(&[], &[], RRF_K);
        assert!(none.is_empty());
    }
}
