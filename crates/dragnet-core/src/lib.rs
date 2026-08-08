// SPDX-License-Identifier: AGPL-3.0-only
//! dragnet-core — Dragnet bileşenleri arasında paylaşılan çekirdek tipler.
//!
//! Bu crate yalnızca veri tiplerini ve temel doğrulamayı içerir; ağ, disk veya
//! async mantık barındırmaz. Böylece dht/meta/store/api crate'lerinin ortak dili olur.

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
}
