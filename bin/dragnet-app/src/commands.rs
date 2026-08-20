// SPDX-License-Identifier: AGPL-3.0-only
//! Frontend'in çağırdığı Tauri komutları.

use std::net::TcpStream;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tauri::{AppHandle, State};

use dragnet_api::SearchMode;
use dragnet_engine::Engine;
use dragnet_store::{Filter, Overview, SortKey, TorrentSummary};

use crate::settings::Settings;
use crate::{autostart, updater, AppState};

fn summary_json(s: &TorrentSummary) -> Value {
    json!({
        "infohash": s.infohash.to_hex(),
        "name": s.name,
        "size": s.total_size,
        "files": s.file_count,
        "seen": s.seen_count,
        "peers": s.peer_count, // null = henüz kontrol edilmedi, 0 = ölü, N = canlı
        "category": s.category,
        "first_seen": s.first_seen,
        "last_seen": s.last_seen,
        "magnet": s.infohash.to_magnet(Some(&s.name)),
    })
}

fn overview_json(o: &Overview) -> Value {
    json!({
        "fetched": o.fetched,
        "total_infohashes": o.total_infohashes,
        "total_size": o.total_size,
        "total_files": o.total_files,
        "total_peers": o.total_peers,
        "alive": o.alive,
        "dead": o.dead,
        "unchecked": o.unchecked,
        "categories": o.categories.iter()
            .map(|(c, n, s)| json!({ "category": c, "count": n, "size": s }))
            .collect::<Vec<_>>(),
    })
}

fn make_filter(
    hide_adult: bool,
    only_alive: bool,
    category: Option<String>,
    block_keywords: Vec<String>,
    hide_garbled: bool,
) -> Filter {
    Filter {
        only_alive,
        hide_adult,
        category: category.filter(|c| !c.is_empty() && c != "all"),
        block_keywords,
        hide_garbled,
    }
}

/// Ayarlardaki engel kelimelerini (normalize edilmiş) döndürür.
fn block_keywords(state: &AppState) -> Vec<String> {
    let s = state.settings.lock().unwrap_or_else(|p| p.into_inner());
    s.block_keywords
        .iter()
        .map(|k| k.trim().to_lowercase())
        .filter(|k| !k.is_empty())
        .collect()
}

fn summaries_json(v: &[TorrentSummary]) -> Vec<Value> {
    v.iter().map(summary_json).collect()
}

/// Sürüm bilgisi.
#[tauri::command]
pub fn app_info() -> Value {
    json!({ "name": "Dragnet", "version": env!("CARGO_PKG_VERSION") })
}

