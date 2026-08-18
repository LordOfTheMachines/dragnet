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

## Kaynaklar
- Fine-tune EmbeddingGemma (Google AI): https://ai.google.dev/gemma/docs/embeddinggemma/fine-tuning-embeddinggemma-with-sentence-transformers
- HF blog EmbeddingGemma: https://github.com/huggingface/blog/blob/main/embeddinggemma.md
- Fine-tune BGE with synthetic data (AWS): https://aws.amazon.com/blogs/machine-learning/fine-tune-a-bge-embedding-model-using-synthetic-data-from-amazon-bedrock/
- Contrastive fine-tuning small models w/ LoRA (arXiv 2507.22729): https://arxiv.org/pdf/2507.22729
- LM-Cocktail (model merging, forgetting): https://arxiv.org/pdf/2311.13534
- Model2Vec: https://github.com/MinishLab/model2vec · https://minish.ai/packages/model2vec/introduction/
- Reranker karşılaştırmaları 2026: https://futureagi.com/blog/best-rerankers-for-rag-2026/ · https://agentset.ai/rerankers/compare/baaibge-reranker-v2-m3-vs-jina-reranker-v2-base-multilingual
- DXGI QueryVideoMemoryInfo: https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_4/nf-dxgi1_4-idxgiadapter3-queryvideomemoryinfo
