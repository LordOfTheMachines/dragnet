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
    // ÖNEMLİ: triyaj ve çekim aşamaları işini bitirince kaydı SİLİYOR (sıfır peer →
    // `delete_pending`, deneme hakkı bitti → `mark_fetch_failed`). Bu yüzden hızlarını
    // tablodaki satırları sayarak ölçmek YANILTIR — silinenler görünmez. Ölçümde tam
    // olarak bu tuzağa düşüldü: satır sayımı triyajı 1.317/saat gösterirken gerçek hız
    // (backlog düşüşünden) ~11.000/saat idi. Bu yüzden olay sayaçları (`metrics`) okunur.
    let win_secs = win_min * 60;
    let m = |name: &'static str| {
        let store = store.clone();
        async move { store.metric_since(name, now, win_secs).await.unwrap_or(0) }
    };
    let probed = m(dragnet_store::metric::TRIAGE_DONE).await;
    let triage_dead = m(dragnet_store::metric::TRIAGE_DEAD).await;
    let attempted = m(dragnet_store::metric::FETCH_ATTEMPT).await;
    let succeeded = m(dragnet_store::metric::FETCH_OK).await;
    let hinted_ok = m(dragnet_store::metric::FETCH_OK_HINTED).await;
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
        println!("  triyajda ölü çıkan       : %{:.1}  ({triage_dead})", 100.0 * triage_dead as f64 / probed as f64);
        println!("  triyaj → aday dönüşümü   : %{:.1}", 100.0 * (probed - triage_dead) as f64 / probed as f64);
    }
    if succeeded > 0 {
        // F13 kazancının doğrudan kanıtı: DHT araması hiç yapılmadan biten çekimler.
        println!("  DHT aramasız başarı      : %{:.1}  ({hinted_ok}/{succeeded})", 100.0 * hinted_ok as f64 / succeeded as f64);
    }
    // HARVESTER: hasat düşükse sebebi burada görünür — aktif örnekleme mi durdu
    // (samples), yoksa ağ bizi tanımıyor mu (announce/get_peers sıfıra yakın)?
    let samples = m(dragnet_store::metric::DHT_SAMPLES).await;
    let announce = m(dragnet_store::metric::DHT_ANNOUNCE).await;
    let gp = m(dragnet_store::metric::DHT_GET_PEERS).await;
    let sent = m(dragnet_store::metric::DHT_QUERIES_SENT).await;
    let limited = m(dragnet_store::metric::DHT_RATE_LIMITED).await;
    println!("\nHARVESTER (DHT)");
    println!("  BEP-51 örnek (aktif)      : {samples:>7}  → {:>8.1}/sn", samples as f64 / win_secs as f64);
    println!("  gelen announce (pasif)    : {announce:>7}  → {:>8.0}/saat", per_h(announce));
    println!("  gelen get_peers (pasif)   : {gp:>7}  → {:>8.0}/saat", per_h(gp));
    println!("  gönderilen sorgu          : {sent:>7}  → {:>8.1}/sn", sent as f64 / win_secs as f64);
    if sent + limited > 0 {
        println!("  rate-limit ile düşen      : {limited:>7}  (%{:.0} talep reddedildi)",
            100.0 * limited as f64 / (sent + limited) as f64);
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
    // Sorgu metinleri `dragnet_store::queries` içinden gelir — yani ÇALIŞAN sorgunun
    // planı gösterilir. (Bir kez elle kopyalanmıştı ve kopya bayatlayınca bu araç,
    // düzeltilmiş sorgular için hâlâ "TEMP B-TREE" raporladı.)
    use dragnet_store::queries;
    for (ad, sql) in [
        ("next_to_triage", queries::NEXT_TO_TRIAGE),
        ("next_to_fetch/canlı", queries::NEXT_TO_FETCH_LIVE),
        ("next_to_fetch/garbled", queries::NEXT_TO_FETCH_GARBLED),
        ("count_pending", queries::COUNT_PENDING),
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
