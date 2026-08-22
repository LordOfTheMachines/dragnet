<!-- SPDX-License-Identifier: AGPL-3.0-only -->
# Metadata çekim hızı — fiziksel sınırlar ve ölçülmüş darboğazlar

Bu belge tek bir soruyu cevaplar: **"Bir torrent'in adının belirmesi ne kadar hızlı
olabilir ve biz nerede sınırlıyız?"** Kaynak, tahmin değil ölçümdür; her sayının nasıl
elde edildiği yazılıdır. Ölçüm araçları:

- `cargo run --release -p dragnet-store --example rate -- <db> [dakika]`
  boru hattının aşama aşama hızı, aday stoğu, harvester sayaçları, sıcak sorgu planları.
- `cargo run --release -p dragnet-meta --example peerstat -- db <db> [n] [conc]`
  **peer hunisi**: bir peer denemesi tam olarak hangi adımda ölüyor.

## 1. Boru hattı bir zincirdir

```
DHT hasadı  →  triyaj  →  çekim denemesi  →  ad indekslendi
(infohash)     (peer var mı?)  (metadata)
```

İsim üretimi, aşamaların çarpımıdır:

```
isim/saat  =  triyaj_hızı  ×  P(sağlıklı)  ×  P(çekim başarılı | sağlıklı)
```

Zincirin en yavaş halkası tavanı belirler. **Bir halkayı hızlandırmak, darboğaz o değilse
hiçbir şey kazandırmaz** — hatta paylaşılan kaynağı (DHT sorgu bütçesi) yiyorsa zarar verir.
2026-08-21 ölçümünde tam olarak bu oluyordu: çekim kapasitesi aday arzının ~7,7 katıydı ve
artan kapasite triyajdan geçmemiş ölü kayıtlara harcanıp asıl aday üreten aşamayı yavaşlatıyordu.

## 2. Ölçülmüş sabitler (2026-08-21, ev bağlantısı, tek düğüm kimliği)

**DHT araması (`get_peers`), rakipsizken:**

| Ölçüm | Değer |
|---|---|
| Arama süresi (bilinen-canlı torrent) | 0,3 – 1,0 sn |
| Arama süresi (üretim adayları, medyan) | 2,7 sn |
| Bulunan peer (bilinen-canlı: Sintel/BBB/ToS) | 247 / 233 / 79 |
| Bulunan peer (triyajdan geçmiş üretim adayları) | medyan **15**, ortalama **64** |

**Peer hunisi** (`peerstat`; bir peer denemesi nerede ölüyor):

| Adım | Bilinen-canlı (n=559) | Üretim adayları (n=1594) |
|---|---|---|
| TCP zaman aşımı | %66,7 | %82,3 |
| TCP reddedildi/hata | %13,2 | %7,1 |
| **TCP bağlandı** | **%20,0** | **%10,5** |
| Bağlandı ama handshake yok (MSE?) | %2,5 | %1,1 |
| **BitTorrent handshake OK** | **%15,6** | **%7,7** |
| BEP-10 extension yok | %0 | %0 |

Yani peer adreslerinin büyük çoğunluğu erişilemez (NAT arkasında ya da bayat DHT kaydı).
Bu **ağın gerçeğidir**, bizim hatamız değildir; tasarımın bunu varsayması gerekir.

Önemli ayrım: üretimdeki `FetchStats` sayacı "TCP bağlanamadı" ile "bağlandı ama handshake
vermedi" durumlarının ikisini de tek `Timeout` kovasında sayıyordu. `peerstat` bunları
ayırdığı için "peer'lerin %97'si zaman aşımı" ifadesinin gerçekte **bağlanamama** olduğu
görüldü — MSE (şifreli bağlantı) hipotezi ölçümle **elendi** (%1-2,5).

## 3. Çekim başarısının matematiği

Bir torrent için metadata çekimi, bağımsız peer denemelerinin birleşimidir:

```
P(başarı)  =  1 − (1 − p)^n
```

- `p` = tek bir peer'den metadata alma olasılığı ≈ **0,03 – 0,08**
  (üretim adaylarında handshake oranı %7,7; bilinen-canlıda %15,6)
- `n` = **gerçekten denenen** peer sayısı

Bu denklem, projedeki en pahalı yanılgıyı açıklar:

| Denenen peer (n) | p = 0,04 | p = 0,08 |
|---|---|---|
| 1 | %4 | %8 |
| 2,5 | %10 | %19 |
| **15** | **%46** | **%71** |
| 40 | %80 | %96 |

