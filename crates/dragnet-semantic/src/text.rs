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
