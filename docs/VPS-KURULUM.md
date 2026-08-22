<!-- SPDX-License-Identifier: AGPL-3.0-only -->
# VPS kurulumu — sıfırdan çalışan sunucuya, adım adım

Bu belge `docs/SUNUCU.md`'nin **uygulama** kılavuzudur: hangi sunucuyu, nereden, nasıl
kiralayacağın; kurulumun her komutu; kendi bilgisayarını nasıl bağlayacağın; ve indeksi
ücretli bir abonelik hâline getirmek istersen neyin hazır, neyin daha yazılmadığı.

Toplam maliyet, aşağıdaki seçimle: **ayda ~4 €** (sunucu) + **yılda ~10 €** (alan adı).
Cloudflare tarafı ücretsiz plan ile yeter.

---

## 1. Hangi sunucu — ve neden

**Öneri: Hetzner Cloud, CX22, konum Helsinki (Finlandiya).**

| | CX22 |
|---|---|
| Fiyat | ~3,79–4,35 €/ay (+KDV) |
| CPU / RAM | 2 vCPU / 4 GB |
| Disk | 40 GB SSD |
| Trafik | 20 TB/ay dahil |
| IPv4 | 1 adet dahil |

Neden bu makine yeter — crawler CPU değil **ağ** bekler:

- **Trafik:** harvester saniyede ~120 DHT sorgusu × ~100 bayt ≈ 1 GB/gün; metadata
  çekimi torrent başına ~50 KB. Yoğun çalışmada bile ayda ~50–100 GB. Dahil olan
  20 TB'ın binde beşi.
- **Disk:** ölçüm — 3.404 kayıtlık taze bir istemci veritabanı 36 MB (FTS indeksi ve
  dosya listeleri dahil). Kabaca **kayıt başına ~10 KB** planla: 40 GB ≈ 3–4 milyon
  kayıt. `db_max_gb` ayarı zaten bir tavan koyar.
- **RAM:** 4 GB hem `dragnetd`'ye hem derlemeye yeter (derleme için 2 GB swap ekle).

**En önemli teknik neden: sunucuda NAT yok.** Ev bağlantında modemin bağlantı-izleme
tablosu darboğazdı — internetini kilitleyen şey oydu. Sunucuda o tavan kalkıyor;
üstelik UDP 6881 doğrudan dinlenebildiği için **pasif hasat** (başkalarının sana
gönderdiği `announce_peer`/`get_peers`) açılıyor. Ölçümde bu, aday kalitesinin en iyi
kaynağıydı.

### Konum neden Helsinki

Hetzner Alman şirketi; Almanya telif ihtarları (*Abmahnung*) konusunda dünyanın en
hareketli ülkesi. Finlandiya lokasyonunu seçmek makinenin fiziksel olarak o rejimin
dışında olmasını sağlar. Fiyat aynı.

### Sağlayıcı riski — dürüst değerlendirme

Hetzner'in kullanım şartları "dosya paylaşım araçları"na sıcak bakmaz. Dragnet'in
bu tanıma girmediğini savunabilirsin, çünkü ölçülebilir gerçekler şunlar:

- **İçerik indirmiyoruz.** Peer wire üzerinden yalnız `ut_metadata` (BEP-9) çekiyoruz —
  yani torrent'in *info sözlüğü*: ad, dosya listesi, boyutlar. Tek bir veri parçası
  (piece) indirilmiyor.
- **Hiçbir şey paylaşmıyoruz.** Kodda `announce_peer` yalnızca **gelen** sorgu olarak
  işleniyor; biz göndermiyoruz. Yani hiçbir swarm'a "bu içerik bende var" demiyoruz.
  Telif ihtarlarının dayandığı iddia tam olarak budur ve bizde yok.
- **Trafik profili sakin.** magnetico gibi araçlar saniyede binlerce düğüme yazar;
  Dragnet 40–120 sorgu/sn ile çalışır.

