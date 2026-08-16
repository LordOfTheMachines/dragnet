// SPDX-License-Identifier: AGPL-3.0-only
//! Ortak arama yolu (HTTP API + masaüstü uygulaması aynı fonksiyonu kullanır):
//! sorgu modu seçimi (fts / semantic / hybrid) ve semantik katmanın **çalışma anında
//! takılıp çıkarılabilen** yuvası (`SemanticSlot`). Semantik kapalıyken davranış
//! birebir eski FTS'tir.

use std::sync::Arc;

use dragnet_semantic::Semantic;
use dragnet_store::{Filter, SortKey, Store, StoreError, TorrentSummary, HYBRID_CANDIDATES};

/// Semantik katman yuvası: `None` = kapalı. Uygulama ayarlardan açıp kapatınca yuvayı
/// günceller; API/arama bir sonraki sorguda yeni durumu görür (yeniden başlatma yok).
pub type SemanticSlot = Arc<tokio::sync::RwLock<Option<Arc<Semantic>>>>;

/// Boş (kapalı) yuva.
pub fn empty_slot() -> SemanticSlot {
    Arc::new(tokio::sync::RwLock::new(None))
}

/// Arama modu (`mode` parametresi). Bilinmeyen/boş → `Auto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchMode {
    /// Semantik hazırsa hibrit, değilse FTS.
    #[default]
    Auto,
    Fts,
    Semantic,
    Hybrid,
}

impl SearchMode {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "fts" | "text" | "keyword" => Self::Fts,
            "semantic" | "sem" | "vector" => Self::Semantic,
            "hybrid" => Self::Hybrid,
            _ => Self::Auto,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Fts => "fts",
            Self::Semantic => "semantic",
            Self::Hybrid => "hybrid",
        }
    }
}

/// Arama sonucu + gerçekte kullanılan mod (UI rozeti için).
pub struct SearchOutcome {
    pub rows: Vec<TorrentSummary>,
    pub used: SearchMode,
}

