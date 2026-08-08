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

## 4. Kurulum (kullanıcı adımları — Faz 6'da netleşecek)

1. `dragnetd` servisini çalıştır (indeks üretir + API sunar).
2. `plugins/qbittorrent/dragnet.py` dosyasını qBittorrent'in arama-plugin dizinine kopyala
   (qBittorrent > Arama > Arama motorları > Plugin ekle, ya da doğrudan `engines/` klasörüne).
3. qBittorrent arama sekmesinde "Dragnet" motoru görünür ve sorgulanabilir.

> qBittorrent'in kendi kaynağı hiçbir aşamada değiştirilmez. Bağ tamamen plugin + HTTP API'dir.

## 5. Neden bu sınır doğru

- qBittorrent güncellemelerinden bağımsız kalırız (`git pull` bizi bozmaz).
- Dragnet'i başka istemcilerle de (Transmission, Deluge veya doğrudan tarayıcı) aynı API
  üzerinden paylaşabiliriz.
- Test ve dağıtım qBittorrent derlemesi gerektirmez.
