<!-- SPDX-License-Identifier: AGPL-3.0-only -->
# Faz F — Semantik arama: "mükemmele en yakın" için model iyileştirme planı

Durum (Faz E sonu, 2026-08-18): hibrit (FTS + Gemma-Q4 embedding + bge-reranker) gerçek
DB'de hit@5 %79 / MRR 0.72 (`crates/dragnet-semantic/examples/eval.rs`). Kalan zayıf
sınıflar: yazım hatası ("hery poter"), soyut betimleme ("büyücü çocuk filmi"), Türkçe çeviri
başlık ("taht oyunları dizisi"), dönem sorguları ("2000'lerin bilim kurgu filmleri").
Kademeler: **auto** (donanıma göre) → light (potion) / balanced (MiniLM) / quality (Gemma);
Qwen3-Embedding kademesi elendi (torrent adlarında Gemma'nın altında, CPU'da yavaş, DirectML'de
çalışmıyor).

## Araştırma özeti (2026-08, kaynaklar aşağıda)

1. **Alan-özel ince ayar en büyük kaldıraç.** Bi-encoder'ları hedef alanın (sorgu, doküman)
   çiftleriyle kontrastif ince ayarlamak (sentence-transformers `MultipleNegativesRankingLoss`,
   LoRA ile küçük modellerde tek GPU'da < 1 saat) genel modellere göre alan performansını
   belirgin artırır; sentetik sorgu üretimi (LLM ile doküman başına 3–5 sorgu, negatif
   dokümanla temellendirilmiş) veri kıtlığını çözer. EmbeddingGemma resmî olarak
   sentence-transformers ile ince ayarlanabiliyor (Google örneği MIRIAD) ve ONNX'e
   aktarılabiliyor; MRL (768→256) korunur.
2. **Model2Vec damıtma**: herhangi bir sentence-transformer'dan (ince ayarlanmış Gemma dahil)
   sözcük dağarcığı ileri geçişiyle statik model üretir; veri gerektirmez, 500× hızlı, 15×
   küçük; bağlam duyarlılığı kaybolur ama torrent adları için "bulanık anahtar kelime"
   davranışı iyi (bake-off: potion 0.64 vs Gemma 0.93 hits@5). Alan-özel damıtma potion'u
   geçmeli (özellikle Türkçe sorgu ↔ İngilizce ad hizası öğrenilmiş Gemma'dan).
3. **Reranker**: bge-reranker-v2-m3 2026'da "güvenli varsayılan" (çok dilli, açık lisans, her
   yerde destekli); Qwen3-Reranker 4B/8B kalite lideri ama bizim ölçekte çok ağır (0.6B'si
   ONNX'te DML'de çalışmadı); jina-reranker-v2 daha hızlı. Task-specific ince ayar
   reranker'larda da tutarlı kazanç sağlıyor.
4. **VRAM ölçümü**: DXGI `IDXGIAdapter3::QueryVideoMemoryInfo` (bütçe/kullanım, süreç-yerel,
   okuma-yalnız) → uygulandı (`dragnet_semantic::hw`).

## Plan (öncelik sırasıyla)

### F1 — Değerlendirme setini büyüt (ölçmeden iyileştirme yok)
- `eval.rs` 19 → 100+ sorgu: gerçek DB adlarından örnekleme + kullanıcı sorguları; sınıflar:
  birebir ad, TR doğal dil, yazım hatası, soyut/tema, dönem, kategori niyeti, negatif.
- Metrikler: hit@5, MRR, nDCG@10; kademe × {plan, rerank} matrisi; CI'da çalışmaz (model gerekir),
  ama `--release` ile tek komut.

### F2 — Sentetik eğitim verisi (çevrimdışı, bir kez, kullanıcı GPU'sunda)
- Girdi: indekslenmiş adlar (`text::doc_text`), kategori, ayrıştırılmış başlık/yıl.
- Yerel bir LLM ile (ör. Qwen2.5-7B-Instruct GGUF/ONNX ya da Gemma-3) her ad için 3–5 doğal dil
  sorgu üret: TR + EN, biri "tema" (zombi oyunu), biri "yazım hatalı", biri "dönem/kategori".
  Kalite filtresi: mevcut Gemma ile (sorgu, ad) benzerliği ≥ eşik ya da FTS eşleşmesi.
- Çıktı: `data/train.jsonl` (query, positive, hard_negatives[]) — hard negative'ler
  aynı kategori/benzer başlıklardan (BM25 top-k, doğru olmayanlar).
- Not: bu Python tarafında (`tools/`), Rust ürünü değil; ROADMAP'te "araç" olarak.

### F3 — İnce ayar + dışa aktarım
- **Gemma (quality)**: sentence-transformers + `MultipleNegativesRankingLoss` (+ Matryoshka
  loss 768/256), LoRA (r=16) ya da tam ince ayar; 1 epoch, ~10–50k çift; RTX 4070'te dakikalar.
  ONNX'e aktar (optimum), Q4 quantize; `ModelSpec` `id: "dragnet-gemma-v1"`; kendi GitHub
  release'inden dağıt (Gemma Terms → yalnız türev ağırlıkları paylaşma koşullarına uy).
- **MiniLM (balanced)**: aynı veriyle ince ayar (Apache-2.0, sorunsuz dağıtım).
- **Light**: ince ayarlı Gemma'dan Model2Vec **damıtma** (kendi sözcük dağarcığımızla:
  torrent adlarındaki token'lar + TR/EN sorgu kelimeleri) → `dragnet-potion-v1`.
- Reranker: bge-reranker-v2-m3 aynı çiftlerle ince ayar (opsiyonel; önce F3 embedding
  kazancı ölçülür).
- Kabul: F1 setinde hit@5 ≥ %90, MRR ≥ 0.85 (quality); light ≥ %75.

### F4 — Sorgu-yanı iyileştirmeler (modelden bağımsız, ucuz)

**F4-1 TAMAM (2026-08-18): güven kapısı + TR→EN sözlük — hit@5 %79 → %84, MRR 0.72 → 0.75.**
- Ölçüm: "korpusta karşılığı yok" ayrımını **kosinüs yapamıyor** — klavye zırvası
  ("asdkjhqwe zxcv") 0.421 alırken meşru TR sorgu ("taht oyunları dizisi") 0.332 alıyor;
  gürültü tabanı 0.407 tam ortada, yani yanlış tarafta ayırıyor. Skor-şekli (top1/kuyruk)
  da ayırmıyor (zırva 1.14–1.27 vs meşru zayıf 1.11–1.31), tokenizer parçalanması da
  (zırva 1.62–1.86 krk/parça vs meşru TR 2.00–2.25). **Ayrımı cross-encoder yapıyor**:
  isabetli 15 sorgu −3.72…+6.58; zırva/karşılıksız −5.43, −5.29, −5.14, −5.02, −4.97, −4.62.
  → `WEAK_MATCH_SCORE = −4.5` (model değişirse yeniden ölçülmeli). Kapı yalnız **sözcüksel
  kanıt yokken** uygulanır; API/uygulama `weak: true` döner, liste bilerek boş gelir;
  arayüz "eşleşme bulunamadı" + "Yine de en yakın sonuçları göster" (`show_weak`) sunar.
- TR→EN sözlük (`query::ALIAS_PHRASES` / `ALIAS_WORDS`): çeviri başlıklar (taht oyunları →
  game of thrones …) + tür/tema (bilim kurgu → sci-fi science fiction, büyücü → wizard …).
  "taht oyunları dizisi" MISS r=7 → **OK r=1**. Kelime zaten İngilizce biçimdeyse
  dokunulmaz ("zombies" → "zombie" FTS eşleşmesini bozardı).
**F4-2 TAMAM: yazım düzeltme — hit@5 %84 → %89.** Adaylar indeksin kendi FTS sözlüğünden
(`torrents_vocab` = fts5vocab); harici sözlük yok. Üç ölçüm kararı: (a) düzeltmeyi her
sorguya uygulamak %84 → %74 düşürdü (Türkçe kelimeler korpusta "bilinmeyen") → düzeltme
yalnız **sonuç bulunamayan** sorguda çalışır; (b) kelime kelime en sık aday "hery poter" →
"hero peter" üretti → kombinasyonlar FTS eş-geçişiyle doğrulanır; (c) kısa kelimelerde
düzenleme mesafesi yetmiyor → ünsüz iskeleti (hery→hry) eşleşmesi, mesafe ≤2 şartıyla.

**F4-3 TAMAM: kategori gözatma + kavram sözlüğü + tanınmayan tek kelime — hit@5 %89 → %90
(19/21), MRR 0.82.** (a) Sorgu yalnız kategori kelimesiyse ("oyunlar") arama değil
gözatmadır → kategori filtresiyle listeleme; (b) kavram→örnek genişletmeleri ("işletim
sistemi" → operating system linux ubuntu debian iso); (c) tek kelimelik + sözlükte olmayan
+ düzeltilemeyen + tanıdık sinyal taşımayan sorgu ("mtrix", korpusta Matrix yok) → boş
sonuç; cross-encoder bu sınıfta yanıltıcıydı ("Metro Simulator 2" −1.98 ile geçiyordu).
Aday kombinasyonları toplam yakınlığa göre sıralanır (kartezyen sıra doğru adayı kesime
düşürüyordu). Teşhis aracı: `dragnet-store --example vocab <db> <kelime…>`.

- Kalan: kullanıcı geri beslemesi (F4-4: magnet kopyalama = zayıf pozitif).

- Yazım düzeltme: FTS için trigram/edit-distance önerisi (indeksten sözlük; "hery poter" →
  "harry potter"), semantik zaten kısmen toleranslı.
- Dönem sorgusu: yıl aralığı artırması + adında yıl olmayanlara sezon/yıl çıkarımı;
  "2000'lerin bilim kurgu" gibi soyut tür için tema sözlüğü (bilim kurgu → sci-fi, science
  fiction) sorgu genişletme (TR→EN eşanlam listesi).
- Kullanıcı geri beslemesi: magnet kopyalama = zayıf pozitif → yerel LTR artırması + F2 verisine
  katkı (yalnız yerel; dışarı çıkmaz).

### F5 — Performans/VRAM
- GPU'da fp16 Gemma (1.2 GB) seçeneği: DirectML'de Q4'ten hızlı (162 vs 92 ad/sn) — büyük
  VRAM'li makinelerde `auto` bunu seçebilir (DXGI bütçesine göre).
- Reranker: fp16 sürüm DirectML'de ölçülecek (int8 CPU'dan yavaştı); ilk-30 yerine dinamik
  N (FTS eşleşmesi çoksa 20, azsa 40).
- Sorgu embed önbelleği (LRU 256): aynı sorgu tekrarında 0 ms.
- İndeks: 768d int8 500k = 366 MB RAM; MRL-256 seçeneği (0.93→0.86 hits@5) düşük RAM modu.

### F0 — Durum kartı + canlı VRAM (TAMAM, 2026-08-18)
- **Semantik durum kartı** (Ayarlar → Semantik): kart başlığında aşama rozeti (Kapalı /
  İndiriliyor / Yükleniyor / Hazır / Hata — pano kartlarıyla aynı `pill` dili), altında
  rozetler (Model + kademe, Cihaz, Yeniden sıralayıcı, indeks RAM'i) ve ölçüm çubukları:
  İndirme (dosya + %), İndeks (embed edilen / adı bilinen kayıt), VRAM (kullanım / toplam,
  çubukta bütçe işareti, ipucunda bütçe + oturum tepesi). En altta "Donanım: <GPU> · <VRAM>
  · N çekirdek" ve otomatik kademe gerekçesi; kapalıyken son "VRAM serbest bırakıldı" notu.
- **VRAM "kullanım 0 MB" hatası düzeltildi**: ölçüm artık `status_json()` içinde her
  yoklamada (2,5 s) canlı alınıyor — eskiden `apply()` sırasında, yani model daha
  indirilmeden bir kez okunuyordu. Ölçülen doğrulama (`--example vram quality`, RTX 4070
  Laptop): yükleme öncesi 0 MB → yüklendi (çıkarımsız) **89 MB** → ilk çıkarımdan sonra
  **145 MB** → düşürülünce 0 MB (sızıntı yok, ikinci yüklemede de aynı). Yani DirectML
  tahsisin bir kısmını yüklemede, kalanını ilk çıkarımda yapıyor; tek seferlik ölçüm bu
  yüzden 0/eksik gösteriyordu. Oturum tepe değeri ayrıca tutulur (kapatma notunda "önce"
  olarak kullanılır). ORT DML tahsis sayacıyla çapraz doğrulama gerekmedi.
- Görsel dil pano kartlarıyla hizalı (card-head + rozet + `muted small` açıklama satırı).

### F8 — Dosya yolları + sertleştirme (Python portu incelemesinden, 2026-08-19)

Arkadaşın Python portu (`dragnetpy`) incelendi; mimari örtüşüyor (aynı SQLite+FTS5, int8
nicemleme, %80 göreli kesim, bge-reranker). Bizde olmayan ve alınacak dört şey — bu
sırayla yapılacak:

1. **Dosya yollarını indeksle** (en büyük kalite kazancı). FTS tablosu `fts5(name, paths,
   infohash UNINDEXED, tokenize='unicode61 remove_diacritics 2')` olarak yeniden kurulur;
   semantik doküman metni `ad + kategori + en büyük N dosya yolu` (sınırlı) olur, model
   kimliği `:v3` ile eski vektörler geçersizleşir. Adı `s01` gibi anlamsız olan torrent'ler
   ancak içeriğinden anlaşılabiliyor; kategori tahmini de dosya uzantılarıyla isabetlenir.
   Aynı işlemde **aksan eritme** (`remove_diacritics 2`) gelir: "işletim"↔"isletim",
   "büyücü"↔"buyucu" — sözlükteki elle yazılmış ASCII varyantlarına gerek kalmaz.
   Sıralamada ad ağırlığı yollardan yüksek tutulur (`bm25(torrents_fts, 10.0, 1.0)`).
2. **Dosya ağacı görüntüleyici** (kullanıcı isteği): sonuç satırından tıklanınca torrent'in
   dosya listesi ağaç olarak açılır (boyutlar + toplam). Veri zaten `files` tablosunda.
3. **Peer adres politikası** (güvenlik açığı): bir peer'e TCP bağlanmadan önce adresin
   **global** olduğu doğrulanır; özel (RFC1918), loopback, link-local, multicast, reserved
   ve CGNAT aralıkları reddedilir. Bugün yalnız loopback/unspecified eleniyor → kötü
   niyetli bir DHT düğümü yerel ağa bağlantı denemesi yaptırabilir (DHT→LAN SSRF).
4. **Depolama büyüme freni**: DB bütçesi aşılınca ya da boş disk rezervin altına inince
   yazma (sighting/metadata/embedding) duraklar, indeks salt-okunur sunulmaya devam eder;
   basınç geçince kendiliğinden sürer. Ayarlarda bütçe + rezerv alanı.

Küçük notlar (opsiyonel): model kimliğine bağlam şemasını gömmek (bizde `:v2` elle),
API'de sabit-zamanlı token karşılaştırması + sorgu uzunluğu sınırı + IP başına hız limiti
(varsayılan loopback bind olduğu için düşük öncelikli).

### F7 — Zenginleştirme: "bu torrent gerçekte nedir?" (kullanıcı önerisi, 2026-08-19)

Kategori ve başlık şu an **yalnız ada bakarak** tahmin ediliyor; tavan burada. Öneri: ad
çözümlendikten sonra açık bir katalogdan doğrulanmış meta veri çekip yerel bir bilgi
kütüphanesi kurmak (kanonik ad, tür, yıl, seri, **Türkçe takma adlar**).

- **İlke sınırı korunur**: keşif ve indeks DHT'den gelir. Zenginleştirme *isteğe bağlı*,
  önbellekli ve düştüğünde arama çalışmaya devam eden bir katmandır — site kazıma değil.
- **Kaynak**: birincil **Wikidata** (CC0 → ticari sürümde de sorunsuz, API anahtarı yok,
  film/dizi/oyun/albüm kapsar, **TR etiketleri var** → F4-1'deki elle yazılmış TR→EN
  sözlüğün yerini otomatik alabilir). İkincil: TMDb (film/dizi), IGDB (oyun),
  MusicBrainz (müzik, CC0).
- **Akış**: `dragnet_core::parse` → başlık+yıl → Wikidata eşleştirme → güven eşiği →
  yerel `titles` tablosu → kategori düzeltmesi + sorgu genişletme + seri gruplama.
- **Riskler**: gizlilik (torrent adları üçüncü tarafa gider → varsayılan KAPALI, açık
  onay), yanlış eşleştirme (güven eşiği + yıl doğrulaması), hız limiti (yerel önbellek).
- **Önce prototip**: 500 gerçek ad üzerinde eşleştirme başarımını ölç (kaçı doğru
  eşleşiyor, kaçı yanlış, kaçı eşleşmiyor); ancak sonuç iyiyse tam entegrasyon.

## Kaynaklar
- Fine-tune EmbeddingGemma (Google AI): https://ai.google.dev/gemma/docs/embeddinggemma/fine-tuning-embeddinggemma-with-sentence-transformers
- HF blog EmbeddingGemma: https://github.com/huggingface/blog/blob/main/embeddinggemma.md
- Fine-tune BGE with synthetic data (AWS): https://aws.amazon.com/blogs/machine-learning/fine-tune-a-bge-embedding-model-using-synthetic-data-from-amazon-bedrock/
- Contrastive fine-tuning small models w/ LoRA (arXiv 2507.22729): https://arxiv.org/pdf/2507.22729
- LM-Cocktail (model merging, forgetting): https://arxiv.org/pdf/2311.13534
- Model2Vec: https://github.com/MinishLab/model2vec · https://minish.ai/packages/model2vec/introduction/
- Reranker karşılaştırmaları 2026: https://futureagi.com/blog/best-rerankers-for-rag-2026/ · https://agentset.ai/rerankers/compare/baaibge-reranker-v2-m3-vs-jina-reranker-v2-base-multilingual
- DXGI QueryVideoMemoryInfo: https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_4/nf-dxgi1_4-idxgiadapter3-queryvideomemoryinfo