/// Canlı durum: tarama açık mı, indeks sayaçları, harvester metrikleri, hız.
#[tauri::command]
pub async fn get_stats(state: State<'_, AppState>) -> Result<Value, String> {
    let (scanning, harvester, addr, fetch, queue, dht_client) = {
        let guard = state.engine.lock().await;
        match guard.as_ref() {
            Some(e) => {
                let snap = e.snapshot().await;
                (
                    true,
                    Some(snap.harvester),
                    Some(e.harvester_addr().to_string()),
                    Some(snap.fetch),
                    Some(snap.queue),
                    Some(snap.dht_client),
                )
            }
            None => (false, None, None, None, None, None),
        }
    };

    let fetched = state.store.count_fetched().await.unwrap_or(0);
    let total = state.store.count_total().await.unwrap_or(0);

    // BEP-51 örnek/sn (delta).
    let samples = harvester.as_ref().map(|h| h.samples_seen).unwrap_or(0);
    let sample_rate = {
        let mut prev = state.rate_prev.lock().unwrap_or_else(|p| p.into_inner());
        let dt = prev.1.elapsed().as_secs_f64();
        let rate = if dt > 0.1 && samples >= prev.0 {
            (samples - prev.0) as f64 / dt
        } else {
            0.0
        };
        *prev = (samples, Instant::now());
        rate.round() as u64
    };

    let p = state.store.pressure();
    let storage = json!({
        "db_bytes": p.db_bytes, "free_bytes": p.free_bytes,
        "paused": p.paused, "reason": p.reason,
    });
    Ok(json!({
        "scanning": scanning,
        "fetched": fetched,
        "total": total,
        "harvester_addr": addr,
        "queries_sent": harvester.as_ref().map(|h| h.queries_sent).unwrap_or(0),
        "responses": harvester.as_ref().map(|h| h.responses_seen).unwrap_or(0),
        "samples": samples,
        "unique": harvester.as_ref().map(|h| h.unique_infohashes).unwrap_or(0),
        "peer_hints": harvester.as_ref().map(|h| h.peer_hints).unwrap_or(0),
        // Erişilebilirlik: gelen (pasif) sorgular — port dışarıdan açıksa dakikalar içinde
        // yüzlerce gelir; NAT arkasında ~0 kalır.
        "queries_seen": harvester.as_ref().map(|h| h.queries_seen).unwrap_or(0),
        "get_peers_seen": harvester.as_ref().map(|h| h.get_peers_seen).unwrap_or(0),
        "announce_seen": harvester.as_ref().map(|h| h.announce_seen).unwrap_or(0),
        "dht_client": dht_client.map(|(fw, pub_addr, port)| json!({ "firewalled": fw, "public": pub_addr, "port": port })),
        "sample_rate": sample_rate,
        // Faz E: çekim boru hattı sayaçları + kuyruk (pano "Metadata çekimi" kartı).
        "fetch": fetch.map(|f| json!({
            "attempts": f.attempts, "ok": f.ok, "no_peers": f.no_peers,
            "all_peers_failed": f.all_peers_failed, "avg_ms": f.avg_ms, "avg_peers": f.avg_peers,
        })),
        "queue": queue.map(|(p, h, u, r)| json!({ "pending": p, "hot": h, "unreachable": u, "fetched_last_hour": r })),
        "semantic": state.semantic.status_json().await,
        // Depolama basıncı (F8-4): büyüme duraklatıldıysa UI uyarır.
        "storage": storage,
    }))
}

/// Semantik arama durumu (aşama, indirme ilerlemesi, model/cihaz, indekslenen sayısı).
#[tauri::command]
pub async fn semantic_status(state: State<'_, AppState>) -> Result<Value, String> {
    Ok(state.semantic.status_json().await)
}

/// Bir torrent'in dosya listesi (F8-2): arayüzde ad'a tıklanınca ağaç olarak gösterilir.
/// Veri zaten `files` tablosunda; metadata çekilmemişse `null` döner.
#[tauri::command]
pub async fn torrent_files(state: State<'_, AppState>, infohash: String) -> Result<Value, String> {
    let Some(ih) = dragnet_core::InfoHash::from_hex(infohash.trim()) else {
        return Err("geçersiz infohash".into());
    };
    let rec = state.store.get(ih).await.map_err(|e| e.to_string())?;
    Ok(match rec {
        Some(r) => json!({
            "name": r.name,
            "total_size": r.total_size,
            "files": r.files.iter().map(|f| json!({ "path": f.path, "size": f.size })).collect::<Vec<_>>(),
        }),
        None => Value::Null,
    })
}

