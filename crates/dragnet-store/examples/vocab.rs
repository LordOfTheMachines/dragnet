// SPDX-License-Identifier: AGPL-3.0-only
//! Yazım sözlüğü teşhisi: `vocab <db yolu> [kelime...]` — sözlük büyüklüğü ve
//! verilen kelimeler için (bilinir mi / öneri ne) çıktısı.
#[tokio::main]
async fn main() {
    let a: Vec<String> = std::env::args().collect();
    let store = dragnet_store::Store::open(&a[1]).await.expect("db");
    match store.spell_index(300_000).await {
        Ok(idx) => {
            println!("sözlük: {} terim", idx.len());
            for w in &a[2..] {
                println!(
                    "  {w}: bilinen={} öneri={:?} adaylar={:?}",
                    idx.contains(w),
                    idx.suggest(w),
                    idx.candidates(w, 6)
                );
            }
        }
        Err(e) => println!("sözlük kurulamadı: {e}"),
    }
}
