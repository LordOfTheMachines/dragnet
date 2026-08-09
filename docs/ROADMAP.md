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
- [x] Windows sistem tepsisi (tray) kontrolcüsü — `bin/dragnet-tray` (Başlat/Durdur,
      canlı durum, ayarlar, başlangıçta başlat)
- [ ] Web arayüzü (magneticow benzeri gözat/ara)
- [ ] İçerik filtreleme katmanı (engel listeleri)
- [ ] BitTorrent v2 (SHA-256 infohash)
- [ ] PostgreSQL'e ölçekleme, dağıtık crawler
- [ ] Ticari sürüm paketleme (bkz. LICENSING.md)

## Çalışma yöntemi
- Her faz kendi dalında (branch) geliştirilir, testle kapanır.
- Belirsizlik olan yerde önce küçük bir **spike** (deneysel prototip) yapılır, sonra karar yazılır.
- Kararlar `docs/ARCHITECTURE.md` §7'ye işlenir.
