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
    /// Sorgu yalnız kategori niyetinden ibaret ("oyunlar", "filmler", "tüm müzikler").
    /// Bu bir arama değil **gözatma** isteğidir: o kategorideki her şey listelenmeli.
    /// (Kullanıcı geri bildirimi: "oyunlar" sorgusu adında "oyun" geçenleri getiriyordu,
    /// oysa beklenen oyun kategorisinin tamamı.)
    pub category_only: bool,
    /// Sorguda **tanıdık** bir sinyal bulundu mu: TR→EN sözlük eşleşmesi, kategori niyeti
    /// ya da yıl. Tek kelimelik, sözlükte olmayan ve tanınmayan sorgular ("mtrix") büyük
    /// olasılıkla yazım hatasıdır ya da korpusta karşılığı yoktur; arama yolu bunlara
    /// alakasız sonuç döndürmek yerine "bulunamadı" der.
    pub recognized: bool,
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

/// TR→EN karşılıklar (F4). Torrent adları neredeyse tamamen İngilizce/orijinal dilde
/// olduğu için Türkçe sorgu ile ad arasında sözcüksel köprü yoktur; embedding bu boşluğu
/// kısmen kapatır ama ölçümde en zayıf sınıf buydu ("taht oyunları dizisi" → MISS).
/// Burada **çok kelimeli ifadeler** (çeviri başlıklar) önce, sonra tek kelimeler
/// (tür/tema) uygulanır. Türkçe metin İngilizce karşılığıyla **değiştirilir** — hem
/// FTS hem embedding korpusun diline yaklaşır.
const ALIAS_PHRASES: &[(&str, &str)] = &[
    // Çeviri başlıklar (Türkiye'de yaygın kullanılan adlar)
    ("taht oyunları", "game of thrones"),
    ("taht oyunlari", "game of thrones"),
    ("yüzüklerin efendisi", "lord of the rings"),
    ("yuzuklerin efendisi", "lord of the rings"),
    ("yıldız savaşları", "star wars"),
    ("yildiz savaslari", "star wars"),
    ("açlık oyunları", "hunger games"),
    ("aclik oyunlari", "hunger games"),
    ("karayip korsanları", "pirates of the caribbean"),
    ("yürüyen ölüler", "the walking dead"),
    ("yuruyen oluler", "the walking dead"),
    ("uzay yolu", "star trek"),
    ("yıldız geçidi", "stargate"),
    ("kara şövalye", "the dark knight"),
    ("kara sovalye", "the dark knight"),
    ("örümcek adam", "spider-man"),
    ("orumcek adam", "spider-man"),
    ("demir adam", "iron man"),
    ("buz devri", "ice age"),
    ("narnia günlükleri", "chronicles of narnia"),
    ("esaretin bedeli", "the shawshank redemption"),
    ("canavarlar şirketi", "monsters inc"),
    // Kavram → korpustaki örnekler (kullanıcı geri bildirimi: "işletim sistemi" sorgusu
    // adında "windows" geçen her şeyi getiriyordu; model ubuntu'nun bir işletim sistemi
    // olduğunu bilmiyor). Kavramı hem İngilizce karşılığıyla hem yaygın örnekleriyle
    // genişletiyoruz.
    (
        "işletim sistemi",
        "operating system linux ubuntu debian iso",
    ),
    (
        "isletim sistemi",
        "operating system linux ubuntu debian iso",
    ),
    ("linux dağıtımı", "linux distribution ubuntu debian iso"),
    ("ofis programı", "office"),
    ("ofis yazılımı", "office"),
    ("antivirüs", "antivirus security"),
    ("görüntü işleme", "photoshop image editing"),
    ("video düzenleme", "video editing premiere resolve"),
    // Tür/tema
    ("bilim kurgu", "sci-fi science fiction"),
    ("çizgi film", "cartoon animation"),
    ("cizgi film", "cartoon animation"),
    ("süper kahraman", "superhero"),
    ("super kahraman", "superhero"),
    ("hayatta kalma", "survival"),
    ("araba yarışı", "racing"),
    ("araba yarisi", "racing"),
    ("ikinci dünya savaşı", "world war ii"),
];