/// Birleşik arama/gözat (tek sıralanabilir tablo). Sorgu boşsa FTS yerine tüm
/// indeksi listeler (gözat). `sort`/`desc`/`offset` ile sunucu-tarafı sıralama +
/// sonsuz-scroll sayfalama; engel kelimeleri ayarlardan uygulanır.
// Tauri komutu: parametreler frontend'den tek tek gelir (bir struct'a sarmak
// invoke sözleşmesini gereksiz karmaşıklaştırır).
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn search(
    state: State<'_, AppState>,
    query: String,
    limit: Option<i64>,
    offset: Option<i64>,
    sort: Option<String>,
    desc: Option<bool>,
    hide_adult: Option<bool>,
    only_alive: Option<bool>,
    category: Option<String>,
    mode: Option<String>,
    hide_garbled: Option<bool>,
    show_weak: Option<bool>,
) -> Result<Value, String> {
    let blocks = block_keywords(&state);
    let filter = make_filter(
        hide_adult.unwrap_or(false),
        only_alive.unwrap_or(false),
        category,
        blocks,
        hide_garbled.unwrap_or(true),
    );
    let q = query.trim();
    let limit = limit.unwrap_or(60);
    let offset = offset.unwrap_or(0);
    let sort_key = SortKey::parse(sort.as_deref().unwrap_or(""));
    let desc = desc.unwrap_or(true);

    // Ortak arama yolu (API ile aynı): boş sorgu → gözat; semantik yuvası doluysa hibrit.
    let outcome = dragnet_api::search::search(
        &state.store,
        &state.semantic.slot,
        q,
        SearchMode::parse(mode.as_deref().unwrap_or("")),
        limit,
        offset,
        sort_key,
        desc,
        &filter,
        show_weak.unwrap_or(false),
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(
        json!({ "results": summaries_json(&outcome.rows), "count": outcome.rows.len(), "mode": outcome.used.as_str(), "weak": outcome.weak, "corrected": outcome.corrected }),
    )
}

/// Dashboard: genel görünüm + keşif zaman serisi. `bucket` "hour"|"day",
/// `points` kova sayısı (grafik gün/saat seçimi).
#[tauri::command]
pub async fn dashboard(
    state: State<'_, AppState>,
    bucket: Option<String>,
    points: Option<i64>,
) -> Result<Value, String> {
    let is_day = bucket.as_deref() == Some("day");
    let bucket_secs = if is_day { 86_400 } else { 3_600 };
    let pts = points.unwrap_or(if is_day { 30 } else { 48 }).clamp(1, 365);
    let series = state
        .store
        .discovery(bucket_secs, pts)
        .await
        .map_err(|e| e.to_string())?;
    let overview = state.store.overview().await.map_err(|e| e.to_string())?;
    Ok(json!({
        "series": series.iter().map(|(t, n)| json!({ "t": t, "count": n })).collect::<Vec<_>>(),
        "bucket": if is_day { "day" } else { "hour" },
        "overview": overview_json(&overview),
    }))
}

/// Ağ sağlığı: birkaç hedefe TCP bağlantı gecikmesi (ICMP admin gerektirdiğinden TCP).
#[tauri::command]
pub async fn network_health(state: State<'_, AppState>) -> Result<Value, String> {
    // ASIL KANIT: çalışan harvester'ın kendi sayaçları. Sentetik yoklama ISS engeli,
    // sessiz bootstrap düğümü ya da kısa zaman aşımı yüzünden yanlış negatif verebilir
    // (kullanıcı geri bildirimi: kart "UDP çalışmıyor" derken hasat 57 örnek/sn ile
    // sürüyordu). Bu yüzden canlı sayaçlar da raporlanır ve karar onlara göre verilir.
    let live = {
        let guard = state.engine.lock().await;
        match guard.as_ref() {
            Some(e) => {
                let s = e.snapshot().await;
                Some(json!({
                    "queries_sent": s.harvester.queries_sent,
                    "responses": s.harvester.responses_seen,
                    "queries_seen": s.harvester.queries_seen,
                    "samples": s.harvester.samples_seen,
                    "get_peers_seen": s.harvester.get_peers_seen,
                    "announce_seen": s.harvester.announce_seen,
                }))
            }
            None => None,
        }
    };
    // Hedefler Dragnet'in gerçekten ihtiyaç duyduğu şeyleri ölçer: DNS sunucularına TCP
    // gecikmesi (genel internet sağlığı) ve **DHT bootstrap düğümüne UDP** (asıl kritik
    // yol — Dragnet TCP değil UDP ile çalışır; bazı ağlarda 443 açıkken UDP kapalıdır).
    // Not: tek hedefin başarısızlığı ağın bozuk olduğu anlamına gelmez; ISS'ler belirli
    // adresleri (ör. 1.1.1.1) engelleyebilir — bu yüzden hata sebebi ayrı raporlanır.
    // 443 kullanılır: birçok ISS üçüncü taraf DNS'e (TCP/UDP 53) giden trafiği engeller;
    // ilk sürümde hedefler 53. porttaydı ve hepsi "erişilemedi" görünüyordu — oysa ağ
    // çalışıyordu (kullanıcı geri bildirimi + hasat sayaçları bunu kanıtladı).
    const TCP_TARGETS: [(&str, &str); 3] = [
        ("Google", "google.com:443"),
        ("Cloudflare", "cloudflare.com:443"),
        ("Wikipedia", "wikipedia.org:443"),
    ];
    let mut probes = Vec::new();
    for (name, addr) in TCP_TARGETS {
        let r = tauri::async_runtime::spawn_blocking(move || probe(addr))
            .await
            .unwrap_or(Probe::fail("görev çöktü"));
        probes.push(json!({
            "name": name, "target": addr, "ms": r.ms, "ok": r.ok,
            "jitter": r.jitter, "loss": r.loss, "error": r.error,
        }));
    }
    // DHT (UDP) yoklaması: bootstrap düğümüne `find_node` gönderip yanıt bekle.
    let dht = tauri::async_runtime::spawn_blocking(dht_udp_probe)
        .await
        .unwrap_or(Probe::fail("görev çöktü"));
    Ok(json!({
        "probes": probes,
        "dht": { "name": "DHT bootstrap (UDP)", "target": "dht.transmissionbt.com:6881",
                 "ms": dht.ms, "ok": dht.ok, "error": dht.error },
        // Canlı hasat sayaçları: UDP'nin gerçekten çalışıp çalışmadığının kanıtı.
        "live": live,
    }))
}

/// Bir yoklamanın sonucu: en iyi gecikme, kararsızlık (jitter), kayıp oranı ve hata.
struct Probe {
    ms: u64,
    ok: bool,
    jitter: u64,
    loss: u8,
    error: &'static str,
}

impl Probe {
    fn fail(error: &'static str) -> Self {
        Self {
            ms: 0,
            ok: false,
            jitter: 0,
            loss: 100,
            error,
        }
    }
}

/// TCP bağlanma gecikmesi: 3 deneme; en iyi süre, kararsızlık ve kayıp oranı.
/// DNS çözümü ile bağlantı hatası ayrı raporlanır — "erişilemedi" tek başına
/// hangisinin bozuk olduğunu söylemiyordu (kullanıcı geri bildirimi).
fn probe(addr: &str) -> Probe {
    use std::net::ToSocketAddrs;
    let Some(sa) = addr.to_socket_addrs().ok().and_then(|mut a| a.next()) else {
        return Probe::fail("ad çözülemedi (DNS)");
    };
    let mut times = Vec::new();
    let mut fails = 0;
    for _ in 0..3 {
        let start = Instant::now();
        match TcpStream::connect_timeout(&sa, Duration::from_millis(1500)) {
            Ok(_) => times.push(start.elapsed().as_millis() as u64),
            Err(_) => fails += 1,
        }
    }
    if times.is_empty() {
        return Probe::fail("bağlantı kurulamadı (engelli ya da zaman aşımı)");
    }
    let best = *times.iter().min().unwrap_or(&0);
    let worst = *times.iter().max().unwrap_or(&0);
    Probe {
        ms: best,
        ok: true,
        jitter: worst.saturating_sub(best),
        loss: (fails * 100 / 3) as u8,
        error: "",
    }
}

/// DHT bootstrap düğümüne UDP `find_node` gönderip yanıt süresini ölçer. Dragnet'in
/// asıl taşıyıcısı UDP olduğu için bu, TCP yoklamalarından daha anlamlıdır.
fn dht_udp_probe() -> Probe {
    use std::net::{ToSocketAddrs, UdpSocket};
    // Tek bir bootstrap düğümü yanıltıcıdır (router.bittorrent.com sık sık sessiz kalır);
    // üçü sırayla denenir ve ilk yanıt veren raporlanır.
    const NODES: [&str; 3] = [
        "dht.transmissionbt.com:6881",
        "router.utorrent.com:6881",
        "router.bittorrent.com:6881",
    ];
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else {
        return Probe::fail("UDP soketi açılamadı");
    };
    let _ = sock.set_read_timeout(Some(Duration::from_millis(4000)));
    let id = [0x42u8; 20];
    let pkt = dragnet_dht::krpc::build_find_node(b"dg", &id, &id);
    let mut resolved = false;
    for host in NODES {
        let Some(sa) = host.to_socket_addrs().ok().and_then(|mut a| a.next()) else {
            continue;
        };
        resolved = true;
        let start = Instant::now();
        if sock.send_to(&pkt, sa).is_err() {
            continue;
        }
        let mut buf = [0u8; 1500];
        if sock.recv_from(&mut buf).is_ok() {
            return Probe {
                ms: start.elapsed().as_millis() as u64,
                ok: true,
                jitter: 0,
                loss: 0,
                error: "",
            };
        }
    }
    if resolved {
        Probe::fail("bootstrap düğümleri yanıt vermedi")
    } else {
        Probe::fail("ad çözülemedi (DNS)")
    }
}

/// İndirme hızı testi (kullanıcı isteğiyle çalışır): sırayla birkaç genel test
/// sunucusundan ~8 MB indirir, ilk yanıt vereni kullanır. Sonuç Mbit/sn.
#[tauri::command]
pub async fn speed_test() -> Result<Value, String> {
    const URLS: [(&str, &str); 3] = [
        (
            "Cloudflare",
            "https://speed.cloudflare.com/__down?bytes=8000000",
        ),
        ("Hetzner", "https://speed.hetzner.de/10MB.bin"),
        ("OVH", "https://proof.ovh.net/files/10Mb.dat"),
    ];
    let res = tauri::async_runtime::spawn_blocking(|| {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(25))
            .build()
            .map_err(|e| e.to_string())?;
        let mut last_err = String::from("bilinmiyor");
        for (name, url) in URLS {
            let start = Instant::now();
            match client.get(url).send().and_then(|r| r.error_for_status()) {
                Ok(resp) => match resp.bytes() {
                    Ok(body) => {
                        let secs = start.elapsed().as_secs_f64().max(0.001);
                        let mbps = (body.len() as f64 * 8.0) / secs / 1_000_000.0;
                        return Ok(json!({
                            "ok": true, "server": name, "bytes": body.len(),
                            "seconds": (secs * 10.0).round() / 10.0,
                            "mbps": (mbps * 10.0).round() / 10.0,
                        }));
                    }
                    Err(e) => last_err = e.to_string(),
                },
                Err(e) => last_err = e.to_string(),
            }
        }
        Err(last_err)
    })
    .await
    .map_err(|e| e.to_string())?;
    match res {
        Ok(v) => Ok(v),
        Err(e) => Ok(json!({ "ok": false, "error": e })),
    }
}

