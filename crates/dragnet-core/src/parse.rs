// SPDX-License-Identifier: AGPL-3.0-only
//! Torrent (scene) adı ayrıştırıcı: "The.Matrix.1999.1080p.BluRay.x264-GRP" →
//! başlık, yıl, sezon/bölüm, çözünürlük, etiketler, grup.
//!
//! Amaç arama kalitesi: sözcüksel/semantik arama **temiz başlık** üzerinde daha isabetli,
//! yıl/çözünürlük yapısal sinyal olarak kullanılabilir ("2000'lerin filmleri"). Sezgisel ve
//! kusurludur; regex bağımlılığı olmadan, tek geçişte çalışır (500k ad için hızlı olmalı).

/// Ayrıştırılmış ad.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedName {
    /// İlk yapısal işarete (yıl / çözünürlük / sezon) kadar olan, temizlenmiş başlık.
    pub title: String,
    pub year: Option<u16>,
    pub season: Option<u16>,
    pub episode: Option<u16>,
    /// 480/720/1080/2160 (piksel yüksekliği); "4K" → 2160.
    pub resolution: Option<u16>,
    /// Tanınan teknik etiketler, küçük harf (bluray, web-dl, x265, dual, turkish…).
    pub tags: Vec<String>,
    /// Sondaki `-GRP` yayın grubu.
    pub group: Option<String>,
}

const TAGS: &[&str] = &[
    "bluray",
    "blu-ray",
    "bdrip",
    "brrip",
    "web-dl",
    "webdl",
    "webrip",
    "web",
    "hdrip",
    "dvdrip",
    "dvd",
    "hdtv",
    "cam",
    "hdcam",
    "ts",
    "telesync",
    "remux",
    "x264",
    "x265",
    "h264",
    "h265",
    "hevc",
    "avc",
    "av1",
    "xvid",
    "divx",
    "aac",
    "ac3",
    "dts",
    "dd5",
    "ddp5",
    "atmos",
    "truehd",
    "flac",
    "mp3",
    "320kbps",
    "24bit",
    "hdr",
    "hdr10",
    "dv",
    "sdr",
    "10bit",
    "8bit",
    "imax",
    "extended",
    "remastered",
    "repack",
    "proper",
    "uncut",
    "unrated",
    "directors",
    "complete",
    "dual",
    "multi",
    "multisub",
    "sub",
    "subs",
    "dubbed",
    "dublaj",
    "turkish",
    "tr",
    "rus",
    "eng",
    "ita",
    "fre",
    "ger",
    "jpn",
    "kor",
    "chs",
    "cht",
    "internal",
    "limited",
    "retail",
    "iso",
    "mkv",
    "mp4",
    "avi",
    "epub",
    "pdf",
    "mobi",
    "zip",
    "rar",
    "7z",
    "exe",
    "apk",
    "gog",
    "rip",
    "portable",
    "cracked",
    "update",
    "dlc",
    "edition",
    "collection",
    "trilogy",
    "season",
    "series",
    "vol",
    "part",
];