/// Sorguyu moda göre yürütür. Boş sorgu → gözat (`list_paged`; Relevance → Seen).
/// Semantik istenip de hazır değilse FTS'e düşer (hata değil).
#[allow(clippy::too_many_arguments)]
pub async fn search(
    store: &Store,
    slot: &SemanticSlot,
    query: &str,
    mode: SearchMode,
    limit: i64,
    offset: i64,
    sort: SortKey,
    desc: bool,
    filter: &Filter,
) -> Result<SearchOutcome, StoreError> {
    let q = query.trim();
    if q.is_empty() {
        let sk = if matches!(sort, SortKey::Relevance) {
            SortKey::Seen
        } else {
            sort
        };
        let rows = store.list_paged(limit, offset, sk, desc, filter).await?;
        return Ok(SearchOutcome {
            rows,
            used: SearchMode::Fts,
        });
    }
    let sem = if mode == SearchMode::Fts {
        None
    } else {
        slot.read().await.clone()
    };
    let Some(sem) = sem else {
        let rows = store
            .search_paged(q, limit, offset, sort, desc, filter)
            .await?;
        return Ok(SearchOutcome {
            rows,
            used: SearchMode::Fts,
        });
    };
    // Sorgu embed'i CPU-yoğun (10–60 ms) → blocking havuzunda.
    let qs = q.to_string();
    let sem2 = Arc::clone(&sem);
    let hits = tokio::task::spawn_blocking(move || sem2.search(&qs, HYBRID_CANDIDATES as usize))
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or_default();
    let ids: Vec<_> = hits.iter().map(|h| h.infohash).collect();
    let (fts_query, used) = match mode {
        SearchMode::Semantic => ("", SearchMode::Semantic),
        _ => (q, SearchMode::Hybrid),
    };
    let rows = store
        .search_hybrid_paged(fts_query, &ids, limit, offset, sort, desc, filter)
        .await?;
    Ok(SearchOutcome { rows, used })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dragnet_core::{InfoHash, TorrentFile, TorrentRecord};
    use dragnet_semantic::{MockEmbedder, Tier};

    fn record(hex: &str, name: &str) -> TorrentRecord {
        TorrentRecord {
            infohash: InfoHash::from_hex(hex).unwrap(),
            name: name.to_string(),
            total_size: 1,
            files: vec![TorrentFile {
                path: name.to_string(),
                size: 1,
            }],
            first_seen: 1000,
            last_seen: 1000,
            seen_count: 1,
        }
    }

    #[tokio::test]
    async fn modes_fall_back_and_fuse() {
        let store = Store::in_memory().await.unwrap();
        store
            .upsert_torrent(&record(
                "1111111111111111111111111111111111111111",
                "The.Matrix.1999.1080p",
            ))
            .await
            .unwrap();
        store
            .upsert_torrent(&record(
                "2222222222222222222222222222222222222222",
                "Matrix.Reloaded.2003",
            ))
            .await
            .unwrap();
        store
            .upsert_torrent(&record(
                "3333333333333333333333333333333333333333",
                "ubuntu.iso",
            ))
            .await
            .unwrap();
        let slot = empty_slot();
        let f = Filter::default();

        // Semantik kapalı: her mod FTS'e düşer.
        let r = search(
            &store,
            &slot,
            "matrix",
            SearchMode::Hybrid,
            10,
            0,
            SortKey::Relevance,
            true,
            &f,
        )
        .await
        .unwrap();
        assert_eq!(r.used, SearchMode::Fts);
        assert_eq!(r.rows.len(), 2);
        // Boş sorgu → gözat.
        let r = search(
            &store,
            &slot,
            "  ",
            SearchMode::Auto,
            10,
            0,
            SortKey::Relevance,
            true,
            &f,
        )
        .await
        .unwrap();
        assert_eq!(r.rows.len(), 3);

        // Semantik aç (mock) ve indeksi doldur.
        let sem = Arc::new(Semantic::with_embedder(
            Box::new(MockEmbedder::new(32)),
            Tier::Light,
            0.0,
        ));
        sem.embed_and_add(&[
            (
                InfoHash::from_hex("1111111111111111111111111111111111111111").unwrap(),
                "The.Matrix.1999.1080p".into(),
            ),
            (
                InfoHash::from_hex("2222222222222222222222222222222222222222").unwrap(),
                "Matrix.Reloaded.2003".into(),
            ),
            (
                InfoHash::from_hex("3333333333333333333333333333333333333333").unwrap(),
                "ubuntu.iso".into(),
            ),
        ])
        .unwrap();
        *slot.write().await = Some(sem);

        // Auto → hibrit; "matrix 1999" FTS 1'i bulur, semantik de 1'i öne alır.
        let r = search(
            &store,
            &slot,
            "matrix 1999",
            SearchMode::Auto,
            10,
            0,
            SortKey::Relevance,
            true,
            &f,
        )
        .await
        .unwrap();
        assert_eq!(r.used, SearchMode::Hybrid);
        assert_eq!(r.rows[0].name, "The.Matrix.1999.1080p");
        // fts modu zorlanınca semantik yok sayılır.
        let r = search(
            &store,
            &slot,
            "matrix",
            SearchMode::Fts,
            10,
            0,
            SortKey::Relevance,
            true,
            &f,
        )
        .await
        .unwrap();
        assert_eq!(r.used, SearchMode::Fts);
        // Saf semantik: FTS'in bulamayacağı sorguda bile (mock: ortak kelime) sonuç.
        let r = search(
            &store,
            &slot,
            "ubuntu",
            SearchMode::Semantic,
            10,
            0,
            SortKey::Relevance,
            true,
            &f,
        )
        .await
        .unwrap();
        assert_eq!(r.used, SearchMode::Semantic);
        assert_eq!(r.rows[0].name, "ubuntu.iso");
        // Kapat → tekrar FTS.
        *slot.write().await = None;
        let r = search(
            &store,
            &slot,
            "matrix",
            SearchMode::Auto,
            10,
            0,
            SortKey::Relevance,
            true,
            &f,
        )
        .await
        .unwrap();
        assert_eq!(r.used, SearchMode::Fts);
    }
}
