<!-- SPDX-License-Identifier: AGPL-3.0-only -->
# Dragnet

**Otonom BitTorrent DHT keşif ve indeksleme motoru.** Rust ile yazılmıştır.

Dragnet, hiçbir web sitesine veya tracker'a bağımlı olmadan BitTorrent DHT ağını
tarayarak infohash'leri ve torrent metadata'sını (isim, dosyalar, boyut) hasat eder,
bunları aranabilir bir indekse dönüştürür ve bir HTTP arama API'si üzerinden sunar.

qBittorrent'in kırılgan, site-scraper tabanlı Python arama motorunun (`nova3`)
yerine geçmek üzere tasarlanmıştır. Dragnet verisini **ağın kendisinden** üretir:
kaynak siteler çökse bile çalışır, veritabanı silinse bile ağı yeniden tarayarak
indeksi sıfırdan inşa edebilir.

## Durum

🚧 Erken geliştirme. Bileşenler `docs/ROADMAP.md` içindeki fazlara göre inşa ediliyor.
Şu an çalışan: `dragnet-core` (paylaşılan tipler).

## Mimari (özet)

```
BitTorrent DHT ağı
      │  (BEP-5: get_peers / announce_peer trafiği)
      ▼
[dragnet-dht]  ── yeni infohash'ler ──►  [dragnet-meta]  ── metadata (BEP-9) ──►  [dragnet-store]
   DHT harvester                          metadata fetcher                         SQLite + FTS5
                                                                                        │
                                                                                        ▼
                                                                                  [dragnet-api]
                                                                                  HTTP arama API
                                                                                        │
                                                                                        ▼
                                                                         qBittorrent nova3 plugin
                                                                         (plugins/qbittorrent/dragnet.py)
```

Ayrıntı için `docs/ARCHITECTURE.md`.

## Hızlı başlangıç

```bash
cargo build
cargo test
```

## Lisans

Çift lisanslıdır:

- **AGPL-3.0-only** — açık kaynak kullanım için (bkz. `LICENSE`).
- **Ticari lisans** — AGPL şartlarına uymak istemeyen ticari kullanım için (bkz. `COMMERCIAL-LICENSE.md`).

Ayrıntı: `docs/LICENSING.md`.

## Başvuru / emsal

- Başvuru reposu (salt-okunur): qBittorrent — entegrasyon sınırı için incelenir.
- Emsal: [magnetico](https://github.com/boramalper/magnetico) (Go, AGPLv3).
