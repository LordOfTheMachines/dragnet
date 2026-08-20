// SPDX-License-Identifier: AGPL-3.0-only
//! Yazım düzeltme: **indeksin kendi sözlüğünden** öneri (F4-2).
//!
//! Harici bir sözlük kullanılmaz — düzeltme adayları FTS indeksinde geçen terimlerdir
//! (`fts5vocab`), yani korpusta gerçekten bulunan kelimeler. Böylece "hery poter" →
//! "harry potter" düzeltmesi ancak korpusta Harry Potter varsa yapılır; olmayan bir şeye
//! yönlendirme olmaz. Aday seçimi: aynı ilk harf + uzunluk farkı ≤ 2 kovasında
//! Damerau-Levenshtein mesafesi; eşitlikte **daha sık geçen** terim kazanır.

use std::collections::HashMap;

/// Kelime → düzeltme dizini. Bellek: terim başına bir String + u32 (50k terim ≈ 1–2 MB).
#[derive(Debug, Default)]
pub struct SpellIndex {
    /// (ilk harf, uzunluk) → o kovadaki (terim, doküman frekansı).
    buckets: HashMap<(char, usize), Vec<(String, u32)>>,
    /// Ünsüz iskeleti → terimler. "hery"/"harry" → `hry`, "mtrix"/"matrix" → `mtrx`.
    /// Sesli harf düşmesi/fazlalığı ve harf tekrarı hataları edit mesafesiyle 2'yi
    /// aştığı için (kısa kelimelerde riskli), bu sınıf iskeletle yakalanır.
    skeletons: HashMap<String, Vec<(String, u32)>>,
    /// Bilinen terimler (düzeltmeye gerek var mı kontrolü).
    known: std::collections::HashSet<String>,
}

/// Ünsüz iskeleti: küçük harf, sesliler atılır (ilk harf korunur), ardışık tekrarlar
/// sadeleşir. "potter" → `ptr`, "poter" → `ptr`, "witcher" → `wtchr`.
fn skeleton(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    for (i, c) in word.to_lowercase().chars().enumerate() {
        let vowel = matches!(c, 'a' | 'e' | 'i' | 'ı' | 'o' | 'ö' | 'u' | 'ü' | 'y');
        if vowel && i > 0 {
            continue;
        }
        if !out.ends_with(c) {
            out.push(c);
        }
    }
    out
}

impl SpellIndex {
    /// `terms`: (terim, kaç dokümanda geçtiği). Terimler küçük harfe çevrilir.
    pub fn build(terms: impl IntoIterator<Item = (String, u32)>) -> Self {
        let mut me = Self::default();
        for (t, freq) in terms {
            let t = t.to_lowercase();
            let n = t.chars().count();
            if n < 3 {
                continue;
            }
            let Some(first) = t.chars().next() else {
                continue;
            };
            me.known.insert(t.clone());
            me.skeletons
                .entry(skeleton(&t))
                .or_default()
                .push((t.clone(), freq));
            me.buckets.entry((first, n)).or_default().push((t, freq));
        }
        me
    }

    pub fn len(&self) -> usize {
        self.known.len()
    }
    pub fn is_empty(&self) -> bool {
        self.known.is_empty()
    }
    pub fn contains(&self, word: &str) -> bool {
        self.known.contains(&word.to_lowercase())
    }

    /// `word` için düzeltme önerisi. Kelime zaten sözlükteyse ya da uygun aday yoksa `None`.
    /// Mesafe bütçesi: 4–5 harf → 1, 6+ harf → 2 (kısa kelimelerde 2 mesafe anlamı bozar).
    pub fn suggest(&self, word: &str) -> Option<&str> {
        let w = word.to_lowercase();
        let n = w.chars().count();
        if n < 4 || self.known.contains(&w) {
            return None;
        }
        let budget = if n >= 6 { 2 } else { 1 };
        let first = w.chars().next()?;
        // 1) Ünsüz iskeleti eşleşmesi. KORUMA (ölçümle eklendi): iskelet tek başına çok
        // gevşek — İngilizce korpusta doğal olarak bulunmayan Türkçe sorgu kelimeleri
        // ("tavşan", "animasyonu") rastgele terimlere düzeltiliyordu ve hit@5 %84 → %74
        // düşmüştü. Bu yüzden iskelet eşleşmesi de mesafe (≤2), uzunluk farkı (≤2) ve
        // en az 3 ünsüzlük iskelet şartına bağlıdır.
        let sk = skeleton(&w);
        if sk.chars().count() >= 2 {
            if let Some(cands) = self.skeletons.get(&sk) {
                let best = cands
                    .iter()
                    .filter(|(t, _)| t.chars().count().abs_diff(n) <= 2 && damerau(&w, t, 2) <= 2)
                    .max_by_key(|(_, f)| *f);
                if let Some((t, _)) = best {
                    return Some(t.as_str());
                }
            }
        }
        // 2) Düzenleme mesafesi (harf değişimi/yer değiştirme gibi diğer hatalar).
        let mut best: Option<(usize, u32, &str)> = None; // (mesafe, frekans, terim)
        for len in n.saturating_sub(budget)..=n + budget {
            for key in [(first, len)] {
                let Some(cands) = self.buckets.get(&key) else {
                    continue;
                };
                for (t, freq) in cands {
                    let d = damerau(&w, t, budget);
                    if d > budget {
                        continue;
                    }
                    let better = match best {
                        None => true,
                        Some((bd, bf, _)) => d < bd || (d == bd && *freq > bf),
                    };
                    if better {
                        best = Some((d, *freq, t.as_str()));
                    }
                }
            }
        }
        best.map(|(_, _, t)| t)
    }

