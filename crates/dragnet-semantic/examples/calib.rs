// SPDX-License-Identifier: AGPL-3.0-only
//! Skor kalibrasyonu: gerçek modelle bir ad listesini indeksleyip sorguların top-20 skorlarını
//! yazdırır — `min_score` / göreli kesim eşiğini seçmek için. `calib <tier> <names.txt> <q1> <q2>...`
use dragnet_core::InfoHash;
use dragnet_semantic::{Device, Semantic, SemanticConfig, Tier};

fn main() {
    let a: Vec<String> = std::env::args().collect();
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
        .collect();
    let items: Vec<(InfoHash, String)> = names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let mut b = [0u8; 20];
            b[..4].copy_from_slice(&(i as u32).to_le_bytes());
            (InfoHash::from_bytes(b), n.clone())
        })
        .collect();
    let t = std::time::Instant::now();
    for chunk in items.chunks(256) {
        sem.embed_and_add(chunk).unwrap();
    }
    let floor = sem.calibrate_noise().unwrap();
    eprintln!(
        "{} ad indekslendi ({:?}, {}); gürültü tabanı={floor:.3}",
        names.len(),
        t.elapsed(),
        sem.device()
    );
    for q in &a[3..] {
        let hits = if std::env::var("RAW").is_ok() {
            sem.search_raw(q, 20).unwrap()
        } else {
            sem.search(q, 20).unwrap()
        };
        println!("\nQ: {q}");
        for h in hits {
            let i = u32::from_le_bytes(h.infohash.as_bytes()[..4].try_into().unwrap()) as usize;
            println!("  {:.3}  {}", h.score, names[i]);
        }
    }
}