/// Adı ayrıştırır (asla hata vermez; anlaşılmazsa başlık = normalize edilmiş ad).
pub fn parse_name(name: &str) -> ParsedName {
    // Grup: sondaki "-GRP" (boşluksuz, ≤ 20 karakter, uzantı değil).
    let mut work = name.trim().to_string();
    let mut group = None;
    if let Some(pos) = work.rfind('-') {
        let tail = work[pos + 1..].trim();
        let ok = !tail.is_empty()
            && tail.len() <= 20
            && !tail.contains(' ')
            && !tail.contains('.')
            && tail.chars().all(|c| c.is_alphanumeric() || c == '_')
            && !tail.chars().all(|c| c.is_ascii_digit());
        if ok && pos > 0 {
            group = Some(tail.to_string());
            work.truncate(pos);
        }
    }
    // Ayraçları boşluğa çevir; köşeli/normal parantez içi grupları koru ama sınır say.
    let cleaned: String = work
        .chars()
        .map(|c| match c {
            '.' | '_' | '[' | ']' | '(' | ')' | '{' | '}' | ',' | ';' | '|' | '~' => ' ',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let tokens: Vec<&str> = cleaned.split_whitespace().collect();

    let mut out = ParsedName {
        group,
        ..Default::default()
    };
    let mut title_end = tokens.len();
    for (i, tok) in tokens.iter().enumerate() {
        let low = tok.to_lowercase();
        // Yıl: 1900–2039, tek başına bir token (başlığın ilk token'ı değilse).
        if out.year.is_none() && i > 0 && low.len() == 4 {
            if let Ok(y) = low.parse::<u16>() {
                if (1900..=2039).contains(&y) {
                    out.year = Some(y);
                    title_end = title_end.min(i);
                    continue;
                }
            }
        }
        // Çözünürlük: 480p/720p/1080p/1080i/2160p, 4k.
        if out.resolution.is_none() {
            let r = match low.as_str() {
                "480p" => Some(480),
                "576p" => Some(576),
                "720p" => Some(720),
                "1080p" | "1080i" => Some(1080),
                "1440p" => Some(1440),
                "2160p" | "4k" | "uhd" => Some(2160),
                _ => None,
            };
            if let Some(r) = r {
                out.resolution = Some(r);
                title_end = title_end.min(i);
                continue;
            }
        }
        // Sezon/bölüm: S01E02, S01, s1e2, 1x02.
        if out.season.is_none() {
            if let Some((s, e)) = parse_season_episode(&low) {
                out.season = Some(s);
                out.episode = e;
                title_end = title_end.min(i);
                continue;
            }
        }
        if TAGS.contains(&low.as_str()) {
            if !out.tags.contains(&low) {
                out.tags.push(low);
            }
            // Etiketten önceki kısım başlık; ama başlık henüz boşsa (ad etiketle başlıyor) atla.
            if i > 0 {
                title_end = title_end.min(i);
            }
        }
    }
    let title: String = tokens[..title_end.min(tokens.len())].join(" ");
    let title = title.trim().trim_matches('-').trim().to_string();
    out.title = if title.is_empty() {
        cleaned
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .trim_matches('-')
            .trim()
            .to_string()
    } else {
        title
    };
    out
}

fn parse_season_episode(tok: &str) -> Option<(u16, Option<u16>)> {
    let b = tok.as_bytes();
    if b.len() >= 3 && (b[0] == b's') && b[1].is_ascii_digit() {
        // sNN[eNN]
        let mut i = 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let s: u16 = tok[1..i].parse().ok()?;
        if i == b.len() {
            return Some((s, None));
        }
        if b[i] == b'e' {
            let j = i + 1;
            let mut k = j;
            while k < b.len() && b[k].is_ascii_digit() {
                k += 1;
            }
            if k > j && k == b.len() {
                let e: u16 = tok[j..k].parse().ok()?;
                return Some((s, Some(e)));
            }
        }
        return None;
    }
    // NxNN
    if let Some(x) = tok.find('x') {
        let (a, bb) = (&tok[..x], &tok[x + 1..]);
        if !a.is_empty() && a.len() <= 2 && bb.len() == 2 {
            if let (Ok(s), Ok(e)) = (a.parse::<u16>(), bb.parse::<u16>()) {
                return Some((s, Some(e)));
            }
        }
    }
    None
}

/// Addan yıl (varsa) — hızlı yardımcı.
pub fn year_of(name: &str) -> Option<u16> {
    parse_name(name).year
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scene_movie() {
        let p = parse_name("The.Matrix.Reloaded.2003.1080p.BluRay.x264-GROUP");
        assert_eq!(p.title, "The Matrix Reloaded");
        assert_eq!(p.year, Some(2003));
        assert_eq!(p.resolution, Some(1080));
        assert!(p.tags.contains(&"bluray".to_string()) && p.tags.contains(&"x264".to_string()));
        assert_eq!(p.group.as_deref(), Some("GROUP"));
    }

    #[test]
    fn parses_series_and_misc() {
        let p = parse_name("Breaking.Bad.S01E03.720p.HDTV.x264-CTU");
        assert_eq!(p.title, "Breaking Bad");
        assert_eq!((p.season, p.episode), (Some(1), Some(3)));
        let p = parse_name("Friends 1x05 [1080p]");
        assert_eq!((p.season, p.episode), (Some(1), Some(5)));
        let p = parse_name("Heroes of Might & Magic III - HD Edition [RePack]");
        assert_eq!(p.title, "Heroes of Might & Magic III - HD");
        assert!(p.year.is_none());
        let p = parse_name("ubuntu-24.04.2-desktop-amd64.iso");
        assert!(p.title.starts_with("ubuntu"));
        // Yıl başlığın ilk token'ı olamaz ("2001 A Space Odyssey" → başlık korunur).
        let p = parse_name("2001.A.Space.Odyssey.1968.1080p");
        assert_eq!(p.title, "2001 A Space Odyssey");
        assert_eq!(p.year, Some(1968));
        // Boş/garip girdi çökmez.
        assert_eq!(parse_name("").title, "");
        assert_eq!(parse_name("-").title, "");
    }
}