    /// Bir kelime için **en iyi k aday** (mesafe, sonra frekans sırasıyla). Tek aday
    /// yetmez: "poter" için hem "potter" hem "peter" makuldür; doğru olanı ancak
    /// adayların sorgudaki diğer kelimelerle **birlikte geçtiği** doğrulanınca seçilir
    /// (bkz. arama yolundaki eş-geçiş doğrulaması).
    pub fn suggest_all(&self, word: &str, k: usize) -> Vec<&str> {
        let w = word.to_lowercase();
        let n = w.chars().count();
        if n < 4 || self.known.contains(&w) {
            return Vec::new();
        }
        let mut scored: Vec<(usize, u32, &str)> = Vec::new();
        let sk = skeleton(&w);
        if sk.chars().count() >= 2 {
            if let Some(cands) = self.skeletons.get(&sk) {
                for (t, f) in cands {
                    if t.chars().count().abs_diff(n) <= 2 {
                        let d = damerau(&w, t, 2);
                        if d <= 2 {
                            scored.push((d, *f, t.as_str()));
                        }
                    }
                }
            }
        }
        let budget = if n >= 6 { 2 } else { 1 };
        if let Some(first) = w.chars().next() {
            for len in n.saturating_sub(budget)..=n + budget {
                if let Some(cands) = self.buckets.get(&(first, len)) {
                    for (t, f) in cands {
                        let d = damerau(&w, t, budget);
                        if d <= budget && !scored.iter().any(|(_, _, s)| *s == t.as_str()) {
                            scored.push((d, *f, t.as_str()));
                        }
                    }
                }
            }
        }
        scored.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        scored.truncate(k);
        scored.into_iter().map(|(_, _, t)| t).collect()
    }

    /// Sorgunun tamamını düzeltir. Düzeltilen kelime varsa yeni metin, yoksa `None`.
    /// Sayılar ve kısa kelimeler olduğu gibi bırakılır ("witchr 3" → "witcher 3").
    pub fn correct_query(&self, query: &str) -> Option<String> {
        let mut changed = false;
        let out: Vec<String> = query
            .split_whitespace()
            .map(|w| match self.suggest(w) {
                Some(s) => {
                    changed = true;
                    s.to_string()
                }
                None => w.to_string(),
            })
            .collect();
        changed.then(|| out.join(" "))
    }

    /// Aday düzeltme **kombinasyonları** (en olasıdan başlayarak, en çok `max` tane).
    /// Her kelime için kendi hâli + en iyi 3 aday denenir; hepsi orijinal olan kombinasyon
    /// atlanır. Çağıran, hangi kombinasyonun korpusta gerçekten karşılığı olduğunu
    /// (ör. FTS eşleşme sayısıyla) doğrular — "hery poter" için "hero peter" 0 eşleşme,
    /// "harry potter" ise gerçek kayıtlar döndürür.
    pub fn candidates(&self, query: &str, max: usize) -> Vec<String> {
        let words: Vec<&str> = query.split_whitespace().collect();
        if words.is_empty() {
            return Vec::new();
        }
        let per_word: Vec<Vec<String>> = words
            .iter()
            .map(|w| {
                let mut v = vec![w.to_string()];
                v.extend(self.suggest_all(w, 6).into_iter().map(str::to_string));
                v
            })
            .collect();
        // Kombinasyonlar **toplam yakınlık** sırasına göre üretilir: her kelimenin aday
        // sırası (0 = kelimenin kendisi, 1 = en yakın aday …) toplanır, küçük toplam önce
        // denenir. Kartezyen sırayla üretmek "harry potter"ı listenin sonuna atıyor ve
        // kesime takılıyordu (ölçüm: "hery poter" düzeltilemiyordu).
        let mut out: Vec<(usize, String)> = vec![(0, String::new())];
        for opts in &per_word {
            let mut next = Vec::with_capacity(out.len() * opts.len());
            for (rank, prefix) in &out {
                for (i, o) in opts.iter().enumerate() {
                    next.push((
                        rank + i,
                        if prefix.is_empty() {
                            o.clone()
                        } else {
                            format!("{prefix} {o}")
                        },
                    ));
                }
            }
            next.sort_by_key(|(r, _)| *r);
            // Kombinasyon patlamasını sınırla: her adımda en iyi `max * 2` dalı tut.
            next.truncate(max * 2);
            out = next;
        }
        out.sort_by_key(|(r, _)| *r);
        let original = words.join(" ");
        let mut res: Vec<String> = out.into_iter().map(|(_, c)| c).collect();
        res.retain(|c| *c != original);
        res.truncate(max);
        res
    }
}

