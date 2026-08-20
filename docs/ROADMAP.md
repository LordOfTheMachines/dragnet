<!-- SPDX-License-Identifier: AGPL-3.0-only -->
# Dragnet — Yol Haritası

Fazlar sırayla ilerler; her faz kendi başına test edilebilir bir çıktı üretir.
"DoD" = Definition of Done (bitmiş sayılma ölçütü).

## Faz 0 — İskele (TAMAMLANDI)
- [x] Proje yapısı, `CLAUDE.md`, mimari/roadmap/entegrasyon/lisans dokümanları
- [x] Cargo workspace + `dragnet-core` (paylaşılan tipler, çalışır durumda)
- [x] AGPLv3 + ticari lisans dosyaları
- [x] nova3 plugin taslağı (`plugins/qbittorrent/dragnet.py`)

## Faz 1 — DHT Harvester (`dragnet-dht`) — TAMAMLANDI
- [x] DHT crate spike: `mainline` vs `rustydht-lib` — `mainline` temel alındı, pasif
      hasat kendi KRPC katmanıyla (gerekçe: `docs/ARCHITECTURE.md` §7.1)
- [x] Mainline DHT'ye katıl, bootstrap düğümlerinden ağa gir
- [x] `get_peers`/`announce_peer` trafiğinden infohash hasat et (pasif mod)
- [x] Node ID rotasyonu + rate limit (token-bucket)
- [x] Çıktı: sınırlı kanaldan (bounded channel) benzersiz infohash akışı (LRU dedup)
- **DoD:** ✅ `cargo run -p dragnet-dht --example harvest` ile birkaç dakikada terminale
  gerçek infohash'ler akıyor (canlı ağda doğrulandı).

## Faz 2 — Metadata Fetcher (`dragnet-meta`) — TAMAMLANDI
- [x] Metadata spike: minimal kendi wire katmanı seçildi (ARCHITECTURE §7.2)
- [x] Infohash → peer bul (`get_peers`) → BEP-10 handshake → BEP-9 `ut_metadata` çek
- [x] Metadata'yı infohash ile SHA-1 doğrula, bencode çöz → `TorrentRecord`
- [x] Zaman aşımı (peer başına + genel), çok-peer eşzamanlı deneme
- **DoD:** ✅ Sintel & Big Buck Bunny infohash'lerinden doğru isim + dosya listesi + boyut
  canlı ağdan çekildi (`cargo run -p dragnet-meta --example fetch -- <infohash>`).

## Faz 3 — Depolama + İndeks (`dragnet-store`) — TAMAMLANDI
- [x] `sqlx` + SQLite şeması (`torrents`, `files`, `torrents_fts`)
- [x] Idempotent upsert (tekrar görülende `last_seen`/`seen_count` güncelle)
- [x] FTS5 tam metin arama sorgusu (önek + sanitize)
- [x] Şema kurulumu (`IF NOT EXISTS` migration) + harvester `record_sighting` yolu
- **DoD:** ✅ Harvest+fetch sonuçları kalıcı yazılıyor; `name` üzerinden arama çalışıyor
  (5 offline test: upsert/get, idempotent seen_count, FTS önek, pending→fetched geçişi).

## Faz 4 — Arama API (`dragnet-api`) — TAMAMLANDI
- [x] `axum` sunucu: `/search`, `/healthz`, `/stats`
- [x] JSON sonuç şeması (INTEGRATION.md ile hizalı)
- [x] Varsayılan `127.0.0.1` bind + opsiyonel bearer token auth
- [ ] (Ops.) sorgu anında DHT `get_peers` ile seed tahmini → ertelendi (Faz 7+)
- **DoD:** ✅ `/search?q=...` gerçek sonuç döndürüyor (5 test: search, boş sonuç,
  stats, token auth 401/200, healthz).

