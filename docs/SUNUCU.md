<!-- SPDX-License-Identifier: AGPL-3.0-only -->
# Sunucu kurulumu — 7/24 crawler + Cloudflare önyüzü

Bu belge, Dragnet'i bir sunucuda kesintisiz çalıştırıp indeksi istemcilere dağıtmayı
anlatır. Amaç: **taramayı bir kez, düzgün yapmak**; kullanıcılar ister sonucu çeksin,
ister kendi taramasını da sürdürsün.

## 1. Neden sunucu (ve neden Cloudflare Workers değil)

DHT taraması ancak **kesintisiz** çalıştığında verimlidir. Ağın yönlendirme tablolarında
yer edinmek saatler alır ve gelen `announce_peer`/`get_peers` trafiği — bu boru hattındaki
**en kaliteli aday kaynağı** — ancak o birikimden sonra gelir. Ölçüm bunu açıkça gösterdi:
düğüm kimliği sabitlendikten sonra gelen sorgu **71/dk → 3.740/dk** çıktı ve çekim başına
bulunan peer 2,0'dan 4,6'ya yükseldi (`docs/CEKIM-HIZI.md` §12).

**Cloudflare Workers'da crawler çalışamaz.** Bu bir ayar meselesi değil, platform sınırı:

| Gereksinim | Workers |
|---|---|
| UDP soketi (BEP-5 DHT'nin tamamı UDP'dir) | **yok** |
| Uzun ömürlü süreç + kalıcı düğüm kimliği | yok (istek ömürlü) |
| Giden TCP (peer wire) | sınırlı `connect()` |

Workers'ın doğru rolü **önyüz**dür: TLS, cache, DDoS koruması, coğrafi dağıtım. Tarama
işini bir VPS yapar.

## 2. Mimari

```
[VPS]  dragnetd  ──crawl──>  DHT
          │
          ├── SQLite indeks (dragnet.db)
          └── HTTP API :8080  ──>  [Cloudflare]  ──>  istemciler
                                                        │
                                    /changes (artımlı)  ▼
                                              [dragnet-app: yerel indeks]
                                                        │
                                              semantik arama YERELDE
```

İstemcinin üç modu vardır (uygulamada Ayarlar → İndeks kaynağı):

| Mod | Yerel tarama | Sunucudan çekme | Kime uygun |
|---|---|---|---|
| `local` | ✔ | ✘ | Bugünkü davranış; tek başına çalışan kurulum |
| `remote` | ✘ | ✔ | Zayıf makine, kotalı/yavaş internet, "ağımı yormasın" |
| `hybrid` | ✔ | ✔ | Hem katkı verip hem sunucudan yararlanmak |

**Semantik arama her modda yereldedir.** Sunucudan gelen kayıtlar da yerel arka plan
indeksleyici tarafından embed edilir; model ve vektör indeksi kullanıcının makinesinde
kalır. Sunucuda GPU tutmak gerekmez.

## 3. VPS kurulumu

Gereken donanım küçüktür — crawler CPU değil **ağ** bekler:

- 2 vCPU, 2 GB RAM, 40 GB disk (Hetzner CX22 mertebesi, ~5 €/ay) fazlasıyla yeter
- **UDP 6881 açık olmalı** (hem giden hem gelen); pasif hasat buna bağlıdır
- Sunucularda genelde NAT yoktur — bu, ev bağlantısına göre büyük avantajdır

```bash
# Derle (sunucuda ya da çapraz derleyip kopyala)
cargo build --release -p dragnetd

# Yapılandırma
cp dragnetd.example.toml dragnetd.toml
```

`dragnetd.toml` içinde en az şunlar:

```toml
db_path = "/var/lib/dragnet/dragnet.db"
api_bind = "127.0.0.1:8080"     # dışarı Cloudflare/nginx üzerinden açılır
api_token = "uzun-rastgele-bir-dize"   # /changes ve /search'ü korur
harvester_port = 6881
semantic_enabled = false        # sunucuda gerekmez; arama istemcide semantikleşir
```

systemd birimi (`/etc/systemd/system/dragnet.service`):

