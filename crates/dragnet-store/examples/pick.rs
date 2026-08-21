// SPDX-License-Identifier: AGPL-3.0-only
//! Ölçüm araçları için canlı (peer'i olan) infohash seçer: `pick <db> [adet]`.
use sqlx::Row;

#[tokio::main]
async fn main() {
    let a: Vec<String> = std::env::args().collect();
    let n: i64 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(5);
    let store = dragnet_store::Store::open(&a[1]).await.expect("db");
    let rows = sqlx::query(
        "SELECT infohash, probe_peers FROM torrents
          WHERE metadata_status='pending' AND probe_peers > 0
          ORDER BY probe_peers DESC LIMIT ?1",
    )
    .bind(n)
    .fetch_all(store.pool())
    .await
    .expect("sorgu");
    for r in rows {
        println!(
            "{} {}",
            r.get::<String, _>("infohash"),
            r.get::<i64, _>("probe_peers")
        );
    }
}