/// Taramayı başlat (çekirdeği ayarlarla ayağa kaldır).
#[tauri::command]
pub async fn start_scan(state: State<'_, AppState>) -> Result<Value, String> {
    let mut guard = state.engine.lock().await;
    if guard.is_none() {
        let cfg = {
            let s = state.settings.lock().unwrap_or_else(|p| p.into_inner());
            s.to_engine_config(state.db_path.clone())
        };
        let engine = Engine::start(cfg).await.map_err(|e| e.to_string())?;
        *guard = Some(engine);
    }
    Ok(json!({ "scanning": true }))
}

/// Taramayı durdur (çekirdeği bırak).
#[tauri::command]
pub async fn stop_scan(state: State<'_, AppState>) -> Result<Value, String> {
    let mut guard = state.engine.lock().await;
    *guard = None; // drop → tüm görevler durur
    Ok(json!({ "scanning": false }))
}

/// Mevcut ayarlar.
#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> Value {
    let s = state
        .settings
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    serde_json::to_value(s).unwrap_or(Value::Null)
}

/// Ayarları güncelle, kaydet, autostart uygula ve (tarama açıksa) çekirdeği yeniden başlat.
#[tauri::command]
pub async fn set_settings(state: State<'_, AppState>, settings: Settings) -> Result<Value, String> {
    {
        let mut s = state.settings.lock().unwrap_or_else(|p| p.into_inner());
        *s = settings.clone();
        let _ = s.save();
    }
    let _ = autostart::set(settings.autostart);

    // Depolama sınırlarını ANINDA uygula ve basıncı hemen ölç (F8-4). Eskiden yalnız
    // açılışta uygulanıyordu: kullanıcı bütçeyi düşürüp kaydedince hiçbir şey olmuyordu.
    let (db_max, reserve) = settings.storage_limits();
    state.store.set_limits(db_max, reserve);
    let p = state.store.refresh_pressure();
    tracing::info!(
        db_max,
        reserve,
        db_bytes = p.db_bytes,
        paused = p.paused,
        "depolama sınırları uygulandı"
    );

    // Semantik ayarları anında uygula (aç/kapa/kademe değişimi; yeniden başlatma yok).
    state.semantic.apply(state.store.clone(), &settings).await;

    // Tarama açıksa yeni ayarlarla yeniden başlat. ÖNCE ESKİSİ KAPATILIR:
    // harvester'ın UDP portu (6881) tek bir sürece bağlanabilir; eskisi ayakta
    // dururken yenisini başlatınca port dolu bulunuyor ve **efemer porta düşülüyordu**
    // (kullanıcı ekranında port 53237 görünüyordu) — bu da modemdeki yönlendirmeyi
    // işlevsiz bırakıp pasif hasadı sıfırlıyordu. Bedeli: yeni çekirdek başlatılamazsa
    // tarama kapalı kalır (hata döndürülür, kullanıcı yeniden başlatabilir).
    let mut guard = state.engine.lock().await;
    if guard.is_some() {
        drop(guard.take()); // eski çekirdek burada durur, portlar serbest kalır
                            // Soketlerin işletim sistemi tarafından bırakılması bir an sürebilir.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let cfg = settings.to_engine_config(state.db_path.clone());
        match Engine::start(cfg).await {
            Ok(e) => {
                tracing::info!(addr = %e.harvester_addr(), "tarama yeni ayarlarla yeniden başlatıldı");
                *guard = Some(e);
            }
            Err(e) => return Err(format!("tarama yeniden başlatılamadı: {e}")),
        }
    }
    Ok(json!({ "ok": true }))
}

