// SPDX-License-Identifier: AGPL-3.0-only
//! Info sözlüğündeki metin alanları (name, path) için kodlama çözümü.
//!
//! BEP-3 UTF-8 ister ama eski/bölgesel torrent'lerde GBK, Shift-JIS, CP1251 vb.
//! görülür; `from_utf8_lossy` bunları `�` yapar. Sıra: geçerli UTF-8 → doğrudan;
//! değilse `chardetng` ile tespit et ve `encoding_rs` ile çöz; hâlâ çözülemeyen
//! baytlar `�` kalır (çağıran `.utf-8` varyantını öncelikle denemelidir).

use serde_bencode::value::Value;
use std::collections::HashMap;

/// Sözlükten `<key>.utf-8` (varsa) yoksa `<key>` alanını okuyup metne çevirir.
pub fn get_text(dict: &HashMap<Vec<u8>, Value>, key: &str) -> Option<String> {
    let utf8_key = format!("{key}.utf-8");
    if let Some(Value::Bytes(b)) = dict.get(utf8_key.as_bytes()) {
        if let Ok(s) = std::str::from_utf8(b) {
            if !s.trim().is_empty() {
                return Some(s.to_string());
            }
        }
    }
    match dict.get(key.as_bytes()) {
        Some(Value::Bytes(b)) => Some(decode_bytes(b)),
        _ => None,
    }
}

/// Ham baytları en makul kodlamayla metne çevirir.
///
/// Kısa dizelerde istatistiksel tespit güvenilmez; bu yüzden `chardetng` tahmini +
/// yaygın eski kodlamalar (GB18030, Shift_JIS, EUC-KR, CP1251, CP1254) denenir ve
/// hatasız çözülen adaylar arasından "harf oranı" en yüksek olan seçilir.
pub fn decode_bytes(b: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(b) {
        return s.to_string();
    }
    let mut det = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
    det.feed(b, true);
    let guess = det.guess(None, chardetng::Utf8Detection::Deny);
    let candidates = [
        guess,
        encoding_rs::GB18030,
        encoding_rs::SHIFT_JIS,
        encoding_rs::EUC_KR,
        encoding_rs::WINDOWS_1251,
        encoding_rs::WINDOWS_1254,
        encoding_rs::WINDOWS_1252,
    ];
    let mut best: Option<(f32, String)> = None;
    for enc in candidates {
        let (cow, _, had_errors) = enc.decode(b);
        if had_errors {
            continue;
        }
        let total = cow.chars().count().max(1) as f32;
        let letters = cow
            .chars()
            .filter(|c| {
                c.is_alphanumeric()
                    || c.is_whitespace()
                    || matches!(c, '.' | '-' | '_' | '[' | ']' | '(' | ')')
            })
            .count() as f32;
        let score = letters / total;
        if best
            .as_ref()
            .map(|(s, _)| score > *s + 1e-6)
            .unwrap_or(true)
        {
            best = Some((score, cow.into_owned()));
        }
    }
    best.map(|(_, s)| s)
        .unwrap_or_else(|| String::from_utf8_lossy(b).into_owned())
}

// NOT: `is_garbled` kaldırıldı (F13 temizliği). Hiç çağıranı yoktu ve "bozuk ad"ın
// İKİNCİ bir tanımını getiriyordu: depo bunu `instr(name, char(65533)) > 0` ile, yani
// "içinde tek bir � bile varsa bozuk" olarak işaretliyor (`torrents.garbled`), buradaki
// eşik ise %25'ti. Tek gerçeklik kaynağı depodaki sütun olsun — yeniden çekim kararı
// zaten oradan veriliyor.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_passthrough_and_legacy_decoding() {
        assert_eq!(
            decode_bytes("Çekirdek – テスト".as_bytes()),
            "Çekirdek – テスト"
        );
        // GBK: 电影 (dianying)
        let gbk = [0xB5u8, 0xE7, 0xD3, 0xB0];
        assert_eq!(decode_bytes(&gbk), "电影");
        // Shift-JIS: gerçekçi uzunlukta bir ad (çok kısa CJK dizeler GBK/SJIS arasında
        // doğası gereği belirsizdir; tespit uzunlukla güvenilir hâle gelir).
        let jp = "日本語のファイル名 アニメ 第01話 [1080p].mp4";
        let (sjis, _, _) = encoding_rs::SHIFT_JIS.encode(jp);
        assert_eq!(decode_bytes(&sjis), jp);
        // CP1251: Привет
        let cp1251 = [0xCFu8, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2];
        assert_eq!(decode_bytes(&cp1251), "Привет");
    }

    #[test]
    fn prefers_utf8_variant_key() {
        let mut d = HashMap::new();
        d.insert(b"name".to_vec(), Value::Bytes(vec![0xB5, 0xE7, 0xD3, 0xB0]));
        d.insert(
            b"name.utf-8".to_vec(),
            Value::Bytes("电影 (utf8)".as_bytes().to_vec()),
        );
        assert_eq!(get_text(&d, "name").as_deref(), Some("电影 (utf8)"));
        d.remove(b"name.utf-8".as_slice());
        assert_eq!(get_text(&d, "name").as_deref(), Some("电影"));
        assert!(get_text(&d, "missing").is_none());
    }

}