/// Damerau-Levenshtein (bitişik harf yer değişimi dahil), `budget` aşılırsa erken çıkar.
/// Torrent adlarındaki tipik hatalar: harf düşmesi (mtrix), tekrar eksiği (poter),
/// yer değişimi (harry→hrary).
fn damerau(a: &str, b: &str, budget: usize) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > budget {
        return budget + 1;
    }
    let (n, m) = (a.len(), b.len());
    // prev2 = i-2. satır (yer değişimi için), prev = i-1. satır, cur = i. satır.
    let mut prev2: Vec<usize> = vec![0; m + 1];
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur: Vec<usize> = vec![0; m + 1];
    for i in 1..=n {
        cur[0] = i;
        let mut row_min = cur[0];
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(prev2[j - 2] + 1);
            }
            cur[j] = v;
            row_min = row_min.min(v);
        }
        if row_min > budget {
            return budget + 1;
        }
        // Satırları kaydır: prev2 ← prev ← cur (cur, eski prev2'nin tamponunu geri alır).
        let recycled = std::mem::replace(&mut prev2, std::mem::replace(&mut prev, cur));
        cur = recycled;
    }
    prev[m]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx() -> SpellIndex {
        SpellIndex::build(
            [
                ("harry", 12u32),
                ("potter", 12),
                ("matrix", 30),
                ("witcher", 8),
                ("ubuntu", 40),
                ("hurry", 1),
                ("mozart", 5),
                ("resident", 6),
                ("evil", 6),
            ]
            .into_iter()
            .map(|(s, f)| (s.to_string(), f)),
        )
    }

    #[test]
    fn tipik_yazim_hatalari_duzeltilir() {
        let s = idx();
        assert_eq!(s.suggest("hery"), Some("harry")); // harf düşmesi + sık geçen kazanır
        assert_eq!(s.suggest("poter"), Some("potter")); // tekrar eksiği
        assert_eq!(s.suggest("mtrix"), Some("matrix"));
        assert_eq!(s.suggest("witchr"), Some("witcher"));
        assert_eq!(s.suggest("ubunutu"), Some("ubuntu")); // yer değişimi/fazla harf
    }

    #[test]
    fn dogru_kelimeye_ve_alakasiza_dokunmaz() {
        let s = idx();
        assert_eq!(s.suggest("matrix"), None); // zaten sözlükte
        assert_eq!(s.suggest("zeplin"), None); // hiçbir adaya yakın değil
        assert_eq!(s.suggest("abc"), None); // çok kısa
    }

    #[test]
    fn sorgu_duzeltme_sayilari_korur() {
        let s = idx();
        assert_eq!(s.correct_query("hery poter"), Some("harry potter".into()));
        assert_eq!(s.correct_query("witchr 3"), Some("witcher 3".into()));
        assert_eq!(s.correct_query("matrix 1999"), None); // düzeltilecek bir şey yok
    }
}

#[cfg(test)]
mod tests_mtrix {
    use super::*;

    #[test]
    fn tek_kelimelik_hata_aday_uretir() {
        let s = SpellIndex::build(
            [("matrix", 1u32), ("metro", 9), ("mario", 4), ("metal", 7)]
                .into_iter()
                .map(|(w, f)| (w.to_string(), f)),
        );
        assert!(!s.contains("mtrix"));
        let c = s.suggest_all("mtrix", 6);
        println!("adaylar: {c:?}");
        assert!(c.contains(&"matrix"), "matrix aday olmalı, gelen: {c:?}");
        let combos = s.candidates("mtrix", 24);
        println!("kombinasyonlar: {combos:?}");
        assert!(combos.iter().any(|x| x == "matrix"));
    }
}
