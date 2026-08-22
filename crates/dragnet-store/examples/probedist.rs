// SPDX-License-Identifier: AGPL-3.0-only
//! Teşhis: çekim kuyruğunun GERÇEK bileşimi. `probedist <db>`
//!
//! "Neden çekim başına yalnız 2 peer görüyoruz?" sorusunun cevabı burada: kuyruğa
//! giren adayların triyajda ÖLÇÜLEN peer sayısı kaç? Eşik 1 ise, kuyruk 1-2 peer'li
//! adaylarla dolar ve `P(başarı) = 1-(1-p)^n` denklemi n≈1'de %5'te kalır.
use sqlx::Row;

#[tokio::main]
async fn main() {
    let a: Vec<String> = std::env::args().collect();
    let store = dragnet_store::Store::open(&a[1]).await.expect("db");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    println!("ÖLÇÜLEN PEER DAĞILIMI (bekleyen, triyajdan geçmiş)");
    let rows = sqlx::query(
        "SELECT CASE
                  WHEN probe_peers = 0 THEN '0 (ölü)'
                  WHEN probe_peers = 1 THEN '1'
                  WHEN probe_peers = 2 THEN '2'
                  WHEN probe_peers <= 4 THEN '3-4'
                  WHEN probe_peers <= 9 THEN '5-9'
                  WHEN probe_peers <= 15 THEN '10-15'
                  ELSE '16+ (sınırda)'
                END AS kova,
                COUNT(*) AS n
           FROM torrents
          WHERE metadata_status='pending' AND probe_at > 0 AND probe_peers >= 0
          GROUP BY kova ORDER BY MIN(probe_peers)",
    )
    .fetch_all(store.pool())
    .await
    .unwrap_or_default();
    let total: i64 = rows.iter().map(|r| r.get::<i64, _>("n")).sum();
    for r in &rows {
        let n = r.get::<i64, _>("n");
        println!(
            "  {:<14} {:>8}  %{:.1}",
            r.get::<String, _>("kova"),
            n,
            100.0 * n as f64 / total.max(1) as f64
        );
    }

    // Kuyruğun ÇEKİME UYGUN kısmı: eşiği yükseltmek aday havuzunu ne kadar daraltır?
    println!("\nEŞİK SENARYOLARI (son 2 saatte ölçülmüş, denenmemiş adaylar)");
    println!(
        "{:<12} {:>10} {:>14}",
        "min peer", "aday", "beklenen başarı"
    );
    for min in [1i64, 2, 3, 5, 8] {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM torrents
              WHERE metadata_status='pending' AND fetch_attempts = 0
                AND probe_at > ?1 AND probe_peers >= ?2",
        )
        .bind(now - 7200)
        .bind(min)
        .fetch_one(store.pool())
        .await
        .unwrap_or(0);
        // P(başarı) = 1-(1-p)^n, p ≈ 0,05 (ölçülen handshake oranı mertebesi)
        let p = 1.0 - 0.95f64.powi(min as i32);
        println!("  >= {min:<9} {n:>10} {:>13.0}%", 100.0 * p);
    }

    for (ad, sql) in [
        (
            "son 1 saatte ölçülmüş",
            "SELECT COUNT(*) FROM torrents WHERE metadata_status='pending' AND probe_at > ?1",
        ),
        (
            "  · bunlardan >= 3 peer",
            "SELECT COUNT(*) FROM torrents WHERE metadata_status='pending' AND probe_at > ?1 AND probe_peers >= 3",
        ),
    ] {
        let n: i64 = sqlx::query_scalar(sql)
            .bind(now - 3600)
            .fetch_one(store.pool())
            .await
            .unwrap_or(0);
        println!("{ad}: {n}");
    }
}
