// SPDX-License-Identifier: AGPL-3.0-only
//! Minimal BitTorrent peer-wire protokolü: tek bir peer'den torrent metadata'sını
//! **tracker'sız** çeker.
//!
//! Uygulanan BEP'ler:
//! - **BEP-3** — peer handshake (`19:BitTorrent protocol` + reserved + infohash + peer_id).
//! - **BEP-10** — extension protocol (reserved bit `0x10`; extended handshake ile
//!   karşı tarafın `ut_metadata` mesaj kimliği ve `metadata_size` öğrenilir).
//! - **BEP-9** — `ut_metadata`: metadata (info sözlüğü) 16 KiB'lik parçalar hâlinde
//!   istenir, birleştirilir, SHA-1'i infohash ile doğrulanır.
//!
//! Neden kendi implementasyonumuz (spike, ARCHITECTURE §7.2): `librqbit` olgun ama
//! ağır ve tam bir torrent istemcisi; bizim tek ihtiyacımız metadata değişimi.
//! DHT harvester'da olduğu gibi burada da ince, kontrollü ve test edilebilir bir
//! wire katmanı yazmak daha uygun.

use std::net::SocketAddrV4;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use mainline::Id;

use crate::error::PeerError;

const PSTR: &[u8] = b"BitTorrent protocol";
const HANDSHAKE_LEN: usize = 68;
/// BEP-9 metadata parça boyutu.
const METADATA_PIECE_SIZE: usize = 16 * 1024;
/// Bizim karşı tarafa bildirdiğimiz `ut_metadata` mesaj kimliği.
const OUR_UT_METADATA_ID: u8 = 1;
/// Peer wire mesaj kimliği: extended (BEP-10).
const MSG_EXTENDED: u8 = 20;
/// Extended mesaj kimliği 0 = extended handshake.
const EXT_HANDSHAKE_ID: u8 = 0;
/// Kötü niyetli peer'e karşı tek mesaj için üst sınır (metadata parçaları ~16 KiB).
const MAX_MSG_LEN: usize = 1 << 20;
/// Metadata için makul üst sınır (16 MiB) — abartılı boyutları reddet.
const MAX_METADATA_SIZE: i64 = 16 * 1024 * 1024;
/// Extended handshake beklerken taranacak azami mesaj sayısı.
const MAX_HANDSHAKE_MESSAGES: usize = 32;

/// Verilen peer'e bağlanıp infohash'in **doğrulanmış ham info sözlüğü baytlarını** döner.
///
/// Dönen baytların SHA-1'i `infohash`'e eşittir; doğrudan bencode olarak çözülebilir.
pub async fn fetch_info_from_peer(
    addr: SocketAddrV4,
    infohash: [u8; 20],
    timeout: Duration,
) -> Result<Vec<u8>, PeerError> {
    match tokio::time::timeout(timeout, fetch_inner(addr, infohash)).await {
        Ok(result) => result,
        Err(_) => Err(PeerError::Timeout),
    }
}