Yine de sıfır risk yoktur. Pratik önlem: **hazır bir açıklama metni bulundur.** Hetzner
şikayet gelirse 24 saat içinde yanıt ister; yukarıdaki üç maddeyi İngilizce yazıp bir
kenarda tut, aynı gün cevapla. Belgelenmiş bir "DHT crawler yüzünden sunucu kapatıldı"
vakasına rastlanmıyor, ama garanti değil — kritik veriyi (indeks) düzenli yedekle (§8).

Alternatifler: **Netcup** (Almanya/Avusturya, benzer fiyat), **OVH** (Fransa/Polonya),
**Contabo** (daha çok kaynak, değişken performans). Hepsinde aynı değerlendirme geçerli.

---

## 2. Sunucuyu kiralama (tıklama tıklama)

1. **Hesap aç:** <https://accounts.hetzner.com> → *Register*. E-postanı doğrula.
2. **Kimlik doğrulama:** ilk siparişte kimlik ve ödeme yöntemi isteyebilir (kredi kartı
   ya da PayPal). Türkiye'den kart sorunsuz çalışır. Onay birkaç dakika–birkaç saat.
3. **Cloud Console:** <https://console.hetzner.cloud> → *New Project* → adı `dragnet`.
4. **SSH anahtarı üret** (kendi Windows makinende PowerShell'de):
   ```powershell
   ssh-keygen -t ed25519 -C "dragnet"
   # Enter'a bas (varsayılan yol), parola sorarsa boş geçebilirsin
   type $env:USERPROFILE\.ssh\id_ed25519.pub
   ```
   Çıkan tek satırı kopyala.
5. **Sunucu oluştur:** *Add Server* →
   - **Location:** Helsinki
   - **Image:** Ubuntu 24.04
   - **Type:** *Shared vCPU* → **CX22**
   - **SSH keys:** *Add SSH key* → 4. adımdaki satırı yapıştır
   - **Name:** `dragnet`
   - *Create & Buy now*
6. Listede beliren **IPv4 adresini** not et. Bundan sonrası SSH ile.

---

## 3. İlk bağlantı ve güvenlik

Windows PowerShell'den:

```powershell
ssh root@SUNUCU_IP
```

Sunucuda sırayla:

```bash
# Güncelle
apt update && apt upgrade -y

# Derleme için swap (4 GB RAM + linkleme için emniyet payı)
fallocate -l 2G /swapfile && chmod 600 /swapfile && mkswap /swapfile && swapon /swapfile
echo '/swapfile none swap sw 0 0' >> /etc/fstab

# Servis kullanıcısı (root olarak çalıştırma)
adduser --system --group --home /var/lib/dragnet dragnet

# Güvenlik duvarı
apt install -y ufw
ufw allow 22/tcp             # SSH
ufw allow 6881/udp           # DHT — ZORUNLU, pasif hasat buna bağlı
ufw allow 6881/tcp           # peer wire (gelen bağlantılar da işe yarar)
ufw --force enable
```

> **Not:** API portunu (8080) **açma.** Dışarıya Cloudflare Tunnel üzerinden çıkacak;
> sunucuda yalnız `127.0.0.1`'de dinleyecek.

---

## 4. Dragnet'i kurma

```bash
# Derleme araçları
apt install -y build-essential pkg-config libssl-dev git curl

# Rust (servis kullanıcısı değil, root ya da kendi kullanıcınla derle)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"

# Kaynak
git clone https://github.com/LordOfTheMachines/dragnet.git /opt/dragnet
cd /opt/dragnet
cargo build --release -p dragnetd
```

Derleme 2 vCPU'da **10–20 dakika** sürer. `dragnet-semantic` bağımlılığı ONNX Runtime
indirir; sunucuda semantik arama kapalı olacağı için çalışırken kullanılmaz, sadece
derlenir.

```bash
install -m 755 target/release/dragnetd /usr/local/bin/dragnetd
mkdir -p /etc/dragnet /var/lib/dragnet
chown dragnet:dragnet /var/lib/dragnet

# Token üret ve SAKLA — istemcilerin gireceği değer bu
openssl rand -hex 32
```

`/etc/dragnet/dragnetd.toml`:

```toml
db_path   = "/var/lib/dragnet/dragnet.db"
api_bind  = "127.0.0.1:8080"
api_token = "BURAYA_YUKARIDAKI_TOKEN"

harvester_port = 6881

# Sunucu değerleri (NAT yok, bu yüzden ev ayarlarından yüksek)
harvester_max_queries_per_sec = 120
fetch_workers                 = 12
fetch_peer_concurrency        = 12
triage_concurrency            = 24

# Sunucu tarar; arama istemcide semantikleşir, burada model gerekmez
semantic_enabled = false
sync_mode        = "local"

db_max_gb        = 30
disk_reserve_gb  = 4
```

```bash
chown dragnet:dragnet /etc/dragnet/dragnetd.toml
chmod 600 /etc/dragnet/dragnetd.toml
```

---

## 5. systemd servisi

`/etc/systemd/system/dragnet.service`:

```ini
[Unit]
Description=Dragnet DHT crawler
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=dragnet
Group=dragnet
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
systemctl daemon-reload
systemctl enable --now dragnet
journalctl -u dragnet -f
```

30 saniyede bir `durum` satırı görmelisin. **İlk saatlerde sabırlı ol:** DHT'de
yerleşiklik kazanmak zaman alır; pasif hasat ancak düğüm kimliği ağın yönlendirme
tablolarına yerleştikten sonra gelir. Kimlik ve bilinen düğümler
`/var/lib/dragnet/dragnet.db.dht0` dosyasında saklanır, yeniden başlatmalar birikimi
sıfırlamaz.

Doğru çalıştığının işareti:

```bash
journalctl -u dragnet | grep "BEP-42"
# → "dış adres çoğunlukla doğrulandı → BEP-42 kimliği kuruldu ip=... oy=8"
```

---

## 6. Alan adı + Cloudflare Tunnel

Tunnel'ın avantajı: sunucuda hiçbir port açmıyorsun, TLS sertifikası yönetmiyorsun,
sunucunun gerçek IP'si dışarıya görünmüyor.

1. **Alan adı al.** En ucuz yol Cloudflare Registrar (maliyetine satar, ~10 $/yıl) ya da
   Namecheap. Aldıktan sonra alan adını Cloudflare hesabına ekle ve kayıt şirketindeki
   **nameserver**'ları Cloudflare'in verdiği ikisiyle değiştir (yayılması 1–24 saat).
2. **Sunucuda cloudflared kur:**
   ```bash
   curl -L https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64.deb -o /tmp/cf.deb
   dpkg -i /tmp/cf.deb
   cloudflared tunnel login          # çıkan bağlantıyı tarayıcıda aç, alan adını seç
   cloudflared tunnel create dragnet
   cloudflared tunnel route dns dragnet dragnet.ornek.com
   ```
3. `/etc/cloudflared/config.yml`:
   ```yaml
   tunnel: dragnet
   credentials-file: /root/.cloudflared/<tunnel-id>.json
   ingress:
     - hostname: dragnet.ornek.com
       service: http://127.0.0.1:8080
     - service: http_status:404
   ```
4. ```bash
   cloudflared service install
   systemctl enable --now cloudflared
   ```
5. **Sına** (kendi bilgisayarından):
   ```powershell
   curl.exe -H "Authorization: Bearer TOKEN" "https://dragnet.ornek.com/stats"
   ```

> **Dikkat:** Tunnel yalnız HTTP API içindir. DHT'nin UDP trafiği tünelden **geçmez** ve
> geçmemelidir — o doğrudan sunucunun kendi arayüzünden akar. Tunnel açmak UDP 6881
> ihtiyacını ortadan kaldırmaz.

Cloudflare panelinde işe yarayan iki ayar: `/search` için 30–60 sn **Cache Rule**, ve
`/changes` için IP başına **Rate limiting** (bu uç nokta tüm indeksi kopyalamaya izin verir).

---

## 7. Kendi bilgisayarını bağlama

Masaüstü uygulamasında **Ayarlar → İndeks kaynağı**:

| Alan | Değer |
|---|---|
| Mod | `remote` (hiç taramaz) veya `hybrid` (hem tarar hem çeker) |
| Sunucu adresi | `https://dragnet.ornek.com` — kök adres, `/changes` eklenmez |
| Sunucu token'ı | §4'te ürettiğin token |

- **`remote`** seçersen çekirdek hiç başlatılmaz: tek bir DHT paketi bile gitmez,
  internetin tamamen serbest kalır. Ölçüldü: istemci logunda 0 DHT satırı.
- **Semantik arama yine yerelde çalışır** — sunucudan gelen kayıtları senin makinendeki
  indeksleyici embed eder.
- İlk senkron kaldığın yerden değil baştan başlar; imleç `meta` tablosunda kalıcıdır,
  uygulamayı kapatıp açmak baştan indirmez.

Başsız bir istemci (başka bir makinede `dragnetd`) için aynısı `dragnetd.toml`'da:

```toml
sync_mode  = "remote"
sync_url   = "https://dragnet.ornek.com"
sync_token = "TOKEN"
```

---

## 8. Bakım ve izleme

```bash
# Canlı durum
journalctl -u dragnet -f

# Aşama aşama hız (son 60 dakika) — kararlar bunun çıktısıyla verilir
cd /opt/dragnet && cargo run --release -p dragnet-store --example rate -- \
  /var/lib/dragnet/dragnet.db 60
```

"MOTOR: n/m kalp atışı" satırı tam değilse ölçümü yorumlama — motor o pencerenin bir
kısmında çalışmamış demektir.

**Yedek** (indeks senin asıl varlığın; sağlayıcı kapatırsa elde kalan tek şey bu):

```bash
# SQLite'ı çalışırken güvenli kopyalamanın doğru yolu
apt install -y sqlite3
sqlite3 /var/lib/dragnet/dragnet.db ".backup '/var/lib/dragnet/yedek.db'"
```

Kendi makinene indir: `scp root@SUNUCU_IP:/var/lib/dragnet/yedek.db .`
Haftalık bir cron makul. Hetzner'in kendi yedekleme seçeneği de var (fiyatın %20'si).

