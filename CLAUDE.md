# CLAUDE.md — Dragnet

Bu dosya, bu depoda çalışan Claude Code (ve diğer geliştiriciler) için tek referans rehberdir.
Yeni bir oturuma başlarken önce bunu, sonra `docs/ARCHITECTURE.md` ve `docs/ROADMAP.md` dosyalarını oku.

## Proje tek cümlede

**Dragnet**, BitTorrent DHT ağını doğrudan tarayarak (crawl) infohash ve torrent
metadata'sını hasat eden, kendi arama indeksini üreten, hiçbir web sitesine bağımlı
olmayan, Rust ile yazılmış otonom bir torrent keşif/indeksleme servisidir.
Bir HTTP arama API'si sunar; qBittorrent'e ince bir nova3 plugin'i üzerinden bağlanır.

## Neden var — çözdüğü problem

qBittorrent'in mevcut arama motoru (`nova3`, Python) bir **meta-arama scraper'ıdır**:
başka torrent sitelerinin arama sayfalarını çeker ve HTML ayrıştırır. O siteler kapanınca
arama kör olur. Dragnet, veriyi **ağın kendisinden** (DHT) üretir; bir site çökse, hatta
tüm siteler çökse bile çalışmaya devam eder. Veritabanı silinse dahi crawler'ı yeniden
çalıştırıp indeksi sıfırdan inşa edebilir — **ağ, tek gerçeklik kaynağıdır.**

## Kesin kurallar (bunları asla ihlal etme)

1. **qBittorrent deposu SALT-OKUNURDUR.** `c:\Users\gilik\.PyProjects\qBittorrent`
   sadece bir **başvuru reposudur**. Oraya ASLA yazma, düzenleme, commit yapma.
   Entegrasyon sınırını incelemek için okunur; değiştirilmez.
   (Bkz. `docs/INTEGRATION.md` — nova3/SearchPluginManager sınırı oradan çıkarıldı.)
2. **Dil: Rust.** Async runtime `tokio`. Yeni bileşenler Cargo workspace crate'i olarak eklenir.
3. **Lisans: AGPLv3 + Ticari (çift lisans).** Her yeni kaynak dosyanın başına SPDX satırı:
   `// SPDX-License-Identifier: AGPL-3.0-only`. Bkz. `docs/LICENSING.md`.
4. **Mimari: ayrı servis.** Dragnet qBittorrent'ten bağımsız derlenir/çalışır. qBittorrent'e
   tek dokunuş `plugins/qbittorrent/dragnet.py` (kullanıcının kendi nova3 dizinine kopyalayacağı
   dosya) üzerindendir; qBittorrent kaynak koduna değişiklik gerektirmez.
5. **Hukuki not:** Bu bir keşif/indeksleme altyapısıdır. İçerik filtreleme ve sorumlu kullanım
   `docs/ARCHITECTURE.md` içindeki "Legal & Safety" başlığında ele alınır; yok sayma.

## Depo yapısı

```
dragnet/
  Cargo.toml              # Rust workspace kökü
  crates/
    dragnet-core/         # paylaşılan tipler (Infohash, TorrentRecord) — ÇALIŞIR durumda
    dragnet-dht/          # (Faz 1) DHT harvester
    dragnet-meta/         # (Faz 2) BEP-9 metadata fetcher
    dragnet-store/        # (Faz 3) kalıcılık + tam metin arama (SQLite FTS5)
    dragnet-api/          # (Faz 4) HTTP arama API (axum)
  bin/
    dragnetd/             # (Faz 5) her şeyi birleştiren daemon binary
  plugins/
    qbittorrent/
      dragnet.py          # nova3 engine plugin (qBittorrent'e kopyalanır)
  docs/
    ARCHITECTURE.md       # bileşen şeması, veri modeli, teknoloji seçimleri
    ROADMAP.md            # fazlı geliştirme planı
    INTEGRATION.md        # qBittorrent nova3 sınırı ve plugin sözleşmesi
    LICENSING.md          # AGPLv3 + ticari model
  LICENSE                 # AGPL-3.0-only
  COMMERCIAL-LICENSE.md   # ticari lisans şablonu/politikası
```

## Geliştirme komutları

```bash
cargo build            # tüm workspace'i derle
cargo test             # testleri çalıştır
cargo run -p dragnetd  # (Faz 5'ten sonra) daemon'ı çalıştır
cargo clippy           # linter
cargo fmt              # biçimlendirme
```

## Referans

- Başvuru reposu (salt-okunur): `c:\Users\gilik\.PyProjects\qBittorrent`
- Emsal proje: magnetico (Go, AGPLv3) — https://github.com/boramalper/magnetico
- Ele alacağımız protokoller: BEP-5 (DHT), BEP-9 (metadata exchange), BEP-10 (extension protocol), BEP-3 (peer wire)