async fn fetch_inner(addr: SocketAddrV4, infohash: [u8; 20]) -> Result<Vec<u8>, PeerError> {
    let mut stream = TcpStream::connect(addr).await?;

    // --- BEP-3 handshake ---
    stream.write_all(&build_handshake(&infohash)).await?;
    let mut hs = [0u8; HANDSHAKE_LEN];
    stream.read_exact(&mut hs).await?;
    let supports_ext = parse_handshake(&hs, &infohash)?;
    if !supports_ext {
        return Err(PeerError::NoExtension);
    }

    // --- BEP-10 extended handshake ---
    stream.write_all(&build_extended_handshake()).await?;

    let (peer_ut_metadata_id, metadata_size) = read_extended_handshake(&mut stream).await?;
    if peer_ut_metadata_id == 0 {
        return Err(PeerError::NoUtMetadata);
    }
    if metadata_size <= 0 || metadata_size > MAX_METADATA_SIZE {
        return Err(PeerError::BadMetadataSize(metadata_size));
    }
    let metadata_size = metadata_size as usize;

    // --- BEP-9 ut_metadata: tüm parçaları çek ---
    let num_pieces = metadata_size.div_ceil(METADATA_PIECE_SIZE);
    let mut metadata = vec![0u8; metadata_size];
    let mut have = vec![false; num_pieces];
    let mut remaining = num_pieces;

    // Parçaları sırayla iste; her istek sonrası gelen data mesajını bekle.
    // `piece` hem `have`/`metadata` indekslemesi hem de ofset aritmetiği için gerekli.
    #[allow(clippy::needless_range_loop)]
    for piece in 0..num_pieces {
        stream
            .write_all(&build_metadata_request(peer_ut_metadata_id, piece as i64))
            .await?;

        // Bu parçanın data mesajı gelene kadar oku (araya giren diğer mesajları atla).
        loop {
            let (id, payload) = read_message(&mut stream).await?;
            if id != MSG_EXTENDED || payload.is_empty() {
                continue;
            }
            let ext_id = payload[0];
            if ext_id != OUR_UT_METADATA_ID {
                continue; // bize ait ut_metadata mesajı değil.
            }
            let body = &payload[1..];
            let dict_len = bencode_value_len(body).ok_or(PeerError::Bencode)?;
            let dict: serde_bencode::value::Value =
                serde_bencode::from_bytes(&body[..dict_len]).map_err(|_| PeerError::Bencode)?;
            let (msg_type, msg_piece) = parse_metadata_msg(&dict).ok_or(PeerError::Bencode)?;

            match msg_type {
                1 => {
                    // data
                    if msg_piece as usize != piece {
                        continue; // beklediğimiz parça değil.
                    }
                    let data = &body[dict_len..];
                    let start = piece * METADATA_PIECE_SIZE;
                    let end = (start + METADATA_PIECE_SIZE).min(metadata_size);
                    let expected = end - start;
                    if data.len() < expected {
                        return Err(PeerError::ConnectionClosed);
                    }
                    metadata[start..end].copy_from_slice(&data[..expected]);
                    if !have[piece] {
                        have[piece] = true;
                        remaining -= 1;
                    }
                    break;
                }
                2 => return Err(PeerError::PieceRejected(piece as u32)), // reject
                _ => continue,
            }
        }
    }

    if remaining != 0 {
        return Err(PeerError::ConnectionClosed);
    }

    // --- Doğrulama: SHA-1(metadata) == infohash ---
    let digest = sha1_smol::Sha1::from(&metadata).digest().bytes();
    if digest != infohash {
        return Err(PeerError::HashMismatch);
    }

    Ok(metadata)
}

/// 68 baytlık peer handshake üretir (extension biti set).
fn build_handshake(infohash: &[u8; 20]) -> [u8; HANDSHAKE_LEN] {
    let mut out = [0u8; HANDSHAKE_LEN];
    out[0] = PSTR.len() as u8; // 19
    out[1..20].copy_from_slice(PSTR);
    // reserved[5] |= 0x10  → BEP-10 extension protocol desteği.
    out[25] = 0x10;
    out[28..48].copy_from_slice(infohash);
    // peer_id: "-DN0001-" + 12 rastgele bayt.
    let mut peer_id = [0u8; 20];
    peer_id[..8].copy_from_slice(b"-DN0001-");
    peer_id[8..].copy_from_slice(&Id::random().as_bytes()[..12]);
    out[48..68].copy_from_slice(&peer_id);
    out
}

/// Handshake yanıtını doğrular; extension protokolü destekleniyorsa `true` döner.
fn parse_handshake(hs: &[u8; HANDSHAKE_LEN], infohash: &[u8; 20]) -> Result<bool, PeerError> {
    if hs[0] as usize != PSTR.len() || &hs[1..20] != PSTR {
        return Err(PeerError::BadHandshake);
    }
    let supports_ext = hs[25] & 0x10 != 0;
    if &hs[28..48] != infohash {
        return Err(PeerError::InfoHashMismatch);
    }
    Ok(supports_ext)
}

/// Extended handshake mesajı: `{"m": {"ut_metadata": 1}}`.
fn build_extended_handshake() -> Vec<u8> {
    // bencode gövdesi (anahtarlar sıralı): d1:md11:ut_metadatai1eee
    let mut body = Vec::new();
    body.push(b'd');
    push_str(&mut body, b"m");
    body.push(b'd');
    push_str(&mut body, b"ut_metadata");
    push_int(&mut body, OUR_UT_METADATA_ID as i64);
    body.push(b'e');
    body.push(b'e');
    build_extended_message(EXT_HANDSHAKE_ID, &body)
}