Sunucu güncelleme:

```bash
cd /opt/dragnet && git pull && cargo build --release -p dragnetd
install -m 755 target/release/dragnetd /usr/local/bin/dragnetd
systemctl restart dragnet
```

---

## 9. Ücretli abonelik — neyin hazır, neyin yazılması gerekiyor

### Önce hukuki soru: indeksi paylaşmak zorunda mısın?

**Hayır.** AGPL **kaynak kodunu** kapsar, ürettiğin **veriyi** değil. Ağ üzerinden hizmet
verdiğinde AGPL §13'ün istediği şey, kullanıcılara *yazılımın* kaynağını sunmandır — ve
kod zaten GitHub'da açık, yani şu anda bile uyumlusun. Topladığın indeks senin emeğinin
ürünüdür; istediğin fiyattan, istediğin koşulla verirsin ya da hiç vermezsin.

Bilmen gereken tek incelik ticari, hukuki değil: AGPL sayesinde herkes Dragnet'i indirip
kendi sunucusunu kurabilir. Yani **sattığın şey kod değil**; kesintisiz çalışan bir
sunucunun aylar içinde biriktirdiği indeks ve onun tazeliği. Rekabet avantajın da orada —
sıfırdan başlayan biri, senin bugün sahip olduğun yerleşikliğe haftalarca ulaşamaz.

