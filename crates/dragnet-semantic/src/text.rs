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