/// Tek kelimelik TR→EN karşılıklar (tam kelime eşleşmesi; ek almış biçimler için
/// `starts_with` + kısa ek toleransı `alias_of` içinde).
const ALIAS_WORDS: &[(&str, &str)] = &[
    ("büyücü", "wizard"),
    ("buyucu", "wizard"),
    ("sihirbaz", "wizard magician"),
    ("büyü", "magic"),
    ("zombi", "zombie"),
    ("vampir", "vampire"),
    ("korsan", "pirate"),
    ("uzay", "space"),
    ("casus", "spy"),
    ("kahraman", "hero"),
    ("kahramanlar", "heroes"),
    ("ejderha", "dragon"),
    ("korku", "horror"),
    ("gerilim", "thriller"),
    ("komedi", "comedy"),
    ("aksiyon", "action"),
    ("macera", "adventure"),
    ("romantik", "romance"),
    ("polisiye", "crime detective"),
    ("savaş", "war"),
    ("tarihi", "historical"),
    ("gizem", "mystery"),
    ("efsane", "legend"),
    ("strateji", "strategy"),
    ("futbol", "football soccer"),
    ("matriks", "matrix"),
    ("çocuk", "kids children"),
    ("cocuk", "kids children"),
    ("şövalye", "knight"),
    ("prenses", "princess"),
    ("hırsız", "thief"),
    ("katil", "killer"),
    ("dedektif", "detective"),
    ("uzaylı", "alien"),
    ("uzayli", "alien"),
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

/// Bir kelimenin TR→EN karşılığı. Türkçe ekleri tolere eder: kök eşleşirse ve kelime
/// kökten en çok 4 karakter uzunsa ("zombi" ↔ "zombiler", "korku" ↔ "korkulu") karşılık
/// döner. Kısa kökler (< 4 harf) yalnız tam eşleşir ("büyü" ↔ "büyücü" karışmasın).
fn alias_of(word: &str) -> Option<&'static str> {
    ALIAS_WORDS
        .iter()
        .find(|(tr, en)| {
            // Kelime zaten İngilizce biçimse dokunma: "zombies" → "zombie" yapmak FTS
            // eşleşmesini bozar (korpusta "Zombies" geçer). Aynısı vampires/pirates için.
            if word.starts_with(en.split(' ').next().unwrap_or(en)) {
                return false;
            }
            let n = tr.chars().count();
            if n >= 5 {
                word.starts_with(tr) && word.chars().count() <= n + 4
            } else {
                word == *tr
            }
        })
        .map(|(_, en)| *en)
}

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

    // TR→EN karşılıklar: önce çok kelimeli başlıklar/temalar, sonra tek kelimeler.
    for (tr, en) in ALIAS_PHRASES {
        if text.contains(tr) {
            text = text.replace(tr, en);
            plan.recognized = true;
        }
    }
    if ALIAS_WORDS.iter().any(|(tr, _)| text.contains(tr)) {
        text = text
            .split_whitespace()
            .map(|w| match alias_of(w) {
                Some(en) => {
                    plan.recognized = true;
                    en.to_string()
                }
                None => w.to_string(),
            })
            .collect::<Vec<_>>()
            .join(" ");
    }
    plan.recognized |= plan.year.is_some() || plan.year_range.is_some();

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
    // Sorgu **yalnız** kategori kelimelerinden ibaretse ("oyunlar", "tüm filmler"): bu bir
    // arama değil gözatma isteğidir; arama yolu kategori filtresiyle listeleme yapar.
    if !words.is_empty() && words.iter().all(|w| cat_of(w).is_some()) {
        plan.category = words.iter().find_map(|w| cat_of(w));
        plan.category_only = true;
        plan.semantic_text = words.join(" ");
        plan.fts_text = plan.semantic_text.clone();
        return plan;
    }
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
                    plan.recognized = true;
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
        // F4: TR→EN karşılık — "zombi" korpusun dili olan İngilizceye çevrilir.
        let p = understand("zombi konulu oyunları listeler misin");
        assert_eq!(p.semantic_text, "zombie oyunları");
        assert_eq!(p.fts_text, "zombie");
        assert_eq!(p.category, Some("game"));

        let p = understand("İçinde heroes geçen oyunları listeler misin");
        assert_eq!(p.semantic_text, "heroes oyunları");
        assert_eq!(p.fts_text, "heroes");
        assert_eq!(p.category, Some("game"));

        let p = understand("2000'lerin bilim kurgu filmleri");
        assert_eq!(p.semantic_text, "sci-fi science fiction filmleri");
        assert_eq!(p.fts_text, "sci-fi science fiction");
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
        // Tek kelime kategori: gözatma isteği (F4-3) — kategori filtresiyle listelenir.
        let p = understand("oyunlar");
        assert_eq!(p.semantic_text, "oyunlar");
        assert_eq!(p.category, Some("game"));
        assert!(p.category_only);
        // Çoklu kategori kelimesi de gözatmadır ("tüm filmler" → dolgu + kategori).
        let p = understand("tüm filmler");
        assert_eq!(p.category, Some("video"));
        assert!(p.category_only);
        // Kategori + başka kelime → normal arama.
        let p = understand("zombi oyunları");
        assert!(!p.category_only);
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