```ini
[Unit]
Description=Dragnet DHT crawler
After=network-online.target

[Service]
Type=simple
User=dragnet
WorkingDirectory=/var/lib/dragnet
ExecStart=/usr/local/bin/dragnetd /etc/dragnet/dragnetd.toml
Restart=always
RestartSec=10
# Crawler çok sayıda soket açar; varsayılan 1024 yetmez.
LimitNOFILE=65535

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl enable --now dragnet
journalctl -u dragnet -f
```

**Yeniden başlatmalar zararsızdır:** düğüm kimliği ve bilinen düğümler
`dragnet.db.dht0` dosyasında saklanır, dolayısıyla ağdaki yerleşiklik korunur.

## 4. Cloudflare önyüzü

En basit ve sağlam yol **Cloudflare Tunnel**'dır: sunucuda port açmaya, sabit IP'ye ya da
sertifika yönetmeye gerek kalmaz.

```bash
cloudflared tunnel create dragnet
cloudflared tunnel route dns dragnet dragnet.ornek.com
```

`~/.cloudflared/config.yml`:

```yaml
tunnel: dragnet
credentials-file: /root/.cloudflared/<tunnel-id>.json
ingress:
  - hostname: dragnet.ornek.com
    service: http://127.0.0.1:8080
  - service: http_status:404
```

> **Dikkat:** Tunnel yalnız **HTTP API** içindir. DHT'nin UDP trafiği tünelden geçmez ve
> geçmemelidir — o doğrudan sunucunun kendi arayüzünden akar. Tunnel açmak UDP 6881
> ihtiyacını ortadan kaldırmaz.

Cloudflare tarafında işe yarayan ayarlar:

- **Cache Rules:** `/search` yanıtlarını kısa süre (30–60 sn) önbelleğe alın; aynı popüler
  sorgular sunucuya inmez.
- **Rate limiting:** `/changes` uç noktası tam indeks kopyalamaya izin verdiği için
  IP başına sınır koyun.
- `/changes` ve `/search` **token** ile korunuyorsa (`api_token`), istemciler token'ı
  Ayarlar → Sunucu token'ı alanına girer.

## 5. Senkronizasyonun çalışma biçimi

İstemci `GET /changes?since=<imleç>&limit=<n>` çağırır:

```json
{ "records": [ { "infohash": "…", "name": "…", "files": [ … ] } ],
  "cursor": 12345, "more": true }
```

- **İmleç** `torrents.rowid`'dir; istemci saklar ve kaldığı yerden devam eder
  (`meta` tablosunda kalıcıdır — uygulama yeniden başlayınca baştan indirmez).
- **`more`** doluysa istemci beklemeden devam eder; boşsa bir dakika bekler.
- İmleç ancak **yazma bittikten sonra** ilerletilir: süreç ortada kapanırsa parti yeniden
  çekilir (yinelenen yazma zararsızdır, `upsert` idempotenttir) ama kayıt **atlanmaz**.
- Yalnız **adı bilinen** kayıtlar taşınır. Bekleyen infohash yığınının istemciye faydası
  yoktur; onu her düğüm kendi DHT'sinden zaten görür.

Kabaca boyut: kayıt başına ~200–400 bayt JSON (dosya listesiyle). 100.000 kayıtlık bir
indeks ilk senkronizasyonda ~30 MB civarıdır, sonrası yalnız artıştır.

## 6. Ölçüm ve sağlık

Sunucuda da aynı teşhis araçları çalışır:

```bash
cargo run --release -p dragnet-store --example rate -- /var/lib/dragnet/dragnet.db 60
```

"MOTOR: n/m kalp atışı" satırı tam değilse ölçümü yorumlamayın — motor o pencerenin bir
kısmında çalışmamış demektir (bkz. `docs/CEKIM-HIZI.md` §10).

## 7. Hukuki not

Sunucu, kullanıcıların indeksini **dağıtan** bir taraf hâline gelir; bu, tek başına
çalışan bir masaüstü uygulamasından farklı bir konumdur. `docs/ARCHITECTURE.md` içindeki
"Legal & Safety" başlığı ve `LICENSE` (AGPL-3.0) burada da geçerlidir: AGPL, ağ üzerinden
hizmet sunulduğunda kaynak kodun sunulmasını da gerektirir. Barındırma yapacak kişi
bulunduğu ülkenin kurallarını kendisi değerlendirmelidir.
