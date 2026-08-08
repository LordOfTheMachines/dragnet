// SPDX-License-Identifier: AGPL-3.0-only
//! Yakın zamanda görülen infohash'ler için sınırlı boyutlu (LRU benzeri) küme.
//!
//! DHT'de aynı infohash saniyeler içinde defalarca uçar; kanalı tekrarlarla
//! doldurmamak için kaba bir "son N benzersiz" filtresi tutarız. Kapasite
//! aşılınca en eski giriş düşürülür (FIFO tahliye — gerçek LRU'ya yakın ve ucuz).

use std::collections::{HashSet, VecDeque};

/// Sabit kapasiteli, ekleme sırasına göre tahliye eden benzersizlik filtresi.
#[derive(Debug)]
pub struct RecentSet {
    capacity: usize,
    seen: HashSet<[u8; 20]>,
    order: VecDeque<[u8; 20]>,
}

impl RecentSet {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            capacity,
            seen: HashSet::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
        }
    }

    /// Anahtar yeni ise ekler ve `true`, daha önce görülmüşse `false` döner.
    pub fn insert(&mut self, key: [u8; 20]) -> bool {
        if self.seen.contains(&key) {
            return false;
        }
        if self.order.len() >= self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
        self.order.push_back(key);
        self.seen.insert(key);
        true
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.seen.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedups_repeats() {
        let mut set = RecentSet::new(8);
        assert!(set.insert([1u8; 20]));
        assert!(!set.insert([1u8; 20]));
        assert!(set.insert([2u8; 20]));
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn evicts_oldest_when_full() {
        let mut set = RecentSet::new(2);
        assert!(set.insert([1u8; 20]));
        assert!(set.insert([2u8; 20]));
        // 3. ekleme en eskiyi (1) düşürür.
        assert!(set.insert([3u8; 20]));
        assert_eq!(set.len(), 2);
        // 1 tahliye edildiği için yeniden "yeni" sayılır.
        assert!(set.insert([1u8; 20]));
    }
}