/// ut_metadata request: `{"msg_type": 0, "piece": <n>}`.
fn build_metadata_request(peer_ut_metadata_id: u8, piece: i64) -> Vec<u8> {
    // anahtar sırası: msg_type < piece
    let mut body = Vec::new();
    body.push(b'd');
    push_str(&mut body, b"msg_type");
    push_int(&mut body, 0);
    push_str(&mut body, b"piece");
    push_int(&mut body, piece);
    body.push(b'e');
    build_extended_message(peer_ut_metadata_id, &body)
}

/// Bir extended (id=20) peer mesajı çerçeveler: `<len><20><ext_id><payload>`.
fn build_extended_message(ext_id: u8, payload: &[u8]) -> Vec<u8> {
    let len = 2 + payload.len(); // id(1) + ext_id(1) + payload
    let mut out = Vec::with_capacity(4 + len);
    out.extend_from_slice(&(len as u32).to_be_bytes());
    out.push(MSG_EXTENDED);
    out.push(ext_id);
    out.extend_from_slice(payload);
    out
}

/// Extended handshake'i bekler; `(peer_ut_metadata_id, metadata_size)` döner.
async fn read_extended_handshake(stream: &mut TcpStream) -> Result<(u8, i64), PeerError> {
    for _ in 0..MAX_HANDSHAKE_MESSAGES {
        let (id, payload) = read_message(stream).await?;
        if id != MSG_EXTENDED || payload.is_empty() || payload[0] != EXT_HANDSHAKE_ID {
            continue; // bitfield/have vb. ya da diğer extended mesajlar.
        }
        // serde_bencode'a vermeden önce derinlik/sınır doğrula (özyineleme DoS savunması).
        if bencode_value_len(&payload[1..]).is_none() {
            return Err(PeerError::Bencode);
        }
        let dict: serde_bencode::value::Value =
            serde_bencode::from_bytes(&payload[1..]).map_err(|_| PeerError::Bencode)?;
        return parse_extended_handshake(&dict).ok_or(PeerError::NoUtMetadata);
    }
    Err(PeerError::NoUtMetadata)
}

/// Uzunluk-önekli bir peer mesajı okur. `(id, payload)` döner; keep-alive'ları atlar.
async fn read_message(stream: &mut TcpStream) -> Result<(u8, Vec<u8>), PeerError> {
    loop {
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;
        if len == 0 {
            continue; // keep-alive
        }
        if len > MAX_MSG_LEN {
            return Err(PeerError::ConnectionClosed);
        }
        let mut id = [0u8; 1];
        stream.read_exact(&mut id).await?;
        let mut payload = vec![0u8; len - 1];
        stream.read_exact(&mut payload).await?;
        return Ok((id[0], payload));
    }
}

// --- bencode yardımcıları ---

fn push_str(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(s.len().to_string().as_bytes());
    out.push(b':');
    out.extend_from_slice(s);
}

fn push_int(out: &mut Vec<u8>, i: i64) {
    out.push(b'i');
    out.extend_from_slice(i.to_string().as_bytes());
    out.push(b'e');
}

// Bencode güvenlik doğrulaması (derinlik/uzunluk sınırlı) dragnet-core'da paylaşılır:
// dragnet_core::bencode_value_len. serde_bencode'a güvenilmeyen veri vermeden önce çağrılır.
use dragnet_core::bencode_value_len;

/// Extended handshake sözlüğünden `(m.ut_metadata, metadata_size)` çıkarır.
fn parse_extended_handshake(v: &serde_bencode::value::Value) -> Option<(u8, i64)> {
    use serde_bencode::value::Value;
    let Value::Dict(d) = v else { return None };
    let ut_metadata_id = match d.get(b"m".as_ref())? {
        Value::Dict(m) => match m.get(b"ut_metadata".as_ref())? {
            Value::Int(id) => *id as u8,
            _ => return None,
        },
        _ => return None,
    };
    let metadata_size = match d.get(b"metadata_size".as_ref()) {
        Some(Value::Int(n)) => *n,
        _ => 0,
    };
    Some((ut_metadata_id, metadata_size))
}