### Bugünkü kodun sunduğu

- `GET /changes?since=<imleç>&limit=<n>` — artımlı senkron, imleç kalıcı, kayıt atlamıyor
- `api_token` ile bearer koruma (`/search`, `/stats`, `/changes`)
- İstemci tarafı üç mod, uçtan uca doğrulandı

### Abonelik için eksik olanlar (henüz yazılmadı)

1. **Kullanıcı başına token.** Bugün tek bir paylaşımlı token var. Bir abone çıkarsa
   token'ı değiştirmen ve herkese yenisini dağıtman gerekir.
2. **Kota / hız sınırı (abone başına).** Cloudflare IP başına sınırlar; abone başına değil.
3. **Abonelik durumu.** Ödeme bitince erişimin kesilmesi.

### Önerilen mimari (Faz H olarak planlanabilir)

Burada **Cloudflare Workers gerçekten doğru araç** — crawler için değil, **kapı** için:

```
abone → Cloudflare Worker (token doğrula, kota say, süresi geçmişse reddet)
              │  KV: token → {abone_id, bitiş_tarihi, aylık_kota}
              ▼
        Cloudflare Tunnel → VPS: dragnetd /changes
```

Worker ~30 satır: gelen `Authorization` başlığını KV'de arar, geçerliyse isteği Tunnel'a
geçirir, kullanımı sayar. Ödeme sağlayıcısının webhook'u aynı KV'ye yazar (ödeme geldi →
token üret/uzat; iptal → sil). VPS tarafında **hiçbir değişiklik gerekmez** — `dragnetd`
zaten tek token'la korunuyor ve o token yalnız Worker'da durur.

