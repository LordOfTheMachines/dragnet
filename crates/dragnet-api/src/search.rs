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
    /// Yazım düzeltmesi uygulandıysa düzeltilmiş sorgu ("hery poter" → "harry potter").
    /// UI kullanıcıya "… olarak arandı" der. Bkz. `dragnet_core::spell`.
    pub corrected: Option<String>,
}

/// Yazım düzeltme adaylarından korpusta **gerçekten geçeni** seçer: kelime kelime en sık
/// adayı almak "hery poter" → "hero peter" gibi anlamsız sonuç veriyordu; kombinasyonlar
/// FTS eşleşme sayısıyla doğrulanır ve en çok eşleşen kazanır ("harry potter").
async fn best_candidate(
    store: &Store,
    spell: &dragnet_core::spell::SpellIndex,
    query: &str,
) -> Option<String> {
    // Adaylar zaten (mesafe, sonra frekans) sırasında: korpusta karşılığı olan İLK aday
    // seçilir. "En çok eşleşen"i seçmek yanlıştı — "mtrix" için nadir ama doğru "matrix"
    // yerine sık geçen ama uzak bir terim kazanabiliyordu.
    for cand in spell.candidates(query, 24) {
        if store.count_fts_matches(&cand).await > 0 {
            return Some(cand);
        }
    }
    None
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
///
/// Yazım düzeltme (F4-2) **yalnız sonuç bulunamadığında** devreye girer: sorgu güven
/// kapısına takılırsa indeksin sözlüğüne göre düzeltilip **bir kez** yeniden aranır ve
/// düzeltilmiş sorgu da sonuç veriyorsa onun sonuçları döner (`corrected` dolu). Böylece
/// çalışan sorgulara hiç dokunulmaz — ölçüm: düzeltmeyi her sorguya uygulamak
/// hit@5'i %84'ten %74'e düşürüyordu (Türkçe kelimeler İngilizce korpusta doğal olarak
/// "bilinmeyen" olduğu için yanlış düzeltiliyordu).
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
    show_weak: bool,
) -> Result<SearchOutcome, StoreError> {
    // Kısa sorguda (≤2 kelime) hiçbir kelime indeksin sözlüğünde yoksa ve sorgu hiçbir
    // kayıtla eşleşmiyorsa, arama yapmadan önce yazımı düzelt: "mtrix" gibi tek kelimelik
    // hatalar zayıf da olsa sonuç döndürdüğü için güven kapısına takılmıyor, dolayısıyla
    // aşağıdaki "bulunamadı → düzelt" yolu devreye girmiyordu. Uzun doğal dil sorgularına
    // dokunulmaz: Türkçe kelimeler korpusta zaten yoktur, düzeltmek onları bozar.
    let mut query = query;
    let mut pre_fix: Option<String> = None;
    let toks: Vec<&str> = query.split_whitespace().collect();
    if (1..=2).contains(&toks.len()) {
        if let Some(spell) = store.spell().await {
            let all_unknown = toks
                .iter()
                .all(|t| t.chars().count() >= 4 && !spell.contains(t));
            if all_unknown && store.count_fts_matches(query).await == 0 {
                match best_candidate(store, &spell, query).await {
                    Some(fixed) => {
                        pre_fix = Some(fixed);
                        query = pre_fix.as_deref().unwrap_or(query);
                    }
                    // Tek kelimelik, sözlükte olmayan, düzeltilemeyen ve tanıdık bir
                    // sinyal taşımayan sorgu ("mtrix"): korpusta karşılığı yok demektir.
                    // Cross-encoder böyle sorgularda yanıltıcı olabiliyor (ölçüm: "mtrix"
                    // için "Metro Simulator 2" −1.98 ile kapıdan geçiyordu).
                    None if toks.len() == 1
                        && !dragnet_semantic::query::understand(query).recognized =>
                    {
                        return Ok(SearchOutcome {
                            rows: Vec::new(),
                            used: if mode == SearchMode::Fts {
                                SearchMode::Fts
                            } else {
                                SearchMode::Hybrid
                            },
                            weak: true,
                            corrected: None,
                        });
                    }
                    None => {}
                }
            }
        }
    }
    let out = search_once(
        store, slot, query, mode, limit, offset, sort, desc, filter, show_weak,
    )
    .await?;
    if !out.weak {
        return Ok(SearchOutcome {
            corrected: pre_fix.or(out.corrected),
            ..out
        });
    }
    // Sonuç yok → "bunu mu demek istediniz": düzeltilmiş sorguyu bir kez dene.
    let Some(spell) = store.spell().await else {
        return Ok(out);
    };
    let Some(fixed) = best_candidate(store, &spell, query.trim()).await else {
        return Ok(out);
    };
    let retry = search_once(
        store, slot, &fixed, mode, limit, offset, sort, desc, filter, show_weak,
    )
    .await?;
    // Düzeltilmiş sorgu da karşılıksızsa orijinal "bulunamadı" sonucunu koru.
    if retry.weak || retry.rows.is_empty() {
        return Ok(out);
    }
    Ok(SearchOutcome {
        corrected: Some(fixed),
        ..retry
    })
}

#[allow(clippy::too_many_arguments)]
async fn search_once(
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
            corrected: None,
        });
    }
    // Sorgu anlama: dolgu temizliği, kategori niyeti, yıl aralığı (bkz.
    // `dragnet_semantic::query`). FTS-yalnız modda da dolgu temizliği uygulanır (dolgu FTS'i
    // de bozar) — niyet artırması yalnız hibrit yolda (saf FTS sözleşmesi değişmesin).
    let plan = dragnet_semantic::query::understand(q);
    // Kategori-yalnız sorgu ("oyunlar", "tüm filmler") bir gözatma isteğidir: adında o
    // kelime geçenleri değil, **o kategorideki her şeyi** listele (kullanıcı geri bildirimi).
    if plan.category_only && filter.category.is_none() {
        if let Some(cat) = plan.category {
            let mut f = filter.clone();
            f.category = Some(cat.to_string());
            let sk = if matches!(sort, SortKey::Relevance) {
                SortKey::Seen
            } else {
                sort
            };
            let rows = store.list_paged(limit, offset, sk, desc, &f).await?;
            return Ok(SearchOutcome {
                rows,
                used: SearchMode::Fts,
                weak: false,
                corrected: None,
            });
        }
    }
    let corrected = None;
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
            corrected: None,
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
                            corrected,
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
        corrected,
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