/// Yalnızca başlangıçta-başlat ayarını değiştir.
#[tauri::command]
pub fn set_autostart(state: State<'_, AppState>, enabled: bool) -> Result<Value, String> {
    autostart::set(enabled).map_err(|e| e.to_string())?;
    let mut s = state.settings.lock().unwrap_or_else(|p| p.into_inner());
    s.autostart = enabled;
    let _ = s.save();
    Ok(json!({ "autostart": enabled }))
}

/// Güncelleme var mı?
#[tauri::command]
pub async fn check_update() -> Result<Value, String> {
    let r = tauri::async_runtime::spawn_blocking(updater::check)
        .await
        .map_err(|e| e.to_string())?;
    match r {
        Ok(Some(i)) => Ok(json!({ "available": true, "version": i.version, "notes": i.notes })),
        Ok(None) => Ok(json!({ "available": false })),
        Err(e) => Ok(json!({ "available": false, "error": e })),
    }
}

/// Güncellemeyi indir, imzayı doğrula, exe'yi değiştir ve yeniden başlat.
#[tauri::command]
pub async fn install_update(app: AppHandle) -> Result<Value, String> {
    let info = tauri::async_runtime::spawn_blocking(updater::check)
        .await
        .map_err(|e| e.to_string())??
        .ok_or_else(|| "güncelleme bulunamadı".to_string())?;
    tauri::async_runtime::spawn_blocking(move || updater::install(&info))
        .await
        .map_err(|e| e.to_string())??;

    // Yeni exe'yi başlat ve bu süreçten çık.
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
    app.exit(0);
    Ok(json!({ "restarted": true }))
}
