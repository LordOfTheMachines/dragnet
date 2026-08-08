<!-- SPDX-License-Identifier: AGPL-3.0-only -->
# Dragnet — Mimari Tasarım

## 1. Tasarım ilkeleri

1. **Site bağımsızlığı.** Veri kaynağı BitTorrent DHT ağıdır, hiçbir web sitesi değil.
2. **Yeniden inşa edilebilirlik.** İndeks, ağdan sıfırdan yeniden üretilebilir bir önbellektir;
   asla "kaybedilemez" tek gerçeklik değildir. Ağ = gerçeklik kaynağı.
3. **Bileşen ayrımı.** Harvest / fetch / store / serve birbirinden bağımsız crate'lerdir;
   her biri ayrı ölçeklenebilir ve ayrı test edilir.
4. **Backpressure.** DHT'den gelen infohash akışı, metadata fetch hızını aşabilir; aradaki
   kuyruk sınırlı olmalı ve dolduğunda zarifçe düşürmeli (drop), çökmemeli.
5. **Ayrı servis.** qBittorrent'e gömülmez; onunla yalnızca bir Python plugin + HTTP API
   sözleşmesi üzerinden konuşur.

## 2. Bileşenler

### 2.1 `dragnet-dht` — DHT Harvester (Faz 1)
- BitTorrent Mainline DHT'ye (BEP-5, Kademlia) bir düğüm olarak katılır.
- İki mod:
  - **Pasif:** ağda uçuşan `get_peers` / `announce_peer` sorgularını dinleyerek infohash toplar.
  - **Aktif:** `find_node` ile kimlik uzayında gezinip (node ID'yi periyodik değiştirerek)
    daha fazla trafik görünürlüğü kazanır ("horizontal crawling").
- Çıktı: benzersiz infohash akışı (sınırlı kanal / bounded channel üzerinden).
- Aday crate'ler: `mainline`, `rustydht-lib`. Değerlendirilecek; birini seçip sarmalayacağız.
- Node ID rotasyonu, rate-limit ve kötü niyetli düğüm filtreleme burada ele alınır.

### 2.2 `dragnet-meta` — Metadata Fetcher (Faz 2)
- Girdi: infohash. Görev: torrent metadata'sını **tracker'sız** almak.
- Adımlar: DHT'den peer bul (`get_peers`) → peer'e bağlan → BEP-10 extension handshake →
  BEP-9 `ut_metadata` ile metadata parçalarını çek → infohash ile doğrula → bencode çöz.
- Çıktı: `TorrentRecord` (isim, toplam boyut, dosya listesi).
- Aday yapı taşı: `librqbit` / `librqbit-core` (olgun Rust torrent client; peer wire + metadata
  değişimini zaten içerir). Sıfırdan yazmak yerine bunu değerlendireceğiz.
- Zaman aşımı, yeniden deneme ve "ulaşılamayan infohash" işaretlemesi burada.

### 2.3 `dragnet-store` — Kalıcılık + Arama İndeksi (Faz 3)
- Başlangıç: **SQLite** (`sqlx` async) + **FTS5** tam metin arama. Tek dosya, sıfır kurulum.
- Ölçekleme yolu: aynı `sqlx` soyutlamasıyla PostgreSQL'e geçiş.
- Şema (bkz. §4). Yazma yolu idempotent: aynı infohash tekrar görülürse `last_seen` /
  `seen_count` güncellenir (popülerlik vekili), yeni satır açılmaz.

### 2.4 `dragnet-api` — HTTP Arama API (Faz 4)
- `axum` tabanlı REST. Uç noktalar:
  - `GET /search?q=<sorgu>&cat=<kategori>&limit=<n>` → JSON sonuç listesi.
  - `GET /healthz` → sağlık kontrolü.
  - `GET /stats` → indeks büyüklüğü, crawl hızı.
- Bu, qBittorrent plugin'inin konuştuğu tek yüzeydir. Sözleşme `docs/INTEGRATION.md`.

### 2.5 `dragnetd` — Daemon (Faz 5)
- Tüm crate'leri tek süreçte birleştirir: harvester → kuyruk → fetcher havuzu → store → api.
- Yapılandırma (`figment`/`config`): dinleme portu, DB yolu, eşzamanlılık, kuyruk boyutu.
- Gözlemlenebilirlik: `tracing` ile yapılandırılmış log; `/stats` üzerinden metrik.

### 2.6 `plugins/qbittorrent/dragnet.py` — Entegrasyon (Faz 6)
- nova3 `Engine` arayüzünü uygulayan ince Python dosyası. `dragnet-api`'ye HTTP sorgusu atar,
  gelen JSON'u `novaprinter.prettyPrinter` sözleşmesine (`link|name|size|seeds|leech|...`) çevirir.
- qBittorrent kaynağına dokunmaz; kullanıcı bu dosyayı kendi arama-plugin dizinine kopyalar.

## 3. Veri akışı (uçtan uca)

1. `dragnet-dht` ağı tarar, ham infohash üretir.
2. Sınırlı kanal + dedup (yakın zamanda görülenler için bloom/LRU) → gürültüyü keser.
3. `dragnet-meta` havuzu her yeni infohash için metadata çeker (paralel, zaman aşımlı).
4. Başarılı kayıtlar `dragnet-store`'a yazılır; FTS indeksine girer.
5. `dragnet-api` sorguları FTS üzerinden yanıtlar.
6. qBittorrent plugin'i API'yi sorgular, kullanıcıya sonuç gösterir.

## 4. Veri modeli (ilk taslak)

`torrents` tablosu:

| alan | tip | açıklama |
|---|---|---|
| `infohash` | TEXT (PK) | 40 hex karakter (v1) — birincil anahtar |
| `name` | TEXT | torrent adı (metadata'dan) |
| `total_size` | INTEGER | bayt cinsinden toplam boyut |
| `file_count` | INTEGER | dosya sayısı |
| `first_seen` | INTEGER | ilk görülme (unix ts) |
| `last_seen` | INTEGER | son görülme (unix ts) |
| `seen_count` | INTEGER | DHT'de kaç kez görüldü (popülerlik vekili) |
| `metadata_status` | TEXT | `pending` / `fetched` / `unreachable` |

`files` tablosu (1-N): `infohash`, `path`, `size`.
`torrents_fts` (FTS5): `name` üzerinde tam metin arama; `infohash` ile eşlenir.

> Not: "seeds/leech" değerleri DHT crawl'ından doğrudan gelmez; `seen_count` bir vekildir.
> Gerçek seed sayısı istenirse sorgu anında DHT `get_peers` scrape'i ile tahmin edilebilir (Faz 4+).

## 5. Teknoloji seçimleri

| İhtiyaç | Seçim | Gerekçe |
|---|---|---|
| Dil | Rust | yüksek eşzamanlı ağ I/O, bellek güvenliği, tek binary |
| Runtime | `tokio` | async ağ için fiilî standart |
| DHT | `mainline` / `rustydht-lib` (değerlendirilecek) | hazır Kademlia/BEP-5 |
| Peer wire + BEP-9 | `librqbit` bileşenleri (değerlendirilecek) | olgun, yeniden yazmayı önler |
| Depolama | `sqlx` + SQLite/FTS5 → sonra PostgreSQL | sıfır kurulumla başla, sonra ölçekle |
| HTTP API | `axum` | tokio ekosistemi, ergonomik |
| Serileştirme | `serde`, bencode: `bendy`/`serde_bencode` | standart |
| Log/metrik | `tracing` | yapılandırılmış gözlemlenebilirlik |
| Config | `figment` | katmanlı yapılandırma |

## 6. Legal & Safety

- DHT indeksi doğası gereği yasa dışı içerik de barındırabilir (bkz. magnetico'nun aynı uyarısı).
- API/UI varsayılan olarak yerel (`127.0.0.1`) dinlemeli; herkese açık dağıtım kullanıcının
  bilinçli kararı olmalı ve kimlik doğrulama gerektirmeli.
- İçerik filtreleme (engellenecek infohash/kelime listeleri) opsiyonel bir katman olarak
  planlanır (Faz 4+). Sorumlu kullanım kullanıcının yükümlülüğüdür.

## 7. Açık kararlar (ileride netleşecek)

- DHT crate: `mainline` mi `rustydht-lib` mi? → Faz 1 spike ile ölçülecek.
- Metadata: `librqbit` sarmalamak mı, minimal kendi wire implementasyonu mu? → Faz 2 spike.
- BitTorrent v2 (SHA-256 infohash) desteği ne zaman? → v1 çalıştıktan sonra.
