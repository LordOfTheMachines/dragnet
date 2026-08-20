// SPDX-License-Identifier: AGPL-3.0-only
//! Teşhis: çekim kuyruğunun **bileşimi** ve başarı oranları. `queue <db yolu>`
//! "Neden yavaş indeksliyoruz?" sorusunun cevabı buradadır: kuyrukta sıcak (yakın
//! zamanda gerçek trafikte görülmüş, dolayısıyla canlı olma ihtimali yüksek) kayıt mı
//! var, yoksa BEP-51 örneklemesinden gelen soğuk/ölü yığın mı?
use sqlx::Row;

#[tokio::main]
async fn main() {
    let a: Vec<String> = std::env::args().collect();
    let store = dragnet_store::Store::open(&a[1]).await.expect("db");
    let now = chrono_now();
    let q = |sql: String| {
        let store = store.clone();
        async move {
            sqlx::query(&sql)
                .fetch_one(store.pool())
                .await
                .map(|r| r.get::<i64, _>(0))
                .unwrap_or(-1)
        }
    };
    let hot2h = q(format!(
        "SELECT COUNT(*) FROM torrents WHERE metadata_status='pending' AND hot_seen > {}",
        now - 7200
    ))
    .await;
    let hinted = q(
        "SELECT COUNT(*) FROM torrents WHERE metadata_status='pending' AND hint_peers > 0"
            .to_string(),
    )
    .await;
    let pending =
        q("SELECT COUNT(*) FROM torrents WHERE metadata_status='pending'".to_string()).await;
    let unreach =
        q("SELECT COUNT(*) FROM torrents WHERE metadata_status='unreachable'".to_string()).await;
    let fetched =
        q("SELECT COUNT(*) FROM torrents WHERE metadata_status='fetched'".to_string()).await;
    let alive = q("SELECT COUNT(*) FROM torrents WHERE peer_count > 0".to_string()).await;
    let dead = q("SELECT COUNT(*) FROM torrents WHERE peer_count = 0".to_string()).await;
    let unchecked = q(
        "SELECT COUNT(*) FROM torrents WHERE metadata_status='fetched' AND last_check IS NULL"
            .to_string(),
    )
    .await;
    let tried = q("SELECT COUNT(*) FROM torrents WHERE fetch_attempts > 0".to_string()).await;
    let last1h = q(format!(
        "SELECT COUNT(*) FROM torrents WHERE metadata_status='fetched' AND fetched_at > {}",
        now - 3600
    ))
    .await;
    let oldest_check = q("SELECT COALESCE(MIN(last_check), 0) FROM torrents WHERE metadata_status='fetched' AND last_check IS NOT NULL".to_string()).await;

    println!("KUYRUK BİLEŞİMİ");
    println!("  bekleyen (pending)      : {pending}");
    println!("    · sıcak (son 2 saat)  : {hot2h}");
    println!("    · peer ipuçlu         : {hinted}");
    println!("    · soğuk (kalan)       : {}", pending - hot2h - hinted);
    println!("  ulaşılamayan            : {unreach}");
    println!("  denenmiş (en az 1 kez)  : {tried}");
    println!("\nİNDEKS");
    println!("  adlı kayıt              : {fetched}   (son 1 saatte: {last1h})");
    println!("  canlı / ölü / kontrolsüz: {alive} / {dead} / {unchecked}");
    if oldest_check > 0 {
        let age_h = (now - oldest_check) as f64 / 3600.0;
        println!("  en eski canlılık kontrolü: {age_h:.1} saat önce (döngü süresi)");
    }
    println!("\nYORUM: kuyruğun büyük kısmı soğuksa (BEP-51 örneklemesi), çekim denemelerinin");
    println!("çoğu ölü torrent'e gider ve başarı oranı düşer. Sıcak kayıtlar gerçek trafikten");
    println!("gelir — onları artırmanın yolu daha çok gelen announce/get_peers almaktır.");
}

fn chrono_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
