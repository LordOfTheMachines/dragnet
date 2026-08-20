// SPDX-License-Identifier: AGPL-3.0-only
//! dragnet-core — Dragnet bileşenleri arasında paylaşılan çekirdek tipler.
//!
//! Bu crate yalnızca veri tiplerini ve temel doğrulamayı içerir; ağ, disk veya
//! async mantık barındırmaz. Böylece dht/meta/store/api crate'lerinin ortak dili olur.

pub mod parse;
pub mod rank;
pub mod spell;

use std::fmt;

use serde::{Deserialize, Serialize};

/// Bir BitTorrent v1 infohash'i (20 bayt / 40 hex karakter).
///
/// Torrent'i evrensel olarak tanımlayan parmak izidir. Magnet linklerinin taşıdığı
/// `urn:btih:` değeri budur.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InfoHash([u8; 20]);

impl InfoHash {
    /// Ham 20 baytlık diziden oluşturur.
    pub const fn from_bytes(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    /// 40 karakterlik hex string'den ayrıştırır (büyük/küçük harf duyarsız).
    ///
    /// Uzunluk 40 değilse ya da hex olmayan karakter varsa `None` döner.
    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 40 {
            return None;
        }
        let mut out = [0u8; 20];
        for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
            let hi = hex_val(chunk[0])?;
            let lo = hex_val(chunk[1])?;
            out[i] = (hi << 4) | lo;
        }
        Some(Self(out))
    }

    /// Ham baytlara erişim.
    pub const fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    /// 40 karakterlik küçük harf hex gösterimi.
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(40);
        for b in self.0 {
            s.push(nibble_to_hex(b >> 4));
            s.push(nibble_to_hex(b & 0x0f));
        }
        s
    }

    /// qBittorrent'in tükettiği magnet linkini üretir.
    pub fn to_magnet(&self, display_name: Option<&str>) -> String {
        match display_name {
            Some(name) => format!(
                "magnet:?xt=urn:btih:{}&dn={}",
                self.to_hex(),
                urlencode(name)
            ),
            None => format!("magnet:?xt=urn:btih:{}", self.to_hex()),
        }
    }
}

impl fmt::Debug for InfoHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "InfoHash({})", self.to_hex())
    }
}

impl fmt::Display for InfoHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// Bir torrent'in metadata'sından çıkarılan tek dosya.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TorrentFile {
    pub path: String,
    pub size: u64,
}

/// İndeksin sakladığı bir torrent kaydı.
///
/// Alanlar `docs/ARCHITECTURE.md` §4'teki veri modeliyle hizalıdır.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TorrentRecord {
    pub infohash: InfoHash,
    pub name: String,
    pub total_size: u64,
    pub files: Vec<TorrentFile>,
    pub first_seen: i64,
    pub last_seen: i64,
    pub seen_count: u64,
}

impl TorrentRecord {
    /// qBittorrent nova3 plugin'inin ilettiği magnet linki.
    pub fn magnet(&self) -> String {
        self.infohash.to_magnet(Some(&self.name))
    }

    /// Bu kaydın içerik kategorisi (isim + dosya uzantılarından tahmin).
    pub fn category(&self) -> &'static str {
        categorize(&self.name, &self.files)
    }
}

/// Bilinen kategori anahtarları (UI etiketleri/filtreleri bunlarla eşlenir).
pub const CATEGORIES: &[&str] = &[
    "video", "audio", "software", "game", "book", "adult", "archive", "other",
];