## Faz 5 — Daemon (`dragnetd`) — TAMAMLANDI
- [x] Tüm crate'leri tek süreçte birleştir (harvester→sighting→fetcher havuzu→store→api)
- [x] `figment` yapılandırma (varsayılan→toml→env) + `tracing` log
- [x] Zarif kapanış (Ctrl+C), Semaphore ile bounded fetcher havuzu (backpressure)
- [x] Bonus: `seed_infohashes` ile başlangıçta indeks ısıtma
- **DoD:** ✅ Tek komutla uçtan uca servis; canlı smoke test: seed Sintel çekildi →
  `/healthz`=ok, `/stats`={fetched:1}, `/search?q=sintel` doğru sonucu döndürdü.

## Faz 6 — qBittorrent Entegrasyonu — TAMAMLANDI (GUI adımı kullanıcıda)
- [x] `dragnet.py` plugin'ini gerçek API'ye bağla (JSON → prettyPrinter magnet satırı)
- [x] Kurulum talimatı (INTEGRATION.md §4: GUI + elle kurulum, adres eşleme)
- [x] Plugin↔API sözleşme testi: canlı API'ye karşı doğrulandı + offline unit test
      (`plugins/qbittorrent/test_dragnet.py`, 2 test)
- **DoD:** ✅ Plugin canlı `/search`'ten gelen sonucu doğru magnet satırına çeviriyor.
  Son adım (qBittorrent GUI'sinde motoru görmek) kullanıcının kendi qBittorrent'inde
  plugin'i kurmasıyla tamamlanır — otomasyonla test edilemez.

## Faz 7+ — Olgunlaştırma (opsiyonel)
- [x] BEP-51 `sample_infohashes` — aktif, NAT-dostu hasat (~600× artış)
- [x] `dragnet-engine` çekirdeği (daemon+app ortak) + `dragnetd` inceltildi
- [x] **Tauri masaüstü uygulaması** `bin/dragnet-app` (tek exe): tray, dashboard
      (grafik + tablolar + ağ sağlığı), arama, ayarlar, ed25519 oto-güncelleme
- [x] Nazik varsayılanlar (router conntrack koruması) + port çakışma fallback'i
- [x] `unreachable` işaretleme (ölü torrent'leri tekrar denememe)
- [x] **Faz C: Torrent canlılık kontrolü** — nazik DHT scrape ile canlı peer sayısı;
      store peer_count/last_check, API `seeds` alanı, app'te canlı/ölü göstergesi
      (Tears of Steel canlıda 16 peer doğrulandı)
- [x] İçerik kategorileri (video/audio/software/game/book/adult/archive/other heuristiği)
- [x] **İçerik filtreleme katmanı** — kullanıcı tanımlı engel kelimeleri (ayarlarda
      düzenlenebilir chip listesi); sorgu-anı, yıkıcı-olmayan filtre (store `Filter.block_keywords`)
- [x] **UI overhaul** — tam genişlik düzen; tek detaylı SIRALANABİLİR + sayfalı (sonsuz-scroll)
      gözat/ara tablosu (satır no + tıkla-sırala başlıklar, sunucu-tarafı sıralama);
      kategori hızlı sekmeleri; SVG çizgi grafik (saat/gün seçimli keşif serisi); palet cilası
- [x] Kalite çevrimleri (2 tur): kritik bencode DoS guard'ları, XSS/CSP, atomik ayar yazımı,
      mutex-poison kurtarma, API'nin çekirdekten ayrılması (tarama durunca arama kesilmez)
- [x] **Faz D: Semantik arama** — `dragnet-semantic` crate (Embedder trait; 3 kademe:
      potion-multilingual / MiniLM-L12 int8 / EmbeddingGemma-300m Q4; ort + DirectML GPU;
      int8 bellek-içi brute-force indeks; `torrent_embeddings` kalıcılığı; RRF hibrit
      arama; API `mode=fts|semantic|hybrid`; app ayarları (kademe/cihaz, anında aç/kapa,
      indirme ilerlemesi); dragnetd `semantic_*` config). Bake-off + kararlar ARCHITECTURE
      §7.3. **DoD:** ✅ 30+ offline test (mock embedder); 3 kademe gerçek modelle duman
      testi; canlı e2e: "tavşan animasyonu"→Big Buck Bunny, "çelik gözyaşları bilim
      kurgu"→Tears of Steel, "buck buny"→Big Buck Bunny (FTS boş dönerken); `mode=fts`
      eski davranış; DirectML aktif.
- [x] **Faz E: bütünsel kalite çevrimi** — pipelined metadata fetch (150 hash: 1111 s→143 s),
      öncelikli çekim kuyruğu (sıcak›popüler›taze, soğumalı yeniden deneme), sighting kaynağı +
      BEP-51 takip get_peers peer ipuçları (peer-yok %73→~%1), FetchStats + pano kartı,
      ad kodlaması (name.utf-8 / GBK/SJIS/CP1251), keşif grafiği (dolu seri, eksen, hover),
      semantik gürültü tabanı kalibrasyonu, Deneysel Qwen3 kademesi. Karar: ARCHITECTURE §7.4.
      Ek turlar: erişilebilirlik kartı, popülerlik/peer-count önceliği, bozuk ad onarımı; ad ayrıştırıcı +
      sorgu anlama + kategori-farkındalı embedding (hit@5 %47→%74); bge-reranker-v2-m3 yeniden sıralayıcı (%79).
- [x] Faz E (6): DXGI VRAM ölçümü + kapatmada serbest bırakma notu, donanıma göre otomatik kademe,
      Qwen3 kademesi elendi.
- [x] Faz F (F4-2, F4-3): yazım düzeltme (indeks sözlüğü + eş-geçiş doğrulaması), kategori
      gözatma, kavram sözlüğü, tanınmayan tek kelime → boş; hit@5 %84→%90, MRR 0.82.
- [ ] Faz F (F7 — kullanıcı önerisi): çevrim içi zenginleştirme (Wikidata öncelikli;
      kanonik ad/tür/yıl/TR takma adlar, yerel önbellek, varsayılan kapalı) — önce
      500 ad üzerinde eşleştirme başarımı prototipi.
- [x] Faz F (F4-1): güven kapısı (karşılığı olmayan sorgu → boş sonuç; cross-encoder eşiği
      −4.5, kosinüs bu ayrımı yapamıyor) + TR→EN sözlük (çeviri başlık/tür); hit@5 %79→%84,
      MRR 0.72→0.75. Kalan F4: yazım düzeltme, dönem sorgusu, geri besleme.
- [x] Faz F (F0-2): VRAM'de cihaz geneli / Dragnet ayrımı (PDH sayaçları; NVML yerine
      vendor-bağımsız), yığılmış çubuk + WebView2 payı ayrımı.
- [x] Faz F (F0): semantik durum kartı (rozet + çubuk: model/kademe, cihaz, reranker, RAM,
      indeks ilerlemesi, canlı VRAM/bütçe + donanım satırı); VRAM her yoklamada DXGI ile
      canlı okunuyor ("0 MB" hatası: ölçüm ilk çıkarımdan önce alınıyordu).
- [ ] **Faz F — model iyileştirme** (plan: `docs/PLAN-FAZ-F.md`; sıradaki **F1**): eval seti 100+, sentetik sorgu
      üretimi, Gemma/MiniLM ince ayar + ONNX, Model2Vec damıtma (light), yazım düzeltme,
      tema sözlüğü, fp16 GPU seçeneği, sorgu önbelleği.
- [ ] Semantik: özel damıtılmış model (Model2Vec ile Gemma'dan torrent-adı korpusuna
      damıtma) + kendi GitHub release'inden dağıtım; CUDA EP opsiyonu; >2M kayıtta ANN (HNSW)
- [ ] Web arayüzü (magneticow benzeri gözat/ara)
- [ ] BitTorrent v2 (SHA-256 infohash)
- [ ] PostgreSQL'e ölçekleme, dağıtık crawler
- [ ] Ticari sürüm paketleme (bkz. LICENSING.md)

## Çalışma yöntemi
- Her faz kendi dalında (branch) geliştirilir, testle kapanır.
- Belirsizlik olan yerde önce küçük bir **spike** (deneysel prototip) yapılır, sonra karar yazılır.
- Kararlar `docs/ARCHITECTURE.md` §7'ye işlenir.
