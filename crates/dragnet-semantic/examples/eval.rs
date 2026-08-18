// SPDX-License-Identifier: AGPL-3.0-only
//! Semantik arama değerlendirmesi: `eval <tier> <names.txt> [--plan]`
//! Sabit sorgu seti (TR/EN, doğal dil, yazım hatalı, dönem/kategori niyetli) ile
//! hits@5 (beklenen alt-dizeler) ölçer. `--plan`: sorgu anlama uygulanır (metin
//! temizliği + kategori artırması, üretim yoluyla aynı mantık). Doküman metni üretimdeki
//! gibi `text::doc_text(name, kategori)` (kategori `dragnet_core::categorize(name, [])`).
use dragnet_core::InfoHash;
use dragnet_semantic::{query, text, Device, Semantic, SemanticConfig, Tier};

const QUERIES: &[(&str, &[&str])] = &[
    (
        "içinde heroes geçen oyunları listeler misin",
        &["Heroes of Might", "Heroes.of.Might"],
    ),
    (
        "zombi konulu oyunları listeler misin",
        &["Plants_vs_Zombies"],
    ),
    ("resident evil filmleri", &["Resident Evil"]),
    ("büyük tavşan animasyonu", &["Big Buck Bunny"]),
    ("çelik gözyaşları", &["Tears of Steel"]),
    (
        "linux işletim sistemi",
        &["ubuntu", "debian", "linuxmint", "archlinux", "xubuntu"],
    ),
    (
        "linux dağıtımı iso",
        &["ubuntu", "debian", "linuxmint", "archlinux", "xubuntu"],
    ),
    ("witcher 3", &["Witcher 3"]),
    ("doom oyunu", &["DOOM"]),
    ("harry potter filmi", &["Harry.Potter", "Harry Potter"]),
    ("game of thrones", &["Game of Thrones", "Game.of.Thrones"]),
    (
        "taht oyunları dizisi",
        &["Game of Thrones", "Game.of.Thrones"],
    ),
    ("batman 2022", &["The.Batman.2022"]),
    ("mozart klasik müzik", &["Mozart"]),
    (
        "japon animesi",
        &["Anime", "Jujutsu", "Spy.x.Family", "Naruto"],
    ),
    ("hery poter", &["Harry.Potter", "Harry Potter"]),
    ("büyücü çocuk filmi", &["Harry.Potter", "Harry Potter"]),
    (
        "kahramanlar strateji oyunu",
        &["Heroes of Might", "Heroes.of.Might"],
    ),
    (
        "2000'lerin bilim kurgu filmleri",
        &[
            "Resident Evil Extinction",
            "Matrix",
            "Minority",
            "Equilibrium",
        ],
    ),
    ("asdkjhqwe zxcv", &[]),
    ("qwrtyzx plmokn", &[]),
    ("sdfgh jklş", &[]),
    ("zeplin belgeseli", &[]),
    ("kuantum fiziği ders notları", &[]),
];

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let use_plan = a.iter().any(|x| x == "--plan");
    let use_rerank = a.iter().any(|x| x == "--rerank");
    let rr_dev = if a.iter().any(|x| x == "--cpu") {
        Device::Cpu
    } else {
        Device::Auto
    };
    let reranker = if use_rerank {
        Some(
            dragnet_semantic::rerank::Reranker::load(
                std::path::Path::new("C:/dgcache/dragnet-models"),
                rr_dev,
            )
            .expect("reranker"),
        )
    } else {
        None
    };
    let mut rr_ms = 0u128;
    let cfg = SemanticConfig {
        tier: Tier::parse(&a[1]),
        device: Device::Auto,
        models_dir: "C:/dgcache/dragnet-models".into(),
    };
    let sem = Semantic::load(&cfg).expect("model");
    let names: Vec<String> = std::fs::read_to_string(&a[2])
        .unwrap()
        .lines()
        .map(|s| s.to_string())
        .filter(|s| !s.contains('\u{FFFD}'))
        .collect();
    let cats: Vec<&'static str> = names
        .iter()
        .map(|n| dragnet_core::categorize(n, &[]))
        .collect();
    let items: Vec<(InfoHash, String)> = names
        .iter()
        .zip(&cats)
        .enumerate()
        .map(|(i, (n, c))| {
            let mut b = [0u8; 20];
            b[..4].copy_from_slice(&(i as u32).to_le_bytes());
            (InfoHash::from_bytes(b), text::doc_text(n, c))
        })
        .collect();
    let t = std::time::Instant::now();
    for chunk in items.chunks(256) {
        sem.embed_and_add(chunk).unwrap();
    }
    let floor = sem.calibrate_noise().unwrap();
    eprintln!(
        "{} ad ({:?}, {}) taban={floor:.3} plan={use_plan}",
        names.len(),
        t.elapsed(),
        sem.device()
    );
    let (mut score, mut n, mut noise_ok, mut mrr) = (0.0f64, 0usize, true, 0.0f64);
    for (q, expected) in QUERIES {
        let (qtext, cat) = if use_plan {
            let p = query::understand(q);
            (p.semantic_text, p.category)
        } else {
            (q.to_string(), None)
        };
        let mut hits = sem.search(&qtext, 30).unwrap();
        // Güven sinyalleri (F4): (1) embedding top1, (2) top1 / kuyruk(6-20) oranı,
        // (3) reranker'ın en iyi skoru. Amaç: "bu sorgunun korpusta karşılığı var mı?"
        let mut rr_top = f32::NAN;
        let raw30 = sem.search_raw(&qtext, 20).unwrap();
        let emb_top = raw30.first().map(|h| h.score).unwrap_or(0.0);
        let tail: f32 = raw30.iter().skip(5).map(|h| h.score).sum::<f32>()
            / raw30.len().saturating_sub(5).max(1) as f32;
        let sig_ratio = if tail > 0.0 { emb_top / tail } else { 0.0 };
        if std::env::var("DEBUG").is_ok() {
            let raw = sem.search_raw(&qtext, 5).unwrap();
            eprintln!(
                "  [{qtext}] kesimli={} ham: {}",
                hits.len(),
                raw.iter()
                    .map(|h| format!(
                        "{:.3}:{}",
                        h.score,
                        names[idx(h.infohash)].chars().take(28).collect::<String>()
                    ))
                    .collect::<Vec<_>>()
                    .join(" | ")
            );
        }
        // Kategori artırması (üretimdeki RRF çarpanının semantik-yalnız benzeri): eşleşenleri öne al.
        if let Some(c) = cat {
            hits.sort_by(|x, y| {
                let cx = cats[idx(x.infohash)] == c;
                let cy = cats[idx(y.infohash)] == c;
                cy.cmp(&cx).then(y.score.total_cmp(&x.score))
            });
        }
        if let Some(rr) = &reranker {
            // Üretimdeki gibi: sorgu + adayın temiz doküman metni (kategori dahil).
            let docs: Vec<String> = hits
                .iter()
                .map(|h| items[idx(h.infohash)].1.clone())
                .collect();
            let t = std::time::Instant::now();
            let scores = rr.score(&qtext, &docs).unwrap();
            rr_ms += t.elapsed().as_millis();
            rr_top = scores.iter().copied().fold(f32::MIN, f32::max);
            let mut order: Vec<usize> = (0..hits.len()).collect();
            order.sort_by(|&x, &y| scores[y].total_cmp(&scores[x]));
            if std::env::var("FUSE").is_ok() {
                // RRF harmanı: rerank sırası (ağırlık 1.0) + orijinal sıra (ağırlık w).
                let w: f32 = std::env::var("FUSE")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0.5);
                let orig: Vec<usize> = (0..hits.len()).collect();
                let fused = dragnet_core::rank::rrf(&[order.clone(), orig], &[1.0, w], 60.0);
                order = fused.into_iter().map(|(i, _)| i).collect();
            }
            hits = order.into_iter().map(|i| hits[i]).collect();
        }
        // Üretimdeki güven kapısı (bkz. dragnet_semantic::WEAK_MATCH_SCORE): cross-encoder
        // hiçbir adayı alakalı bulmadıysa sonuç listesi boşalır.
        if rr_top.is_finite() && rr_top < dragnet_semantic::WEAK_MATCH_SCORE {
            hits.clear();
        }
        let top: Vec<&str> = hits
            .iter()
            .take(5)
            .map(|h| names[idx(h.infohash)].as_str())
            .collect();
        if expected.is_empty() {
            noise_ok = hits.is_empty();
            println!(
                "{:>5}  {q}  → {} sonuç (boş beklenir) | emb={emb_top:.3} oran={sig_ratio:.2} rr={rr_top:.2}",
                if hits.is_empty() { "OK" } else { "MISS" },
                hits.len()
            );
            continue;
        }
        let hit = top.iter().any(|t| expected.iter().any(|e| t.contains(e)));
        let rank = hits
            .iter()
            .position(|h| expected.iter().any(|e| names[idx(h.infohash)].contains(e)));
        n += 1;
        if hit {
            score += 1.0;
        }
        if let Some(r) = rank {
            mrr += 1.0 / (r as f64 + 1.0);
        }
        println!(
            "{:>5} r={:<3} {q}  → {}  | emb={emb_top:.3} oran={sig_ratio:.2} rr={rr_top:.2}",
            if hit { "OK" } else { "MISS" },
            rank.map(|r| (r + 1).to_string()).unwrap_or("-".into()),
            top.first()
                .map(|s| s.chars().take(60).collect::<String>())
                .as_deref()
                .unwrap_or("-")
        );
    }
    println!(
        "\n=== tier={} plan={use_plan} rerank={} hit@5={:.0}% ({}/{n}) MRR={:.2} anlamsız-boş={noise_ok} rerank_ort={} ms/sorgu",
        a[1], reranker.as_ref().map(|r| r.device()).unwrap_or("-"),
        100.0 * score / n as f64, score as usize, mrr / n as f64,
        if use_rerank { rr_ms / QUERIES.len() as u128 } else { 0 }
    );
}
fn idx(ih: InfoHash) -> usize {
    u32::from_le_bytes(ih.as_bytes()[..4].try_into().unwrap()) as usize
}
