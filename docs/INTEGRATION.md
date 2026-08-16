<!-- SPDX-License-Identifier: AGPL-3.0-only -->
# Dragnet — qBittorrent Entegrasyonu

Bu belge, Dragnet'in qBittorrent'e **kaynak koduna dokunmadan** nasıl bağlandığını tanımlar.
Bilgiler qBittorrent başvuru reposu (salt-okunur) incelenerek çıkarılmıştır.

## 1. qBittorrent arama mimarisi (çıkarılan gerçekler)

- C++ tarafında `SearchPluginManager` (`src/base/search/searchpluginmanager.cpp`) arama
  motorunu **harici bir süreç** olarak başlatır: `nova2.py`'yi bir `QProcess` ile çağırır,
  komut satırından `motor kategori anahtar-kelimeler` geçer.
- Python `nova2.py`, `engines/` klasöründeki her plugin'i (bir `Engine` alt sınıfı) yükler
  ve `search()` metodunu çağırır.
- Her sonuç, `novaprinter.prettyPrinter()` ile **stdout'a `|` ile ayrılmış tek satır** olarak
  basılır. C++ tarafı bu satırları ayrıştırıp arayüze döker.
- **Sözleşme bu kadar:** alt-süreç + stdin/stdout + boru hattı. Değiştireceğimiz/ekleyeceğimiz
  tek şey `engines/` altına düşen bir plugin dosyasıdır.

## 2. prettyPrinter satır sözleşmesi

`prettyPrinter` şu alanları bu sırayla, `|` ile birleştirip basar:

```
link | name | size(bytes) | seeds | leech | engine_url | desc_link(ops.) | pub_date(ops.)
```

- `link` — indirilebilir `.torrent` URL'si **veya** magnet linki.
- `size` — bayt cinsinden tam sayı (`anySizeToBytes` string'i de kabul eder).
- `seeds` / `leech` — tam sayı; bilinmiyorsa `-1`.
- `engine_url` — motorun kök URL'si.
- `pub_date` — unix ts; yoksa `-1`.

Dragnet magnet link üretebildiği için `link` alanına `magnet:?xt=urn:btih:<infohash>&dn=<name>`
koyar. Bu, qBittorrent'in `.torrent` dosyası indirmesine bile gerek bırakmaz.

## 3. Dragnet plugin sözleşmesi (Python ↔ HTTP API)

`plugins/qbittorrent/dragnet.py` bir nova3 `Engine`'idir. `search()` çağrılınca:

1. Dragnet API'sine `GET {base_url}/search?q=<query>&cat=<category>&limit=<n>` atar.
2. Beklenen JSON yanıt:

```json
{
  "results": [
    {
      "infohash": "…40 hex…",
      "name": "…",
      "size": 123456789,
      "seeds": -1,
      "leech": -1,
      "pub_date": 1700000000
    }
  ]
}
```

3. Her sonucu magnet link'e çevirip `prettyPrinter` ile basar.

`base_url` plugin dosyasının başında yapılandırılır (varsayılan `http://127.0.0.1:8080`).

**Opsiyonel parametreler (geriye uyumlu, plugin göndermek zorunda değil):** `offset`,
`sort`, `desc`, `alive`, `hide_adult` ve — Faz D — `mode=fts|semantic|hybrid`
(boş/bilinmeyen = otomatik: semantik katman açık ve hazırsa **hibrit** (FTS + anlamsal
harman), değilse saf FTS). Yanıt gövdesine `"mode"` (kullanılan mod) ve her öğeye
`"category"` alanı eklenmiştir; plugin bilinmeyen alanları yok sayar. Semantik durum
`GET /stats` → `"semantic"` (kapalıysa `null`).

## 4. Kurulum (kullanıcı adımları)

1. **Servisi çalıştır.** İndeks üretir ve API'yi sunar:
   ```bash
   cargo run -p dragnetd            # ya da derlenmiş: ./target/release/dragnetd
   ```
   Varsayılan API adresi `http://127.0.0.1:8080`'dir. Değiştirmek için `dragnetd.toml`
   (`api_bind`) veya `DRAGNET_API_BIND=...` ortam değişkeni kullanın
   (bkz. `dragnetd.example.toml`).

2. **Plugin'i qBittorrent'e ekle.** İki yol:
   - **GUI:** qBittorrent > *Arama* sekmesi > *Arama eklentileri…* > *Yeni eklenti kur* >
     *Yerel dosya* > `plugins/qbittorrent/dragnet.py` dosyasını seç.
   - **Elle:** dosyayı qBittorrent'in nova3 `engines/` dizinine kopyala
     (Windows'ta genelde `%LOCALAPPDATA%\qBittorrent\nova3\engines\`).

3. **Adresi eşle (gerekirse).** dragnetd varsayılan `127.0.0.1:8080` dışında bir adreste
   çalışıyorsa, `dragnet.py` içindeki `base_url`'i düzenle **veya** qBittorrent'i
   `DRAGNET_API_URL=http://host:port` ortam değişkeniyle başlat.

4. qBittorrent arama sekmesinde **"Dragnet"** motoru görünür ve sorgulanabilir.

> qBittorrent'in kendi kaynağı hiçbir aşamada değiştirilmez. Bağ tamamen plugin + HTTP API'dir.

### Plugin testi (qBittorrent olmadan)

Plugin'in API sözleşmesine uyumu, sahte bir HTTP sunucusuna karşı offline test edilebilir:

```bash
py plugins/qbittorrent/test_dragnet.py      # veya: python -m unittest ...
```

## 5. Neden bu sınır doğru

- qBittorrent güncellemelerinden bağımsız kalırız (`git pull` bizi bozmaz).
- Dragnet'i başka istemcilerle de (Transmission, Deluge veya doğrudan tarayıcı) aynı API
  üzerinden paylaşabiliriz.
- Test ve dağıtım qBittorrent derlemesi gerektirmez.