/// ut_metadata mesaj sözlüğünden `(msg_type, piece)` çıkarır.
fn parse_metadata_msg(v: &serde_bencode::value::Value) -> Option<(i64, i64)> {
    use serde_bencode::value::Value;
    let Value::Dict(d) = v else { return None };
    let msg_type = match d.get(b"msg_type".as_ref())? {
        Value::Int(t) => *t,
        _ => return None,
    };
    let piece = match d.get(b"piece".as_ref())? {
        Value::Int(p) => *p,
        _ => return None,
    };
    Some((msg_type, piece))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_roundtrip() {
        let ih = [0x42u8; 20];
        let hs = build_handshake(&ih);
        assert_eq!(hs.len(), HANDSHAKE_LEN);
        assert_eq!(hs[0], 19);
        assert_eq!(&hs[1..20], PSTR);
        assert_ne!(hs[25] & 0x10, 0, "extension biti set olmalı");
        // Kendi handshake'imizi doğrulayabilmeliyiz.
        assert!(parse_handshake(&hs, &ih).unwrap());
    }

    #[test]
    fn handshake_detects_infohash_mismatch() {
        let hs = build_handshake(&[1u8; 20]);
        assert!(matches!(
            parse_handshake(&hs, &[2u8; 20]),
            Err(PeerError::InfoHashMismatch)
        ));
    }

    #[test]
    fn bencode_len_scans_values() {
        assert_eq!(bencode_value_len(b"i42e"), Some(4));
        assert_eq!(bencode_value_len(b"4:spam"), Some(6));
        assert_eq!(bencode_value_len(b"le"), Some(2));
        // sözlük + ardından ham veri: yalnız sözlük uzunluğu dönmeli.
        let d = b"d8:msg_typei1e5:piecei0e10:total_sizei20eeRAWDATA";
        let n = bencode_value_len(d).unwrap();
        assert_eq!(&d[..n], b"d8:msg_typei1e5:piecei0e10:total_sizei20ee");
        assert_eq!(&d[n..], b"RAWDATA");
    }

    #[test]
    fn bencode_rejects_hostile_input() {
        // Derin iç içe kaplar → None (stack overflow tetiklenmez).
        let deep = vec![b'l'; 5000];
        assert_eq!(bencode_value_len(&deep), None);
        // Aşırı/uydurma string uzunluğu → None (slice paniği yok).
        assert_eq!(bencode_value_len(b"999:ab"), None);
        assert_eq!(bencode_value_len(b"18446744073709551616:x"), None); // usize taşması
                                                                        // Geçerli girdi hâlâ çalışır.
        assert_eq!(bencode_value_len(b"4:spam"), Some(6));
        assert_eq!(bencode_value_len(b"ld1:ai1eee"), Some(10));
    }

    #[test]
    fn parses_extended_handshake_dict() {
        // d1:md11:ut_metadatai3ee13:metadata_sizei12345ee
        let mut body = Vec::new();
        body.push(b'd');
        push_str(&mut body, b"m");
        body.push(b'd');
        push_str(&mut body, b"ut_metadata");
        push_int(&mut body, 3);
        body.push(b'e');
        push_str(&mut body, b"metadata_size");
        push_int(&mut body, 12345);
        body.push(b'e');
        let v: serde_bencode::value::Value = serde_bencode::from_bytes(&body).unwrap();
        assert_eq!(parse_extended_handshake(&v), Some((3, 12345)));
    }

    #[test]
    fn builds_valid_metadata_request() {
        let msg = build_metadata_request(3, 2);
        // <len:4><20><ext_id=3><bencode>
        let len = u32::from_be_bytes(msg[..4].try_into().unwrap()) as usize;
        assert_eq!(len, msg.len() - 4);
        assert_eq!(msg[4], MSG_EXTENDED);
        assert_eq!(msg[5], 3);
        let v: serde_bencode::value::Value = serde_bencode::from_bytes(&msg[6..]).unwrap();
        assert_eq!(parse_metadata_msg(&v), Some((0, 2)));
    }
}
