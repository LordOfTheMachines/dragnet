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
| **İsim üretimi** | **146/saat** | **~320/saat** | **2,2×** |

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

## 8. Çürüyen hipotezler (tekrar denemeyin)

Bunlar ölçülüp reddedildi; gerekçeleri `docs/PLAN-FAZ-F.md` §F9–F12'de:

- **uTP yedek yolu**: 9.068 denemede 21 başarı (%0,2); ortalama çekim 3,1 → 35,4 sn.
- **Zaman aşımlarını kısaltmak**: bağlantı 3,5 → 1,8 sn → isim üretimi 315 → 37/saat
  (başarılı bağlantıların önemli kısmı 2–3 sn arasında).
- **Çoklu düğüm kimliği**: 4 kimlik → 171/saat, tek kimlik ~255-325/saat.
- **Sıcak kayıtlarda kısa yeniden deneme** (6 saat → 20 dk): 315 → 157/saat.
- **MSE (şifreli bağlantı) hipotezi**: `peerstat` ile ölçüldü — bağlanabilen peer'lerin
  yalnız %1–2,5'i handshake vermiyor. Sorun şifreleme değil, **erişilemezlik**.

## 9. Ölçüm disiplini

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
