// SPDX-License-Identifier: AGPL-3.0-only
//! Teşhis: boru hattının **aşama aşama gerçek hızı**. `rate <db yolu> [pencere_dk]`
//!
//! "Neden yavaş?" sorusunun cevabı tek bir sayıda değil, aşamalar arasındaki ORANDA:
//! hasat → triyaj → çekim denemesi → başarı. Hangi aşama bir sonrakini besleyemiyorsa
//! darboğaz odur. Ayrıca "hazır bekleyen aday" sayısı, işçilerin aç mı tok mu olduğunu
//! söyler: aday çoksa ama deneme azsa darboğaz çekim tarafındadır.
use sqlx::Row;

#[tokio::main]
async fn main() {
    let a: Vec<String> = std::env::args().collect();
    let win_min: i64 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(60);
    let store = dragnet_store::Store::open(&a[1]).await.expect("db");
    let now = now_unix();
    let since = now - win_min * 60;
    let n = |sql: String| {
        let store = store.clone();
        async move {
            sqlx::query(&sql)
                .fetch_one(store.pool())
                .await
                .map(|r| r.get::<i64, _>(0))
                .unwrap_or(-1)
        }
    };

    let discovered = n(format!("SELECT COUNT(*) FROM torrents WHERE first_seen > {since}")).await;
    let probed = n(format!(
        "SELECT COUNT(*) FROM torrents WHERE probe_at > {since}"
    ))
    .await;
    let attempted = n(format!(
        "SELECT COUNT(*) FROM torrents WHERE last_attempt > {since}"
    ))
    .await;
    let succeeded = n(format!(
        "SELECT COUNT(*) FROM torrents WHERE fetched_at > {since}"
    ))
    .await;
    // Çekime HAZIR ama henüz hiç denenmemiş sağlıklı aday: işçiler aç mı?
    let ready = n(format!(
        "SELECT COUNT(*) FROM torrents WHERE metadata_status='pending' AND fetch_attempts = 0
           AND (probe_peers >= {p} OR hint_peers >= {p})",
        p = dragnet_store::MIN_HEALTHY_PEERS
    ))
    .await;
    let ready_retry = n(format!(
        "SELECT COUNT(*) FROM torrents WHERE metadata_status='pending' AND fetch_attempts > 0
           AND fetch_attempts < {m} AND last_attempt < {c}
           AND (probe_peers >= {p} OR hint_peers >= {p})",
        m = dragnet_store::MAX_FETCH_ATTEMPTS,
        c = now - dragnet_store::HOT_RETRY_COOLDOWN_SECS,
        p = dragnet_store::MIN_HEALTHY_PEERS
    ))
    .await;
    let triage_backlog = n(
        "SELECT COUNT(*) FROM torrents WHERE metadata_status='pending' AND probe_at = 0".to_string(),
    )
    .await;
    // Triyajı işaretlenmiş ama sonucu yazılmamış (probe_at>0 & probe_peers=-1): sızıntı.
    let probe_leaked = n(
        "SELECT COUNT(*) FROM torrents WHERE metadata_status='pending' AND probe_at > 0 AND probe_peers < 0"
            .to_string(),
    )
    .await;

    let per_h = |v: i64| v as f64 * 60.0 / win_min as f64;
    println!("PENCERE: son {win_min} dakika\n");
    println!("AŞAMA HIZI (saatlik projeksiyon)");
    println!("  1. hasat (yeni infohash)  : {discovered:>7}  → {:>8.0}/saat", per_h(discovered));
    println!("  2. triyaj (peer ölçümü)   : {probed:>7}  → {:>8.0}/saat", per_h(probed));
    println!("  3. çekim denemesi         : {attempted:>7}  → {:>8.0}/saat", per_h(attempted));
    println!("  4. BAŞARI (ad indekslendi): {succeeded:>7}  → {:>8.0}/saat", per_h(succeeded));
    if attempted > 0 {
        println!("\n  deneme başına başarı     : %{:.1}", 100.0 * succeeded as f64 / attempted as f64);
    }
    if probed > 0 {
        println!("  triyaj → deneme aktarımı : %{:.1}", 100.0 * attempted as f64 / probed as f64);
    }
    println!("\nADAY STOKU (çekim işçileri aç mı?)");
    println!("  hazır, hiç denenmemiş     : {ready}");
    println!("  hazır, yeniden deneme     : {ready_retry}");
    println!("  triyaj bekleyen           : {triage_backlog}");
    println!("  triyaj SIZINTISI (yarım)  : {probe_leaked}");
    // SORGU PLANI: boru hattının sıcak sorguları saniyede birkaç kez çalışır; biri
    // indeks yerine tam tarama + geçici sıralama yapıyorsa kuyruk büyüdükçe zamanlayıcı
    // yavaşlar ve işçiler beklemede kalır (darboğaz ağ değil, SQLite olur).
    println!("\nSICAK SORGU PLANLARI (SCAN = tam tarama, TEMP B-TREE = geçici sıralama)");
    for (ad, sql) in [
        (
            "next_to_triage",
            "SELECT infohash FROM torrents WHERE metadata_status='pending' AND probe_at=0
               ORDER BY (hot_seen IS NOT NULL AND hot_seen > 0) DESC, hint_peers DESC, last_seen DESC LIMIT 8",
        ),
        (
            "next_to_fetch/canlı",
            "SELECT infohash FROM torrents WHERE metadata_status='pending'
                AND (fetch_attempts = 0 OR (fetch_attempts < 3 AND last_attempt < 0))
                AND (probe_peers >= 1 OR hint_peers >= 1 OR (probe_peers < 0 AND hint_peers > 0)
                     OR (probe_peers < 0 AND hot_seen IS NOT NULL AND hot_seen > 0))
              ORDER BY probe_peers DESC, hint_peers DESC, hot_seen DESC, seen_count DESC LIMIT 24",
        ),
        (
            "next_to_fetch/soğuk",
            "SELECT infohash FROM torrents WHERE (metadata_status='pending'
                 AND (fetch_attempts = 0 OR (fetch_attempts < 3 AND last_attempt < 0))
                 AND hint_peers = 0 AND (hot_seen IS NULL OR hot_seen <= 0))
                OR (metadata_status='fetched' AND garbled = 1 AND fetch_attempts = 0)
              ORDER BY seen_count DESC, hot_count DESC, last_seen DESC LIMIT 6",
        ),
        (
            "count_pending",
            "SELECT COUNT(*) FROM torrents WHERE metadata_status='pending'",
        ),
    ] {
        let rows = sqlx::query(&format!("EXPLAIN QUERY PLAN {sql}"))
            .fetch_all(store.pool())
            .await
            .unwrap_or_default();
        let plan: Vec<String> = rows.iter().map(|r| r.get::<String, _>("detail")).collect();
        println!("  {ad}:");
        for p in plan {
            println!("      {p}");
        }
    }

    println!("\nYORUM: 'hazır' stoğu büyük ve 'çekim denemesi' hızı düşükse darboğaz ÇEKİM");
    println!("tarafındadır (işçi sayısı / çekim süresi). 'hazır' sıfıra yakınsa darboğaz");
    println!("triyaj ya da hasattadır. 'triyaj sızıntısı' büyükse ölçüm yarıda kalıyor.");
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
