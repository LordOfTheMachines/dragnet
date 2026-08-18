// SPDX-License-Identifier: AGPL-3.0-only
//! Zırva sorgu ayrımı için sinyal ölçümü: modelin çok dilli SentencePiece sözlüğünde
//! gerçek kelimeler az parçaya, klavye zırvası çok parçaya bölünür. Her kelime için
//! "parça sayısı" ve "parça başına karakter" yazdırılır; eşik buradan seçilir.
//! Kullanım: `gibberish <tokenizer.json> "sorgu1" "sorgu2" ...`
use tokenizers::Tokenizer;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let tok = Tokenizer::from_file(&a[1]).expect("tokenizer");
    println!("{:<26} {:>6} {:>7} {:>8}  en kötü kelime", "sorgu", "kelime", "parça", "krk/parça");
    for q in &a[2..] {
        let mut words = 0usize;
        let mut pieces = 0usize;
        let mut chars = 0usize;
        let mut worst = ("", 0f32);
        for w in q.split_whitespace() {
            let enc = tok.encode(w, false).expect("encode");
            let n = enc.get_tokens().len();
            let c = w.chars().count();
            words += 1;
            pieces += n;
            chars += c;
            let ratio = n as f32 / c.max(1) as f32; // parça/karakter: 1.0 = harf harf
            if ratio > worst.1 {
                worst = (w, ratio);
            }
        }
        println!(
            "{:<26} {:>6} {:>7} {:>8.2}  {} ({:.2} parça/krk)",
            q,
            words,
            pieces,
            chars as f32 / pieces.max(1) as f32,
            worst.0,
            worst.1
        );
    }
}
