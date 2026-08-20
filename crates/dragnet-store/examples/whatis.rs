// SPDX-License-Identifier: AGPL-3.0-only
//! Teşhis: bir kaydın **embed edilen metnini** ve dosya listesini gösterir.
//! `whatis <db yolu> <infohash|ad parçası>` — adı anlamsız kayıtların (ör. "s01")
//! dosya adlarıyla nasıl anlaşılır hâle geldiğini görmek için.
use dragnet_store::Store;

#[tokio::main]
async fn main() {
    let a: Vec<String> = std::env::args().collect();
    let store = Store::open(&a[1]).await.expect("db");
    let needle = a.get(2).cloned().unwrap_or_default();

    // Önce infohash olarak dene, olmazsa ada göre ara.
    let hashes: Vec<dragnet_core::InfoHash> = match dragnet_core::InfoHash::from_hex(&needle) {
        Some(ih) => vec![ih],
        None => store
            .search_paged(
                &needle,
                5,
                0,
                dragnet_store::SortKey::Relevance,
                true,
                &dragnet_store::Filter::default(),
            )
            .await
            .expect("arama")
            .into_iter()
            .map(|s| s.infohash)
            .collect(),
    };
    for ih in hashes {
        let Some(rec) = store.get(ih).await.expect("get") else {
            println!("bulunamadı: {ih}");
            continue;
        };
        // Üretimdeki gibi: kategori ad + DOSYA UZANTILARIYLA birlikte hesaplanır.
        let cat = rec.category();
        let cat_name_only = dragnet_core::categorize(&rec.name, &[]);
        let files: Vec<String> = rec.files.iter().map(|f| f.path.clone()).collect();
        println!(
            "\n=== {} ({} dosya, {} bayt)",
            rec.name,
            files.len(),
            rec.total_size
        );
        println!("kategori: {cat} (yalnız addan olsaydı: {cat_name_only})");
        for f in rec.files.iter().take(6) {
            println!("   · {} ({} bayt)", f.path, f.size);
        }
        if files.len() > 6 {
            println!("   … {} dosya daha", files.len() - 6);
        }
        println!(
            "EMBED EDİLEN METİN:\n   {}",
            dragnet_semantic::text::doc_text_with_files(
                &rec.name,
                cat,
                &files,
                dragnet_semantic::DOC_FILES_MAX_CHARS
            )
        );
    }
}