/// Bir torrent'i adı ve dosya uzantılarından kabaca sınıflandırır.
///
/// Sezgisel ve kusurludur; amaç filtreleme ve gruplamadır (özellikle yetişkin
/// içeriği varsayılan olarak gizleyebilmek). Sıra önemlidir: önce yetişkin.
pub fn categorize(name: &str, files: &[TorrentFile]) -> &'static str {
    let hay = name.to_lowercase();
    let mut exts = std::collections::HashSet::new();
    for f in files {
        let p = f.path.to_lowercase();
        if let Some(dot) = p.rfind('.') {
            let e = &p[dot + 1..];
            if e.len() <= 5 {
                exts.insert(e.to_string());
            }
        }
    }
    let has = |k: &str| hay.contains(k);
    let ext = |e: &str| exts.contains(e);

    // 1) Yetişkin (filtre önceliği).
    const ADULT: &[&str] = &[
        "xxx",
        "porn",
        "p0rn",
        "hentai",
        "futanari",
        "uncensored",
        "brazzers",
        "onlyfans",
        "milf",
        "javhd",
        "1pondo",
        "caribbeancom",
        "nubiles",
        "eronite",
        "erotic",
        "bdsm",
        "creampie",
        "gangbang",
        " sex ",
        "sex.",
        "sex-",
        "18+",
    ];
    const ADULT_CODES: &[&str] = &[
        "miaa-", "miaa ", "ipz-", "sis001", "ssni", "abp-", "pred-", "mide-", "fsdss", "stars-",
        "ofje", "fes-", "juq-", "cawd", "ipx-", "ssis-",
    ];
    if ADULT.iter().any(|k| has(k)) || ADULT_CODES.iter().any(|k| has(k)) {
        return "adult";
    }

    // 2) Oyun.
    const GAME: &[&str] = &[
        "fitgirl",
        "codex",
        "skidrow",
        "reloaded",
        "-plaza",
        "empress",
        "razor1911",
        "goldberg",
        "-tenoke",
        "dodi",
        "repack",
        "ps4",
        "ps3",
        "xbox360",
        "nsw",
        "-flt",
    ];
    if GAME.iter().any(|k| has(k)) {
        return "game";
    }

    // 3) Yazılım.
    const SOFT: &[&str] = &[
        "crack",
        "keygen",
        "activator",
        "x64",
        "x86",
        "win64",
        "office",
        "adobe",
        "autocad",
        "photoshop",
        "windows 1",
        "macos",
        "activated",
    ];
    if ext("exe") || ext("msi") || ext("dmg") || ext("apk") || SOFT.iter().any(|k| has(k)) {
        return "software";
    }

    // 4) Kitap.
    if ext("pdf")
        || ext("epub")
        || ext("mobi")
        || ext("azw3")
        || ext("cbz")
        || ext("cbr")
        || ext("djvu")
        || has("ebook")
        || has("audiobook")
    {
        return "book";
    }

    // 5) Ses.
    if ext("mp3")
        || ext("flac")
        || ext("wav")
        || ext("aac")
        || ext("m4a")
        || ext("ogg")
        || ext("opus")
        || ext("wma")
        || ext("alac")
        || ext("ape")
        || has("discography")
        || has("[flac]")
        || has("320kbps")
    {
        return "audio";
    }

    // 6) Video.
    const VID: &[&str] = &[
        "1080p", "720p", "2160p", "480p", "x264", "x265", "hevc", "bluray", "brrip", "webrip",
        "web-dl", "web.dl", "hdtv", "dvdrip", "xvid", "bdrip", "4k", "hdrip",
    ];
    if ext("mkv")
        || ext("mp4")
        || ext("avi")
        || ext("mov")
        || ext("wmv")
        || ext("m4v")
        || ext("webm")
        || ext("mpg")
        || ext("mpeg")
        || ext("m2ts")
        || ext("ts")
        || ext("vob")
        || ext("flv")
        || ext("ogv")
        || ext("3gp")
        || ext("rmvb")
        || VID.iter().any(|k| has(k))
        || has_episode(&hay)
    {
        return "video";
    }

    // 7) ISO → çoğunlukla yazılım/OS.
    if ext("iso") {
        return "software";
    }

    // 8) Arşiv.
    if ext("zip") || ext("rar") || ext("7z") || ext("tar") || ext("gz") {
        return "archive";
    }

    "other"
}

