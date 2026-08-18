// SPDX-License-Identifier: AGPL-3.0-only
//! Sorgu anlama: doğal dil sorgudan (TR/EN) niyet çıkarımı.
//!
//! "zombi konulu oyunları listeler misin" → semantik metin `zombi`, FTS metni `zombi`,
//! kategori tercihi `game`. "2000'lerin bilim kurgu filmleri" → yıl aralığı 2000–2009,
//! kategori `video`, metin `bilim kurgu`. Konuşma dolgusu ("listeler misin", "bana",
//! "içinde … geçen") embedding'i ve FTS'i bozduğu için atılır. Kural tabanlı ve ucuzdur;
//! bir LLM değildir — amaç en sık kalıpları yakalamak, gerisini modele bırakmak.

/// Anlaşılmış sorgu.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QueryPlan {
    /// Embedding'e verilecek metin (dolgu ve niyet kelimeleri temizlenmiş).
    pub semantic_text: String,
    /// FTS'e verilecek anahtar kelimeler (aynı temizlik; boşsa FTS atlanır).
    pub fts_text: String,
    /// Sorgudan çıkarılan kategori tercihi (`game`, `video`, `audio`, `book`, `software`).
    /// Yumuşak sinyaldir: filtre değil, sıralamada artırma (kategori sezgiseli kusurlu).
    pub category: Option<&'static str>,
    /// "2000'lerin", "90'ların", "2010s" → kapsayıcı yıl aralığı.
    pub year_range: Option<(u16, u16)>,
    /// Sorgudaki açık yıl (ör. "matrix 1999").
    pub year: Option<u16>,
}

/// Dolgu kelimeleri/ifadeler (küçük harf, Türkçe ekleriyle). Çok kelimeli olanlar önce.
const FILLER_PHRASES: &[&str] = &[
    "listeler misin",
    "listeleyebilir misin",
    "gösterir misin",
    "gösterebilir misin",
    "bulur musun",
    "bulabilir misin",
    "getirir misin",
    "arar mısın",
    "var mı",
    "var mi",
    "içinde",
    "icinde",
    "geçen",
    "gecen",
    "ile ilgili",
    "ilgili",
    "hakkında",
    "hakkinda",
    "konulu",
    "temalı",
    "temali",
    "tarzı",
    "tarzi",
    "gibi",
    "olan",
    "olanlar",
    "olanları",
    "bana",
    "lütfen",
    "lutfen",
    "bir",
    "birkaç",
    "tüm",
    "tum",
    "bütün",
    "butun",
    "en iyi",
    "show me",
    "list me",
    "list",
    "find me",
    "find",
    "search for",
    "search",
    "give me",
    "please",
    "can you",
    "could you",
    "i want",
    "i need",
    "looking for",
    "about",
    "containing",
    "with",
    "the",
    "some",
    "all",
    "best",
    "of",
    "for",
    "me",
    "a",
    "an",
    "any",
];

/// Kategori niyeti: kelime → kategori. Kelime kökleri Türkçe ekleriyle (oyun, oyunlar,
/// oyunları, oyunu…) `starts_with` ile eşlenir; İngilizce tam kelime.
const CATEGORY_HINTS: &[(&str, &str)] = &[
    ("oyun", "game"),
    ("game", "game"),
    ("games", "game"),
    ("rpg", "game"),
    ("film", "video"),
    ("filmler", "video"),
    ("movie", "video"),
    ("movies", "video"),
    ("sinema", "video"),
    ("dizi", "video"),
    ("diziler", "video"),
    ("series", "video"),
    ("belgesel", "video"),
    ("documentary", "video"),
    ("anime", "video"),
    ("animasyon", "video"),
    ("müzik", "audio"),
    ("muzik", "audio"),
    ("music", "audio"),
    ("şarkı", "audio"),
    ("sarki", "audio"),
    ("albüm", "audio"),
    ("album", "audio"),
    ("song", "audio"),
    ("songs", "audio"),
    ("kitap", "book"),
    ("kitab", "book"),
    ("book", "book"),
    ("books", "book"),
    ("ebook", "book"),
    ("roman", "book"),
    ("dergi", "book"),
    ("magazine", "book"),
    ("yazılım", "software"),
    ("yazilim", "software"),
    ("program", "software"),
    ("software", "software"),
    ("uygulama", "software"),
    ("app", "software"),
    ("apps", "software"),
];

