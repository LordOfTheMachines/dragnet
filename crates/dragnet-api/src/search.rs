// SPDX-License-Identifier: AGPL-3.0-only
//! Ortak arama yolu (HTTP API + masaüstü uygulaması aynı fonksiyonu kullanır):
//! sorgu modu seçimi (fts / semantic / hybrid) ve semantik katmanın **çalışma anında
//! takılıp çıkarılabilen** yuvası (`SemanticSlot`). Semantik kapalıyken davranış
//! birebir eski FTS'tir.

use std::sync::Arc;

use dragnet_semantic::Semantic;
use dragnet_store::{Boost, Filter, SortKey, Store, StoreError, TorrentSummary, HYBRID_CANDIDATES};

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
    /// Sorgunun korpusta karşılığı bulunamadı (cross-encoder skoru eşiğin altında ve
    /// adlarda sözcüksel kanıt yok) → `rows` bilerek boştur. UI "eşleşme bulunamadı"
    /// der; eskiden 30 alakasız satır gösteriliyordu. Bkz. `WEAK_MATCH_SCORE`.
    pub weak: bool,
}

/// Adlarda sorgu kelimelerinden biri geçiyor mu? (Sözcüksel kanıt: geçiyorsa sonuçlar
/// zayıf sayılmaz — kullanıcı yazdığı kelimeyi sonuçta görüyordur.)
fn has_lexical_evidence(names: &[String], query: &str) -> bool {
    let toks: Vec<String> = query
        .split_whitespace()
        .map(|t| t.trim().to_lowercase())
        .filter(|t| t.chars().count() >= 3)
        .collect();
    if toks.is_empty() {
        return false;
    }
    names.iter().any(|n| {
        let n = n.to_lowercase();
        toks.iter().any(|t| n.contains(t.as_str()))
    })
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
    // `true`: güven kapısı atlanır — kullanıcı "yine de en yakın sonuçları göster" dedi.
    show_weak: bool,
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
            weak: false,
        });
    }
    // Sorgu anlama: dolgu temizliği, kategori niyeti, yıl aralığı (bkz.
    // `dragnet_semantic::query`). FTS-yalnız modda da dolgu temizliği uygulanır (dolgu FTS'i
    // de bozar) — niyet artırması yalnız hibrit yolda (saf FTS sözleşmesi değişmesin).
    let plan = dragnet_semantic::query::understand(q);
    let boost = Boost {
        // Kullanıcı kategori seçtiyse ona saygı; yoksa sorgudan çıkarılan.
        category: if filter.category.is_some() {
            None
        } else {
            plan.category.map(str::to_string)
        },
        year_range: plan.year_range,
    };
    let sem = if mode == SearchMode::Fts {
        None
    } else {
        slot.read().await.clone()
    };
    let Some(sem) = sem else {
        let fts_q = if plan.fts_text.is_empty() {
            q
        } else {
            plan.fts_text.as_str()
        };
        let rows = store
            .search_paged(fts_q, limit, offset, sort, desc, filter)
            .await?;
        return Ok(SearchOutcome {
            rows,
            used: SearchMode::Fts,
            weak: false,
        });
    };
    // Sorgu embed'i CPU-yoğun (10–60 ms) → blocking havuzunda.
    let qs = if plan.semantic_text.is_empty() {
        q.to_string()
    } else {
        plan.semantic_text.clone()
    };
    let sem2 = Arc::clone(&sem);
    // Semantik aday sayısı: FTS adaylarından az (kesim sonrası genelde çok daha az kalır).
    let hits =
        tokio::task::spawn_blocking(move || sem2.search(&qs, (HYBRID_CANDIDATES / 4) as usize))
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or_default();
    let ids: Vec<_> = hits.iter().map(|h| h.infohash).collect();
    let fts_text = if plan.fts_text.is_empty() {
        q
    } else {
        plan.fts_text.as_str()
    };
    let (fts_query, used) = match mode {
        SearchMode::Semantic => ("", SearchMode::Semantic),
        _ => (fts_text, SearchMode::Hybrid),
    };
    // Yeniden sıralayıcı (cross-encoder) varsa ve alaka sırasındaysak: harmanın ilk
    // RERANK_TOP_N adayı sorguyla birlikte puanlanıp yeniden sıralanır; sayfalama bunun
    // üstünde yapılır (ilk N tutarlı kalsın diye her sayfada aynı pencere yeniden sıralanır).
    let reranker = if matches!(sort, SortKey::Relevance) {
        sem.reranker()
    } else {
        None
    };
    let rows = if let Some(rr) = reranker {
        let n = dragnet_semantic::RERANK_TOP_N as i64;
        let want = (offset + limit).max(n);
        let mut all = store
            .search_hybrid_boosted(fts_query, &ids, want, 0, sort, desc, filter, &boost)
            .await?;
        let head_len = all.len().min(n as usize);
        if head_len > 1 {
            let head: Vec<_> = all.drain(..head_len).collect();
            let docs: Vec<String> = head
                .iter()
                .map(|s| dragnet_semantic::text::doc_text(&s.name, &s.category))
                .collect();
            let qtext = plan.semantic_text.clone();
            let rr2 = Arc::clone(&rr);
            let scores = tokio::task::spawn_blocking(move || rr2.score(&qtext, &docs))
                .await
                .ok()
                .and_then(|r| r.ok());
            // Güven kapısı: cross-encoder'ın en iyi skoru eşiğin altındaysa ve adlarda
            // sorgu kelimesi geçmiyorsa, bu sorgunun korpusta karşılığı yok demektir.
            if let Some(sc) = &scores {
                let best = sc.iter().copied().fold(f32::MIN, f32::max);
                if best < dragnet_semantic::WEAK_MATCH_SCORE && !show_weak {
                    let names: Vec<String> = head.iter().map(|s| s.name.clone()).collect();
                    let probe = if plan.fts_text.is_empty() {
                        q
                    } else {
                        plan.fts_text.as_str()
                    };
                    if !has_lexical_evidence(&names, probe) {
                        return Ok(SearchOutcome {
                            rows: Vec::new(),
                            used,
                            weak: true,
                        });
                    }
                }
            }
            let head = match scores {
                Some(sc) if sc.len() == head.len() => {
                    let mut order: Vec<usize> = (0..head.len()).collect();
                    order.sort_by(|&x, &y| sc[y].total_cmp(&sc[x]));
                    let mut opt: Vec<Option<_>> = head.into_iter().map(Some).collect();
                    order
                        .into_iter()
                        .filter_map(|i| opt[i].take())
                        .collect::<Vec<_>>()
                }
                _ => head,
            };
            let mut merged = head;
            merged.extend(all);
            all = merged;
        }
        let start = (offset.max(0) as usize).min(all.len());
        let end = (start + limit.max(0) as usize).min(all.len());
        all[start..end].to_vec()
    } else {
        store
            .search_hybrid_boosted(fts_query, &ids, limit, offset, sort, desc, filter, &boost)
            .await?
    };
    Ok(SearchOutcome {
        rows,
        used,
        weak: false,
    })
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
            false,
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
            false,
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
            false,
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
            false,
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
            false,
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
            false,
        )
        .await
        .unwrap();
        assert_eq!(r.used, SearchMode::Fts);
    }
}