/// `sNNeNN` (dizi bölüm) kalıbı içeriyor mu? (kaba tarama)
fn has_episode(s: &str) -> bool {
    let b = s.as_bytes();
    let n = b.len();
    let mut i = 0;
    while i + 1 < n {
        if b[i] == b's' && b[i + 1].is_ascii_digit() {
            // s<rakam(lar)>e<rakam>
            let mut j = i + 2;
            while j < n && b[j].is_ascii_digit() {
                j += 1;
            }
            if j + 1 < n && b[j] == b'e' && b[j + 1].is_ascii_digit() {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// İç içe bencode kaplarında azami özyineleme derinliği. Kötü niyetli uzak
/// düğüm/peer'in derin iç içe `d`/`l` göndererek stack overflow (→ süreç abort)
/// tetiklemesini önler. serde_bencode'un kendi derinlik sınırı YOKTUR, bu yüzden
/// güvenilmeyen bencode `serde_bencode::from_bytes`'a verilmeden ÖNCE bununla
/// doğrulanmalıdır.
pub const MAX_BENCODE_DEPTH: usize = 100;

/// `b[0..]` konumundaki ilk tam bencode değerinin bayt uzunluğunu döner.
/// Derinlik sınırlı ve string uzunlukları sınır-kontrollüdür (güvenilmeyen veri).
/// `None` = bozuk, çok derin ya da sınır dışı → çağıran veriyi reddetmeli.
pub fn bencode_value_len(b: &[u8]) -> Option<usize> {
    fn scan(b: &[u8], i: usize, depth: usize) -> Option<usize> {
        if depth > MAX_BENCODE_DEPTH {
            return None;
        }
        match b.get(i)? {
            b'i' => {
                let mut j = i + 1;
                while *b.get(j)? != b'e' {
                    j += 1;
                }
                Some(j + 1)
            }
            b'l' | b'd' => {
                let mut j = i + 1;
                loop {
                    if *b.get(j)? == b'e' {
                        return Some(j + 1);
                    }
                    j = scan(b, j, depth + 1)?;
                }
            }
            b'0'..=b'9' => {
                let mut j = i;
                let mut len = 0usize;
                while b.get(j)?.is_ascii_digit() {
                    len = len.checked_mul(10)?.checked_add((b[j] - b'0') as usize)?;
                    j += 1;
                }
                if *b.get(j)? != b':' {
                    return None;
                }
                j += 1;
                let end = j.checked_add(len)?;
                if end > b.len() {
                    return None;
                }
                Some(end)
            }
            _ => None,
        }
    }
    scan(b, 0, 0)
}

/// Bir bencode tamponunun `serde_bencode`'a güvenle verilebilecek kadar sığ ve
/// sınır-içi olup olmadığını kontrol eder (derinlik/uzunluk saldırılarına karşı).
pub fn bencode_is_safe(b: &[u8]) -> bool {
    bencode_value_len(b).is_some()
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn nibble_to_hex(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'a' + (n - 10)) as char,
        _ => unreachable!("nibble > 15"),
    }
}

/// Magnet `dn` alanı için minimal yüzde-kodlama (harf/rakam dışını kodlar).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let hex = "0123456789abcdef0123456789abcdef01234567";
        let ih = InfoHash::from_hex(hex).expect("geçerli hex");
        assert_eq!(ih.to_hex(), hex);
    }

    #[test]
    fn from_hex_rejects_bad_input() {
        assert!(InfoHash::from_hex("tooshort").is_none());
        assert!(InfoHash::from_hex("zz23456789abcdef0123456789abcdef01234567").is_none());
    }

    #[test]
    fn magnet_contains_infohash_and_name() {
        let ih = InfoHash::from_hex("0123456789abcdef0123456789abcdef01234567").unwrap();
        let magnet = ih.to_magnet(Some("Ubuntu 24.04"));
        assert!(magnet.contains("urn:btih:0123456789abcdef0123456789abcdef01234567"));
        assert!(magnet.contains("dn=Ubuntu%2024.04"));
    }

    fn f(path: &str, size: u64) -> TorrentFile {
        TorrentFile {
            path: path.into(),
            size,
        }
    }

    #[test]
    fn categorize_basics() {
        assert_eq!(
            categorize("Ubuntu 24.04 desktop amd64", &[f("ubuntu.iso", 1)]),
            "software"
        );
        assert_eq!(
            categorize("The Flash S04E01 1080p WEB-DL", &[f("a.mkv", 1)]),
            "video"
        );
        assert_eq!(categorize("Some Movie", &[f("movie.mkv", 1)]), "video");
        assert_eq!(
            categorize("Pink Floyd Discography FLAC", &[f("a.flac", 1)]),
            "audio"
        );
        assert_eq!(categorize("Rust Book", &[f("book.epub", 1)]), "book");
        assert_eq!(
            categorize("Cyberpunk 2077 FitGirl Repack", &[f("setup.exe", 1)]),
            "game"
        );
        assert_eq!(categorize("MIAA-462 something", &[f("a.mp4", 1)]), "adult");
        assert_eq!(categorize("random stuff", &[f("data.bin", 1)]), "other");
        assert_eq!(categorize("archive pack", &[f("x.rar", 1)]), "archive");
    }

    #[test]
    fn episode_detection() {
        assert!(super::has_episode("show s04e01 1080p"));
        assert!(super::has_episode("s1e2"));
        assert!(!super::has_episode("season four"));
    }
}