Ayar `max_peers = 40` idi, ama üretim sayacı çekim başına **2,5 peer** görüyordu — çünkü
denemelerin ~%87'si triyajdan geçmemiş, gerçekten peer'i olmayan kayıtlara gidiyordu.
Aynı adaylarda bağımsız ölçüm (`peerstat`) **medyan 15 / ortalama 64** peer bulabiliyordu.

**Sonuç: darboğaz "peer bulunamaması" değil, bulunan peer'lerin kullanılmamasıydı.**
Triyaj her aday için tam bir DHT araması yapıp bulduğu adresleri **çöpe atıyor**, ardından
çekim aşaması aynı infohash için aramayı **sıfırdan tekrarlıyordu**. Düzeltme (F13): triyaj
adresleri ipucu olarak devrediyor; çekim ipuçları yeterse DHT aramasını hiç yapmıyor.

## 4. Fiziksel tavan: DHT sorgu bütçesi

Zincirdeki her aşama aynı kıt kaynağı harcar: **giden UDP sorgusu**. Bir iteratif
`get_peers` araması Kademlia'da k=20 komşuluk üzerinden ilerler ve pratikte **~50 sorgu +
~50 yanıt** eder.

Sınırlayan şey bant genişliği değildir:

```
300 sorgu/sn × ~100 bayt  =  ~30 KB/s giden      (önemsiz)
300 yanıt/sn × ~500 bayt  =  ~150 KB/s gelen     (önemsiz)
```

Sınırlayan şey **NAT/conntrack tablosudur**. Her yeni hedefe giden UDP paketi ev
modeminde bir bağlantı-izleme girdisi açar ve bu girdiler 30–180 sn yaşar:

```
300 sorgu/sn × 60 sn eşzamanlı yaşam  ≈  18.000 girdi
```

Tipik ev modeminin tablosu 4.000–16.000 girdidir. Yani **~100–300 sorgu/sn** pratik
tavandır; aşılırsa modem tablosu taşar ve **tüm ev interneti** etkilenir (projede bu
gözlemlendiği için `harvester_max_queries_per_sec` bilerek nazik tutulmuştur).

Bu tavanı aramaya çevirirsek:

```
250 sorgu/sn ÷ 50 sorgu/arama  =  ~5 arama/sn  =  ~18.000 DHT araması/saat
```

**Bu, sistemin bütününe verilmiş bir bütçedir** — triyaj, çekim ve harvester bunu paylaşır.
Toplam isim üretimi tavanı:

```
18.000 arama/saat × P(sağlıklı) × P(çekim | sağlıklı)
```

Ölçülen `P(sağlıklı) ≈ %11` (triyaj edilen adayların yalnız bu kadarında en az 1 peer var)
ile:

```
18.000 × 0,11 × 0,45  ≈  890 isim/saat        (ipuçları kullanılırsa, n≈15)
18.000 × 0,11 × 0,10  ≈  200 isim/saat        (ipuçları atılırsa, n≈2,5)
```

**Yani ipucu devri tek başına ~4x'lik bir farktır ve tavan ~900 isim/saat mertebesindedir.**
Ölçüm başlangıcı 146 isim/saat idi.

## 5. Bu tavanı yükseltmenin gerçek yolları

Matematik, hangi kaldıraçların işe yarayacağını da söyler. `isim/saat` çarpımındaki üç
terimden hangisi ucuza büyütülebilir?