/// Kategori kelimesi anlam taşıyan (silinmemesi gereken) durumlar: örn. "game" başlıkta
/// olabilir ("Game of Thrones", "The Art of Game Design"). Kural: kategori kelimesi
/// sorgunun SON kelimesiyse ya da tek başına niyet gibi duruyorsa (2+ kelimeli sorguda)
/// çıkarılır; sorgu yalnız o kelimeden ibaretse metinde kalır.
pub fn understand(query: &str) -> QueryPlan {
    let mut plan = QueryPlan::default();
    // Türkçe: `İ`.to_lowercase() = "i{307}" (birleşik nokta) → önce düz `i` yap.
    let mut q = query.trim().replace(['İ'], "i").to_lowercase();
    if q.is_empty() {
        return plan;
    }
    // Noktalama → boşluk (kesme işareti hariç; "2000'lerin" için ayrı işlenir).
    q = q
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '\'' || c == '’' || c.is_whitespace() {
                c
            } else {
                ' '
            }
        })
        .collect();

    // Yıl aralığı: "2000'lerin", "2000ler", "90'ların", "1990s", "2010s", "80s".
    let mut kept: Vec<String> = Vec::new();
    for tok in q.split_whitespace() {
        let base = tok.trim_matches(|c| c == '\'' || c == '’');
        // 4 haneli + ek/`s`
        let digits: String = base.chars().take_while(|c| c.is_ascii_digit()).collect();
        let rest = &base[digits.len()..];
        let is_decade_suffix = matches!(
            rest.trim_start_matches(['\'', '’']),
            "ler"
                | "lar"
                | "lerin"
                | "ların"
                | "larin"
                | "lerde"
                | "larda"
                | "s"
                | "ler'in"
                | "lı"
                | "li"
        ) || (rest.starts_with('\'') && !rest.is_empty());
        if !digits.is_empty() && !rest.is_empty() && is_decade_suffix {
            let n: u32 = digits.parse().unwrap_or(0);
            let start = match digits.len() {
                4 => n,
                2 => {
                    if n >= 30 {
                        1900 + n
                    } else {
                        2000 + n
                    }
                }
                _ => 0,
            };
            if (1900..=2030).contains(&start) && start.is_multiple_of(10) {
                plan.year_range = Some((start as u16, (start + 9) as u16));
                continue;
            }
        }
        if digits.len() == 4 && rest.is_empty() {
            if let Ok(y) = digits.parse::<u16>() {
                if (1900..=2039).contains(&y) {
                    plan.year = Some(y);
                    kept.push(tok.to_string());
                    continue;
                }
            }
        }
        kept.push(base.to_string());
    }
    let mut text = kept.join(" ");

    // Çok kelimeli dolgu ifadeleri.
    for ph in FILLER_PHRASES.iter().filter(|p| p.contains(' ')) {
        if text.contains(ph) {
            text = text.replace(ph, " ");
        }
    }
    // Tek kelimeli dolgular + kategori niyeti. Kategori kelimesi yalnız **son anlamlı
    // kelimeyse** ("zombi oyunları", "bilim kurgu filmleri", "zombie games") ya da İngilizce
    // çoğul olarak **ilk** kelimeyse ("games about zombies") niyet sayılır; ortadaysa başlığın
    // parçasıdır ("game of thrones", "the art of game design").
    let words: Vec<&str> = text
        .split_whitespace()
        .filter(|w| !FILLER_PHRASES.contains(w))
        .collect();
    let n = words.len();
    let cat_of = |w: &str| -> Option<&'static str> {
        CATEGORY_HINTS
            .iter()
            .find(|(k, _)| {
                if k.chars().count() >= 4 {
                    w.starts_with(k) && w.chars().count() <= k.chars().count() + 4
                } else {
                    w == *k
                }
            })
            .map(|(_, c)| *c)
    };
    // Kategori kelimesi SEMANTİK metinde KALIR (dokümanlar "— game/movie…" içerdiğinden
    // hizalamaya yardım eder; ölçüm: "harry potter filmi" > "harry potter"), FTS metninden
    // çıkar (adlarda "oyunları" geçmez, gürültü olur).
    let mut sem_words: Vec<&str> = Vec::with_capacity(n);
    let mut fts_words: Vec<&str> = Vec::with_capacity(n);
    for (i, w) in words.iter().enumerate() {
        let mut is_cat = false;
        if plan.category.is_none() && n >= 2 {
            let is_last = i == n - 1;
            let is_first_plural_en = i == 0 && w.ends_with('s') && w.is_ascii();
            if is_last || is_first_plural_en {
                if let Some(cat) = cat_of(w) {
                    plan.category = Some(cat);
                    is_cat = true;
                }
            }
        }
        sem_words.push(w);
        if !is_cat {
            fts_words.push(w);
        }
    }
    let sem_text = sem_words.join(" ").trim().to_string();
    let fts_text = fts_words.join(" ").trim().to_string();
    // Her şey dolguysa — orijinali kullan.
    plan.semantic_text = if sem_text.is_empty() {
        query.trim().to_lowercase()
    } else {
        sem_text
    };
    plan.fts_text = if fts_text.is_empty() {
        plan.semantic_text.clone()
    } else {
        fts_text
    };
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_intent_and_strips_filler() {
        let p = understand("zombi konulu oyunları listeler misin");
        assert_eq!(p.semantic_text, "zombi oyunları");
        assert_eq!(p.fts_text, "zombi");
        assert_eq!(p.category, Some("game"));

        let p = understand("İçinde heroes geçen oyunları listeler misin");
        assert_eq!(p.semantic_text, "heroes oyunları");
        assert_eq!(p.fts_text, "heroes");
        assert_eq!(p.category, Some("game"));

        let p = understand("2000'lerin bilim kurgu filmleri");
        assert_eq!(p.semantic_text, "bilim kurgu filmleri");
        assert_eq!(p.fts_text, "bilim kurgu");
        assert_eq!(p.category, Some("video"));
        assert_eq!(p.year_range, Some((2000, 2009)));

        let p = understand("90'ların rock albümleri");
        assert_eq!(p.year_range, Some((1990, 1999)));
        assert_eq!(p.category, Some("audio"));
        assert_eq!(p.fts_text, "rock");

        let p = understand("matrix 1999");
        assert_eq!(p.year, Some(1999));
        assert_eq!(p.semantic_text, "matrix 1999");
        assert_eq!(p.category, None);
    }

    #[test]
    fn keeps_meaningful_words() {
        // Tek kelime: kategori kelimesi metinde kalır.
        let p = understand("oyunlar");
        assert_eq!(p.semantic_text, "oyunlar");
        assert_eq!(p.category, None);
        // Ortadaki kategori kelimesi başlığın parçasıdır.
        let p = understand("game of thrones");
        assert_eq!(p.semantic_text, "game thrones");
        assert_eq!(p.category, None);
        let p = understand("games about zombies");
        assert_eq!(p.fts_text, "zombies");
        assert_eq!(p.category, Some("game"));
        let p = understand("show me some ubuntu iso");
        assert_eq!(p.semantic_text, "ubuntu iso");
        let p = understand("   ");
        assert_eq!(p.semantic_text, "");
    }
}