### Ödeme sağlayıcısı (Türkiye'den)

En pratik yol **merchant of record** (MoR) modeli: satıcı olarak onlar görünür, KDV/vergi
uyumunu onlar üstlenir, sen ödemeni alırsın.

| | Komisyon | Not |
|---|---|---|
| **Paddle** | ~%5 + 0,50 $ | MoR, Birleşik Krallık merkezli, Türkiye'den çalışılabilir |
| **Lemon Squeezy** | ~%5 + 0,50 $ | MoR, ABD; 2024'te Stripe satın aldı, ayrı çalışmaya devam ediyor |
| **Stripe** | ~%2,9 + 0,30 $ | Ucuz ama Türkiye'de doğrudan hesap açmak sınırlı; vergi uyumu sende |

Vergi ve şahıs şirketi/limited tarafı için mali müşavire danış — bu belgenin kapsamı dışı.

### Fiyatlandırma düşüncesi

Maliyetin ayda ~4 €. Yani **ayda 5 €'luk tek bir abone bile sunucuyu karşılar.** Makul
bir başlangıç: ücretsiz kademe (günlük sınırlı `/search`, `/changes` yok) + ~3–5 €/ay
premium (tam `/changes` erişimi). Bant genişliği endişesi yok: 100 abonenin tam indeks
kopyası bile 20 TB'ın yanında görünmez.

---

## 10. Hukuki not

Sunucu, indeksi **dağıtan** bir taraf hâline gelir; tek başına çalışan bir masaüstü
uygulamasından farklı bir konumdur. Ücret alınması bunu ticari bir hizmet yapar ve
sorumluluğu artırır. `docs/ARCHITECTURE.md` içindeki "Legal & Safety" başlığı ve
`LICENSE` (AGPL-3.0) burada da geçerlidir. Türkiye'den yürütülen, telifli içeriğe
işaret eden bir arama hizmetinin 5651 sayılı kanun kapsamındaki durumu ayrı bir konudur;
ticari hâle getirmeden önce hukuki görüş almak yerinde olur. Barındırma yapan kişi
bulunduğu ülkenin kurallarını kendisi değerlendirir.
