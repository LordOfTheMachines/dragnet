// SPDX-License-Identifier: AGPL-3.0-only
//! Bekleyen (adsız) infohash yığınını siler; **adlı kayıtlar korunur**.
//! `reset_pending <db yolu> --evet`
//!
//! Gerekçe (kullanıcı teşhisi + ölçüm): BEP-51 ile toplanan 2 milyondan fazla infohash
//! metadata sırası gelene kadar ölüyor; kuyruk ölü kayıtlarla dolduğu için canlı ve
//! taze torrent'lere sıra gelmiyordu (peer denemelerinin %97'si zaman aşımı).
#[tokio::main]
async fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 3 || a[2] != "--evet" {
        eprintln!("kullanım: reset_pending <db yolu> --evet   (adlı kayıtlar korunur)");
        std::process::exit(2);
    }
    let store = dragnet_store::Store::open(&a[1]).await.expect("db");
    let before_named = store.count_fetched().await.unwrap_or(-1);
    let before_total = store.count_total().await.unwrap_or(-1);
    let deleted = store.reset_pending().await.expect("silme");
    let after_named = store.count_fetched().await.unwrap_or(-1);
    let after_total = store.count_total().await.unwrap_or(-1);
    println!("silinen bekleyen/ulaşılamayan : {deleted}");
    println!("adlı kayıt (önce → sonra)     : {before_named} → {after_named}");
    println!("toplam kayıt (önce → sonra)   : {before_total} → {after_total}");
}