1. **`P(sağlıklı)` — şu an %11.** En büyük kaldıraç budur ve DHT bütçesi harcamaz.
   Sebebi, adayların BEP-51 örneklemesinden gelmesidir: örnekleme, ağın **deposundan**
   rastgele infohash verir ve bunların çoğu ölüdür. Pasif `announce_peer` ise
   niteliksel olarak farklıdır: announce eden düğüm o torrent'i **o anda paylaşıyordur**
   ve bize UDP paketi ulaştırabildiği için erişilebilirdir. Aynı bütçeyle çok daha
   kaliteli aday demektir.
   → Gereken: modemde **port yönlendirme** (6881/UDP) ve düğüm kimliğinin oturumlar
   arasında **korunması** (yönlendirme tablolarında yer edinmek saatler alır; kimlik her
   açılışta değişirse birikim sıfırlanır — F13'te kalıcı kimlik eklendi).

2. **`P(çekim | sağlıklı)` — n'i büyüterek.** `1 − (1−p)^n` denklemi n'de hızla doyar:
   n=15 → %46, n=40 → %80. İpuçları 16 adresle sınırlıdır (`TRIAGE_PEER_CAP`); bunu
   büyütmek DHT araması gerektirmez, yalnız daha çok **TCP** denemesi gerektirir — ve TCP
   bağlantıları UDP'den farklı bir kaynaktır. Ölçülerek artırılabilir.

3. **`triyaj_hızı`** — zaten bütçe sınırındadır. Artırmak, doğrudan modem tablosunu
   zorlar. **Buradan kazanç aramak boşa kürektir**; asıl kazanç 1 ve 2'dedir.

## 6. Hasadın sessiz ölümü: düğüm kuyruğunun kuruması

Yukarıdaki matematik "aday kalitesi" üzerineydi. Ama ölçüm, ondan önce gelen ve çok daha
büyük bir hatayı ortaya çıkardı: **harvester sorgu bütçesinin %97'sini kullanamıyordu.**

| Sayaç | Düzeltme öncesi | Düzeltme sonrası |
|---|---|---|
| Gönderilen DHT sorgusu | **1,5/sn** (bütçe 50/sn) | **50/sn** (bütçe dolu) |
| BEP-51 örnek (aktif hasat) | 0,9/sn | **90/sn** |
| Öğrenilen düğüm | ~0/sn | **87/sn** |
| Sorgu başına gelen yanıt | %6 | %35 |

Üç ayrı kusur birlikte çalışıyordu:

1. **Kanıtlanmış canlı düğümler atılıyordu.** `crawl_loop`, sorgulayacağı düğümü kuyruktan
   `pop` eder ve bir daha geri koymazdı. Yani bize **yanıt vermiş** (dolayısıyla canlı
   olduğu kanıtlanmış) düğümler bir kez kullanılıp çöpe gidiyor, yerlerini yanıtlardan
   gelen kanıtlanmamış — ve DHT'de çoğunluğu ölü — adresler alıyordu. Kuyruk böyle kurur.
   Düzeltme: yanıt veren düğüm kuyruğun sonuna geri eklenir.

2. **Kuyruk boşken DNS fırtınası.** Kuyruk kuruyunca `crawl_loop` **her tıkta** (100 ms)
   `seed_bootstrap` çağırıyordu: saniyede 10 kez, 4 bootstrap adının DNS çözümlemesi. Bu
   bir `await` olduğu için döngünün kendisi DNS'e kilitleniyordu — yani kuyruk boşken hasat
   toparlanmak yerine tamamen duruyordu. Düzeltme: yeniden tohumlama en fazla 5 sn'de bir.

3. **Windows'ta ICMP kaynaklı soket hataları.** Bir DHT hedefi kapalıysa ICMP "port
   unreachable" döner; Windows bunu bağlantısız bir UDP soketinde bile **bir sonraki**
   `recv_from`/`send_to` çağrısını `WSAECONNRESET` ile başarısız kılarak bildirir. Bir
   crawler doğası gereği sürekli ölü hedefe paket yollar, dolayısıyla soket sürekli hata
   verir — ve hatalar `debug!` ile yutulduğu için **sebep görünmez**. Düzeltme:
   `SIO_UDP_CONNRESET = false` (UNIX davranışı) + hataların sayaçlanması.

**Ders:** yutulan hata, olmayan hatadan tehlikelidir. Üç kusur da "çökme" üretmiyordu;
yalnız sistemi yavaşlatıyorlardı ve hiçbiri loglara yansımıyordu. Bu yüzden `metrics`
tablosuna soket hatası, öğrenilen düğüm ve gelen yanıt sayaçları eklendi: bir daha aynı
biçimde sessizce ölemesin.

## 7. F13 sonrası ölçüm (2026-08-21)

Aynı makine, aynı ayarlar (`fetch_workers = 24`, `fetch_peer_concurrency = 16`,
`harvester_max_queries_per_sec = 50`), aynı veritabanı:

| Ölçüm | Önce | Sonra | Kat |
|---|---|---|---|
| Giden DHT sorgusu | 1,5/sn | **46/sn** (bütçenin %92'si) | 31× |
| BEP-51 örnek (aktif hasat) | 0,9/sn | **85/sn** | 94× |
| Öğrenilen düğüm | ~0/sn | **124/sn** | — |
| Sorgu başına gelen yanıt | %6 | **%34** | 5,7× |
| Örneklerde tekrar (dedup) | %99,7 | **%64** | — |
| Triyaj (aday ölçümü) | 1.406/saat | **11.260/saat** | 8× |
| **İsim üretimi** | **146/saat** | **~270–400/saat** | **~2×** |

Son satırdaki aralık gerçektir, belirsizlik değil: aynı kurulumda ardışık pencereler
271, 344, 399 ve 408/saat verdi. Oynaklık bu işin doğasında var (bkz. §9) ve tek bir
pencereye bakıp "şu kadar oldu" demek, bu projede daha önce yanlış kararlara yol açtı.

Son satır neden yalnız 2,2×? Çünkü zincirin ilk halkaları açıldıktan sonra tavanı artık
**aday kalitesi** belirliyor: triyaj edilen adayların %88'inde DHT'de hiç peer yok
(`P(sağlıklı) ≈ %12`). Bol miktarda infohash bulmak, çekilebilir torrent bulmakla aynı
şey değil. Ayrıca bu ölçüm **ısınma dönemindedir**: pasif trafik (gelen `announce`)
hâlâ saatte tek haneli, çünkü ağın yönlendirme tablolarında yer edinmek saatler alır ve
ölçüm boyunca uygulama defalarca yeniden başlatıldı.

**Buradan sonraki en büyük kaldıraç `P(sağlıklı)`'dir** (§5.1): modemde 6881/UDP
yönlendirmesi + kimliğin korunması → gelen `announce_peer` sayısı artar → aday kalitesi
BEP-51 örneklemesinin çok üstüne çıkar. İkinci kaldıraç `TRIAGE_PEER_CAP`'i büyütmektir
(§5.2): DHT bütçesi harcamaz, yalnız TCP denemesi ister.

## 8. Çekimin çöküşü: aday sıralaması ölü aday seçiyordu (2026-08-22)

F13 sonrası panoda çekim başarı oranı **%0**'a düştü: 29.572 denemede yalnız 122 başarı,
74.570 zaman aşımı. Kullanıcının iki hipotezi vardı — zaman aşımı çok kısa olabilir, ya da
port yönlendirmesi çalışmıyordur. İkisi de ölçüldü; **asıl sebep üçüncü bir şeydi.**

### Ölçüm 1 — eşzamanlılık ve zaman aşımı (`peerstat sweep`)

Aynı peer havuzu, her ayara tüm torrentlerden eşit örnek:

| Ayar | bağlandı | handshake | metadata | süre | verim |
|---|---|---|---|---|---|
| conc=8 | %13,7 | %10,7 | %3,3 | 120 s | 0,08 md/sn |
| conc=32 | %16,7 | %12,4 | %3,7 | 32 s | 0,35 |
| **conc=96** | **%18,1** | %13,0 | %4,7 | **12 s** | **1,17** |
| conc=384 | %14,4 | %11,7 | %3,0 | 11 s | 0,82 |
| conc=32, to=10 s | %17,4 | %14,4 | %3,7 | 80 s | 0,14 |
| conc=32, to=20 s | %18,4 | **%15,4** | %4,7 | 147 s | 0,10 |

İki sonuç: (a) eşzamanlılık 384'te bağlanma oranını düşürüyor — modem tablosu taşıyor —
ve verim **96**'da tepe yapıyor; (b) zaman aşımını uzatmak handshake oranını gerçekten
artırıyor (%12,4 → %15,4, yani yavaş ama canlı peer'ler var) ama süreyi 12× uzattığı için
**verimi 13 kat düşürüyor**. Yani "daha uzun bekleyelim" doğru sezgi, yanlış çözüm.

Ama bu tablodaki en düşük satır bile %13 bağlanma verirken üretim **%0,25** veriyordu:
50 kat fark ve bunu ne eşzamanlılık ne zaman aşımı açıklıyor.

### Ölçüm 2 — eşzamanlı DHT araması (`peerstat lookups`)

Şüphe: tek `mainline` istemcisini 48 arama paylaşıyor, aktör döngüsü tıkanıyor olabilir.
Ölçüm bunu **çürüttü** — 48 eşzamanlı aramada bile arama başına 19,7 peer bulunuyor.

### Ölçüm 3 — ADAY SIRALAMASI (`peerstat ordertest`) ← sebep burada

Aynı depo, 40'ar aday, aynı ağ koşulu:

| Sıra | peer/aday | hiç peer'i olmayan |
|---|---|---|
| **`probe_peers DESC`** (üretimin kullandığı) | **0,1** | **%90** |
| `probe_at DESC` | 19,6 | %25 |
| **`last_seen DESC`** | **36,4** | %28 |

Üretim, "triyajda en çok peer ölçülmüş" adayı önce çekiyordu. Ama yüksek `probe_peers`
değerleri **eski** ölçümlerden gelir; o torrentler çoktan ölmüştür. Üstelik bu kayıtlar
sıranın hep başında durduğu için **taze adaylar hiç sıra alamıyordu** (açlık). Aday başına
0,1 peer ile metadata çekmek imkânsızdır — çekim boşa kürek çekiyordu.

Düzeltme: `ORDER BY last_seen DESC` (+ `idx_fetch_fresh`). `probe_peers` canlılık **kanıtı**
olarak WHERE'de kalır, ama **önceliği tazelik belirler**.

**Ders:** "en iyi aday" ölçütü zamanla bayatlıyorsa, sıralama ölçütü olamaz — yalnız
filtre olabilir. Bayat bir ölçüt sıralamada kullanılırsa sistem kendini eski verinin
içine kilitler ve taze veriyi hiç göremez.

### Sıralama seçimi: `last_seen` değil `probe_at`

İlk düzeltmede `last_seen DESC` seçildi (tablodaki en yüksek peer/aday değeri) ama
üretimde deneme başına başarı %2,4'ten %1,2'ye **düştü**. Sebep, testin kapsamıydı:
`ordertest` adayları `probe_peers > 0` ile, yani yalnız triyajdan geçmişlerden seçiyordu.
Üretimin `WHERE`'i ise daha geniştir — hint'li ya da "sıcak" ama henüz **ölçülmemiş**
kayıtları da alır. `last_seen DESC` bu kanıtsız adayları kanıtlıların önüne geçiriyordu.

`probe_at DESC` ikisini birden verir: en son **ölçülen** aday önce gelir, hiç ölçülmemişler
(`probe_at = 0`) doğal olarak sona düşer ve önce triyajdan geçerler. Bu, "dar bir testin
sonucunu geniş bir bağlama taşımanın" tipik tuzağıdır — testin filtresi, üretimin filtresi
değildi.

### Sonuç (2026-08-22, 31 dakikalık pencere)

| Ölçüm | Çöküş anında | F13 öncesi | Düzeltme sonrası |
|---|---|---|---|
| İsim üretimi | 38/saat | 146/saat | **~241–280/saat** |
| Deneme başına başarı | %0,4 | %2,4 | **%3,1** |
| **Triyajda ölü çıkan** | %88 | %88 | **%58,8** |
| BEP-51 örnek | 62/sn | — | **208/sn** |
| Keşfedilen infohash | — | — | **421.000/saat** |
| Gelen `get_peers` (pasif) | — | — | 1.921/saat |

**Beklenmeyen bulgu — düğüm kuyruğu aday KALİTESİNİ de belirliyor.** Kuyruk 8.192 →
65.536 yapıldığında triyajda ölü çıkan oran %88'den **%58,8'e** düştü, yani sağlıklı aday
oranı %12'den %41'e çıktı. Beklenen etki yalnız tekrar oranıydı (aynı düğüme daha seyrek
sormak); ama aynı zamanda **daha çeşitli düğüme** soruluyor ve BEP-51 örnekleri ağın daha
geniş bir kesitinden geliyor. Dar bir düğüm kümesi yalnız aynı infohash'leri değil, aynı
*kalitede* (bayat) infohash'leri veriyormuş.

Bu, §1'deki zincir denklemindeki `P(sağlıklı)` teriminin — §5'te "en büyük kaldıraç" diye
işaretlenen ve port yönlendirmesi gerektirdiği sanılan terimin — aslında **tamamen kendi
kontrolümüzdeki bir ayarla** üçe katlanabildiğini gösterir.

Uygulanan ayarlar: eşzamanlı TCP 384 → **96** (ölçülen verim tepesi), triyaj eşzamanlılığı
24 → **12** (her triyaj ~50 UDP paketi), harvester bütçesi 50 → **120/sn** ve `crawl_batch`
4 → **16** (harvester sorguları tek pakettir, ucuzdur), düğüm kuyruğu 8.192 → **65.536**.

## 9. Çürüyen hipotezler (tekrar denemeyin)

Bunlar ölçülüp reddedildi; gerekçeleri `docs/PLAN-FAZ-F.md` §F9–F12'de:

- **uTP yedek yolu**: 9.068 denemede 21 başarı (%0,2); ortalama çekim 3,1 → 35,4 sn.
- **Zaman aşımlarını kısaltmak**: bağlantı 3,5 → 1,8 sn → isim üretimi 315 → 37/saat
  (başarılı bağlantıların önemli kısmı 2–3 sn arasında).
- **Çoklu düğüm kimliği**: 4 kimlik → 171/saat, tek kimlik ~255-325/saat.
- **Sıcak kayıtlarda kısa yeniden deneme** (6 saat → 20 dk): 315 → 157/saat.
- **MSE (şifreli bağlantı) hipotezi**: `peerstat` ile ölçüldü — bağlanabilen peer'lerin
  yalnız %1–2,5'i handshake vermiyor. Sorun şifreleme değil, **erişilemezlik**.

## 10. Ölçüm disiplini

- **Kısa pencere yalan söyler.** 7–8 dakikalık pencerelerde aynı kurulum 255 ile 325
  arasında ölçüldü. Karar için en az 30 dakika, tercihen "son 1 saat" kullanılmalı.
- **Satır saymak yalan söyler.** Boru hattının iki aşaması işini bitirince kaydı siler
  (triyajda sıfır peer → silme; deneme hakkı bitince → silme). Bu yüzden hız, tablodaki
  satırlardan **ölçülemez**: bir ölçümde satır sayımı triyajı 1.317/saat gösterirken
  gerçek hız ~11.000/saat idi. Hızlar `metrics` tablosundaki **olay sayaçlarından** okunur.
- **Yeniden başlatma ölçümü kirletir.** Pasif hasat ağdaki yerleşikliğe bağlıdır ve bu
  birikim saatler alır; süreç yeniden başlayınca düşer. A/B karşılaştırmaları aynı
  yerleşiklik koşulunda yapılmalıdır.
- **Aracın kendisi de yanılabilir.** `rate` aracının sorgu planı bölümü bir süre elle
  kopyalanmış SQL metinlerini planlıyordu; kopya bayatlayınca düzeltilmiş sorgular için
  hâlâ "TEMP B-TREE" raporladı. Artık planlar `dragnet_store::queries` içindeki **çalışan**
  sorgudan çıkarılıyor.

## 11. Gece boyu çalıştırma: neyin işe yaramadığı (2026-08-22)

Bir gece kesintisiz çalıştırma, kısa pencerelerde iyi görünen iki değişikliğin aslında
**zarar verdiğini** gösterdi. İkisi de bu belgede "kazanç" diye yazılmıştı; ölçüm
disiplininin (§10) neden bu kadar önemli olduğunun kanıtı.

| | F13 öncesi | Gece sonrası |
|---|---|---|
| İsim üretimi | 146/saat | **112/saat** |
| Ort. peer/çekim | 2,5–3,1 | **2,0** |
| Bekleyen kuyruk | 39.000 | **102.278** |
| BEP-51 örnek | 62/sn | 493/sn |

### Hata 1 — hasadı hızlandırmak çekimi öldürüyor

`crawl_batch` 4 → 16 ve bütçe 50 → 120 infohash keşfini gerçekten 8 katına çıkardı.
Ama isim üretimi düştü. Kanıt tartışmasız: uygulama çalışırken **yeni bir DHT istemcisi
ağa bootstrap bile edemiyordu** (`bootstrap: false`), ve aynı adaylarda peer bulma
ölçümü 7,3 → 1,1 peer/aday'a düşüyordu; uygulama kapatılıp ağ toparlanınca geri
yükseliyordu.

Yanılgının kaynağı §4'teki "harvester sorguları tek pakettir, ucuzdur" ifadesiydi.
Sorgu tek pakettir ama **yanıtı da pakettir** ve saniyede yüzlerce örnek hacminde bu,
modemi doyurur. Doğrusu: *bu boru hattında infohash **bulmak** darboğaz değil —
DHT'de yüz binlerce aday zaten birikmiş durumda. Kıt kaynak ağ bütçesidir ve onu
çekim tarafına bırakmak gerekir.*

### Hata 2 — tazelik ve peer sayısı, ikisi de tek başına yetersiz

§8'de `probe_at DESC` seçilmişti. Gece ölçümünde bu sıra en kötülerden çıktı:

| seçim | peer/aday | hiç peer'i olmayan |
|---|---|---|
| `probe_at DESC` (sıralama olarak) | 1,4 | %48 |
| `probe_peers DESC` (sıralama olarak) | 8,3 | **%75** |
| **taze FİLTRE + `probe_peers` SIRA** | **7,3** | **%38** |

Yalnız tazelik, triyajın ürettiği rastgele BEP-51 örneklerini öne çıkarıyor — ve
kuyruğun **%60,9'u tam 1 peer'li** (yeni araç: `--example probedist`). Yalnız peer
sayısı ise çoktan ölmüş eski ölçümleri öne çıkarıyor. Doğrusu ikisini **ayrı rollerde**
kullanmak: tazelik `WHERE`'de filtre, peer sayısı `ORDER BY`'da sıra.

### Hata 3 — sıcak adaylar hiç sıra almıyordu

Panoda 11.107 "sıcak" aday (pasif trafikte az önce görülmüş) beklerken hiçbiri
çekilmiyordu: çoğu henüz triyajdan geçmemiştir (`probe_peers = -1`) ve
`ORDER BY probe_peers DESC` sırasında -1 en sona düşer. Oysa ölçümde sıfır-peer oranı
**en düşük** kaynak bunlardı (%35, triyaj ortalaması %48). Artık kotanın yarısı bu
kaynağa ayrılmıştır (`NEXT_TO_FETCH_HOT`).

### Kalan gerçek sınır: peer başına başarı

Üretimde peer başına metadata başarısı **%0,7**; aynı adaylarda ağ boştayken yapılan
ölçüm **%4** veriyor. Aradaki 6 kat, uygulamanın kendi ağ yükünden geliyor. `P(başarı)
= 1-(1-p)^n` denkleminde p bu kadar düşükken, n'i (peer sayısı) büyütmek de sınırlı
kalıyor. Bu yüzden bir sonraki adım **kapasiteyi düşürüp p'yi yükseltmeyi** ölçmektir:
daha az eşzamanlı iş, ama her birinin başarı şansı daha yüksek.

## 12. Düğüm kimliği saniyede birkaç kez değişiyordu (2026-08-22)

Üretim logunda bulundu — aynı saniye içinde, dönüşümlü:

```
dış adres öğrenildi → BEP-42 kimliği kuruldu ip=31.217.179.223
dış adres öğrenildi → BEP-42 kimliği kuruldu ip=159.146.35.97
dış adres öğrenildi → BEP-42 kimliği kuruldu ip=190.14.141.23
```

Log boyunca **3.834** kimlik değişim olayı vardı. Sebep: BEP-42 için dış adresimizi
yanıtlardaki `ip` alanından öğreniyoruz, ama DHT'de bazı düğümler **yanlış adres
bildiriyor** (hatalı istemci ya da kasıtlı) ve kod her bildirimde kimliği yeniden
kuruyordu.

Bunun bedeli, §5'te "en büyük kaldıraç" diye işaretlenen şeyin ta kendisi: ağdaki
yönlendirme tablolarında yer edinmek **saatler** alır ve gelen `announce_peer` ancak o
birikimden sonra gelir. Kimlik saniyede birkaç kez değişince birikim hiç oluşmaz —
ölçümde gelen announce saatte **39**'da kalıyordu. Yani F13'te eklenen "kalıcı kimlik"
düzeltmesi, çalışma sırasında kimliğin sürekli değişmesi yüzünden pratikte etkisizdi.

Düzeltme: adresler **oylanır** (`Shared::vote_public_ip`). Kimlik ancak bir adres
eşiği (8 oy) geçip ikinciyi açık farkla (2×) geçtiğinde değiştirilir; oylar ara sıra
yarıya indirilerek soldurulur, böylece adres gerçekten değişirse yenisi öne geçebilir.
Doğrulama (üretim logu): oylama doğru adresi seçti (`oy=8`) ve o andan sonra kimlik
hiç değişmedi.

**Ders:** güvenilmeyen ağdan gelen bir değeri, tek bir kaynaktan duyduğu için kabul eden
her kod yolu bir saldırı ya da bozulma yüzeyidir — hele o değer sistemin kimliğini
belirliyorsa. DHT'de "birden çok kaynaktan doğrula" varsayılan tutum olmalıdır.
