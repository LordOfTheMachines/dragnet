// SPDX-License-Identifier: AGPL-3.0-only
//! Torrent adı → embed edilecek metin normalizasyonu.

/// Scene-tarzı adları ("The.Matrix.1999.1080p_x264-GRP") modelin daha iyi anlayacağı
/// boşluklu metne çevirir: `.` `_` → boşluk, ardışık boşluklar tekilleştirilir. Yıl,
/// çözünürlük ve codec token'ları **korunur** — bake-off'ta sorgu sinyali taşıdılar
/// ("2000'lerin filmleri" ↔ "2003"). Boş/anlamsız ad boş dize döner.
pub fn normalize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_space = true;
    for ch in name.chars() {
        let c = match ch {
            '.' | '_' | '[' | ']' | '(' | ')' | '{' | '}' => ' ',
            c if c.is_control() => ' ',
            c => c,
        };
        if c == ' ' {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(c);
            last_space = false;
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dots_and_underscores_become_spaces() {
        assert_eq!(
            normalize_name("The.Matrix.Reloaded.2003.1080p_x264-GRP"),
            "The Matrix Reloaded 2003 1080p x264-GRP"
        );
        assert_eq!(
            normalize_name("[Group] Show - 01 (1080p).mkv"),
            "Group Show - 01 1080p mkv"
        );
        assert_eq!(normalize_name("..."), "");
        assert_eq!(
            normalize_name("ubuntu-24.04.2-desktop-amd64.iso"),
            "ubuntu-24 04 2-desktop-amd64 iso"
        );
    }
}

/// Embed edilecek doküman metni: **temiz başlık + yapısal ipuçları + kategori**.
/// Kategori kelimesi ("game", "movie"…) modele "bu bir oyun" sinyali verir; böylece
/// "zombi oyunları" sorgusu zombi filmlerine değil oyunlara yaklaşır (Faz E gözlemi).
/// Yıl/sezon korunur (dönem sorguları), teknik etiketler (x264, 1080p…) atılır — bunlar
/// anlam taşımaz ve embedding'i gürültüler.
pub fn doc_text(name: &str, category: &str) -> String {
    let p = dragnet_core::parse::parse_name(name);
    let mut s = String::with_capacity(name.len() + 24);
    s.push_str(&normalize_name(&p.title));
    if let Some(y) = p.year {
        s.push(' ');
        s.push_str(&y.to_string());
    }
    if let Some(se) = p.season {
        s.push_str(&format!(" season {se}"));
    }
    let cat_word = match category {
        "video" => "movie video",
        "audio" => "music audio",
        "game" => "game",
        "software" => "software application",
        "book" => "book ebook",
        "adult" => "adult",
        "archive" => "archive",
        _ => "",
    };
    if !cat_word.is_empty() {
        s.push_str(" — ");
        s.push_str(cat_word);
    }
    // Başlık boşsa (tamamen etiket/çöp) ham normalize adı kullan.
    if p.title.trim().is_empty() {
        s = normalize_name(name);
    }
    s
}

/// Doküman metni + **dosya adları** (F8-1). Adı anlamsız olan torrent'ler ("s01",
/// "07-coyotes") ancak içeriğinden anlaşılır; dosya adları hem konuyu hem türü ele verir.
/// Yalnız dosya **adı** alınır (dizin yolu değil: tekrar eden klasör adları gürültüdür),
/// tekrarlar ve uzantı-etiket çöpü normalize edilir, toplam karakter sınırlanır —
/// embedding modelinin bağlam penceresi kısa (Gemma 2048 token) ve uzun listeler
/// başlığın sinyalini boğar.
pub fn doc_text_with_files(
    name: &str,
    category: &str,
    files: &[String],
    max_chars: usize,
) -> String {
    let mut s = doc_text(name, category);
    if files.is_empty() {
        return s;
    }
    let mut seen = std::collections::HashSet::new();
    let mut extra = String::new();
    for f in files {
        let base = f.rsplit(['/', '\\']).next().unwrap_or(f);
        let norm = normalize_name(base);
        let norm = norm.trim();
        // Ad zaten dosya adını içeriyorsa (tek dosyalı torrent) tekrar etme.
        if norm.is_empty() || s.contains(norm) {
            continue;
        }
        // Bölüm/parça numarası dışında aynı olan dosyalar tek kez yazılır: bir dizinin
        // 40 bölümü ("… S01E02 mkv", "… S01E03 mkv") 400 karakterlik bütçeyi tekrarla
        // doldurup gerçek bilgiye (altyazı dili, ek içerik, kurulum dosyası) yer
        // bırakmıyordu. İmza: rakamlar atılmış küçük harfli biçim.
        let sig: String = norm
            .chars()
            .filter(|c| !c.is_ascii_digit())
            .flat_map(|c| c.to_lowercase())
            .collect();
        if !seen.insert(sig) {
            continue;
        }
        if extra.len() + norm.len() + 1 > max_chars {
            break;
        }
        extra.push(' ');
        extra.push_str(norm);
    }
    if !extra.is_empty() {
        s.push_str(" — files:");
        s.push_str(&extra);
    }
    s
}

#[cfg(test)]
mod doc_tests {
    use super::*;

    #[test]
    fn doc_text_keeps_title_year_and_category() {
        let d = doc_text("The.Matrix.Reloaded.2003.1080p.BluRay.x264-GROUP", "video");
        assert_eq!(d, "The Matrix Reloaded 2003 — movie video");
        let d = doc_text("Plants_vs_Zombies_Hybrid_3.0.zip", "archive");
        assert!(d.starts_with("Plants vs Zombies Hybrid 3 0"), "{d}");
        assert!(d.ends_with("— archive"));
        let d = doc_text("Heroes of Might & Magic III - HD Edition [RePack]", "game");
        assert!(
            d.contains("Heroes of Might & Magic III") && d.ends_with("— game"),
            "{d}"
        );
    }
}

#[cfg(test)]
mod file_doc_tests {
    use super::*;

    fn f(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn dizi_bolumleri_tek_kez_yazilir() {
        let files = f(&[
            "The Pact.S01/The.Pact.S01E01.avi",
            "The Pact.S01/The.Pact.S01E02.avi",
            "The Pact.S01/The.Pact.S01E03.avi",
            "The Pact.S01/Subs/Turkish.srt",
        ]);
        let d = doc_text_with_files("The Pact.S01.400p.Novamedia", "video", &files, 400);
        // Bölümler tek imzaya indirgenir, altyazı bilgisi yer bulur.
        assert_eq!(d.matches("The Pact S01E").count(), 1, "{d}");
        assert!(d.contains("Turkish srt"), "{d}");
    }

    #[test]
    fn bilgi_tasiyan_dosyalar_korunur() {
        let files = f(&[
            "setup/Photoshop_2024_installer.exe",
            "setup/crack/keygen.exe",
            "readme.txt",
        ]);
        let d = doc_text_with_files("07-coyotes", "other", &files, 400);
        assert!(d.contains("Photoshop 2024 installer exe"), "{d}");
        assert!(d.contains("keygen exe"), "{d}");
    }

    #[test]
    fn butce_asilinca_durur() {
        let files: Vec<String> = (0..200)
            .map(|i| format!("Uniq{}Name{}File.mkv", i, i * 7))
            .collect();
        let d = doc_text_with_files("Karisik", "video", &files, 120);
        assert!(d.len() < 260, "bütçe aşıldı: {}", d.len());
    }
}
