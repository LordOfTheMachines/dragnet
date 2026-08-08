<!-- SPDX-License-Identifier: AGPL-3.0-only -->
# Dragnet — Yol Haritası

Fazlar sırayla ilerler; her faz kendi başına test edilebilir bir çıktı üretir.
"DoD" = Definition of Done (bitmiş sayılma ölçütü).

## Faz 0 — İskele (TAMAMLANDI)
- [x] Proje yapısı, `CLAUDE.md`, mimari/roadmap/entegrasyon/lisans dokümanları
- [x] Cargo workspace + `dragnet-core` (paylaşılan tipler, çalışır durumda)
- [x] AGPLv3 + ticari lisans dosyaları
- [x] nova3 plugin taslağı (`plugins/qbittorrent/dragnet.py`)

## Faz 1 — DHT Harvester (`dragnet-dht`)
- [ ] DHT crate spike: `mainline` vs `rustydht-lib` — birini seç
- [ ] Mainline DHT'ye katıl, bootstrap düğümlerinden ağa gir
- [ ] `get_peers`/`announce_peer` trafiğinden infohash hasat et (pasif mod)
- [ ] Node ID rotasyonu + rate limit
- [ ] Çıktı: sınırlı kanaldan (bounded channel) benzersiz infohash akışı
- **DoD:** `cargo run` ile birkaç dakikada terminale gerçek infohash'ler akıyor.

## Faz 2 — Metadata Fetcher (`dragnet-meta`)
- [ ] Metadata spike: `librqbit` sarmalamak mı, minimal wire mı — karar
- [ ] Infohash → peer bul → BEP-10 handshake → BEP-9 `ut_metadata` çek
- [ ] Metadata'yı infohash ile doğrula, bencode çöz → `TorrentRecord`
- [ ] Zaman aşımı, yeniden deneme, `unreachable` işaretleme
- **DoD:** Bilinen bir infohash verilince doğru isim + dosya listesi dönüyor.

## Faz 3 — Depolama + İndeks (`dragnet-store`)
- [ ] `sqlx` + SQLite şeması (`torrents`, `files`, `torrents_fts`)
- [ ] Idempotent upsert (tekrar görülende `last_seen`/`seen_count` güncelle)
- [ ] FTS5 tam metin arama sorgusu
- [ ] Migration altyapısı
- **DoD:** Harvest+fetch sonuçları kalıcı yazılıyor; `name` üzerinden arama çalışıyor.

## Faz 4 — Arama API (`dragnet-api`)
- [ ] `axum` sunucu: `/search`, `/healthz`, `/stats`
- [ ] JSON sonuç şeması (bkz. INTEGRATION.md)
- [ ] Varsayılan `127.0.0.1` bind + opsiyonel token auth
- [ ] (Ops.) sorgu anında DHT `get_peers` ile seed tahmini
- **DoD:** `curl "localhost:PORT/search?q=..."` gerçek sonuç döndürüyor.

## Faz 5 — Daemon (`dragnetd`)
- [ ] Tüm crate'leri tek süreçte birleştir (harvester→kuyruk→fetcher havuzu→store→api)
- [ ] `figment` yapılandırma + `tracing` log
- [ ] Zarif kapanış, backpressure ayarları
- **DoD:** Tek komutla (`cargo run -p dragnetd`) uçtan uca çalışan servis.

## Faz 6 — qBittorrent Entegrasyonu
- [ ] `dragnet.py` plugin'ini gerçek API'ye bağla
- [ ] qBittorrent'e kurulum talimatı (kullanıcı plugin'i nova3 dizinine kopyalar)
- [ ] Uçtan uca test: qBittorrent arama kutusundan Dragnet sonuçları
- **DoD:** qBittorrent arama sekmesinde Dragnet motoru sonuç veriyor.

## Faz 7+ — Olgunlaştırma (opsiyonel)
- [ ] Web arayüzü (magneticow benzeri gözat/ara)
- [ ] İçerik filtreleme katmanı (engel listeleri)
- [ ] BitTorrent v2 (SHA-256 infohash)
- [ ] PostgreSQL'e ölçekleme, dağıtık crawler
- [ ] Ticari sürüm paketleme (bkz. LICENSING.md)

## Çalışma yöntemi
- Her faz kendi dalında (branch) geliştirilir, testle kapanır.
- Belirsizlik olan yerde önce küçük bir **spike** (deneysel prototip) yapılır, sonra karar yazılır.
- Kararlar `docs/ARCHITECTURE.md` §7'ye işlenir.
