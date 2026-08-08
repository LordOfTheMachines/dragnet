// SPDX-License-Identifier: AGPL-3.0-only
//! Minimal KRPC (BEP-5) mesaj çözümleme ve üretimi.
//!
//! Neden kendi katmanımız: aday crate'ler (`mainline`, `rustydht-lib`) gelen
//! `get_peers`/`announce_peer` sorgularının **gövdesini** (dolayısıyla infohash'i)
//! dışa açmıyor — biri sadece `bool` filtre kancası veriyor, diğeri artık
//! yayınlanmıyor. Pasif hasat için gelen her paketi kendimiz görmemiz gerektiğinden
//! ince bir KRPC katmanı yazdık. Gerekçenin tamamı: `docs/ARCHITECTURE.md` §7.
//!
//! Çözümleme toleranslıdır (`serde_bencode::Value` üzerinden, bilinmeyen alanları
//! yok sayar). Üretim ise elle yapılır çünkü bencode sözlük anahtarları ham bayt
//! sırasına göre **sıralı** olmalıdır; burada anahtarları bilerek sıralı yazıyoruz.

use std::net::{Ipv4Addr, SocketAddrV4};

use serde_bencode::value::Value;

/// KRPC düğüm kimliği / hedef uzunluğu (BitTorrent v1).
pub const ID_LEN: usize = 20;
/// Compact node info uzunluğu: 20 bayt id + 4 bayt IPv4 + 2 bayt port.
const COMPACT_NODE_LEN: usize = ID_LEN + 6;

/// Gelen bir sorgunun türü.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Ping,
    FindNode,
    GetPeers,
    AnnouncePeer,
    /// Tanımadığımız bir sorgu metodu.
    Other,
}

/// Karşı taraftan gelen bir sorgu (`y = "q"`).
#[derive(Debug, Clone)]
pub struct Query {
    pub txid: Vec<u8>,
    pub method: Method,
    pub info_hash: Option<[u8; ID_LEN]>,
}

/// Karşı taraftan gelen bir yanıt (`y = "r"`).
#[derive(Debug, Clone)]
pub struct Response {
    /// `nodes` alanından çözülen compact IPv4 düğümleri (crawl'ı yaymak için).
    pub nodes: Vec<SocketAddrV4>,
}

/// Çözülmüş bir KRPC mesajı.
#[derive(Debug, Clone)]
pub enum Message {
    Query(Query),
    Response(Response),
    /// Hata (`y = "e"`) ya da ilgilenmediğimiz bir biçim.
    Other,
}

/// Ham UDP yükünü bir [`Message`]'a çözer. Bozuk/ilgisiz paketlerde `None`.
pub fn parse(buf: &[u8]) -> Option<Message> {
    let value: Value = serde_bencode::from_bytes(buf).ok()?;
    let dict = as_dict(&value)?;

    match dict_bytes(dict, b"y") {
        Some(b"q") => {
            let txid = dict_bytes(dict, b"t").unwrap_or_default().to_vec();
            let method = match dict_bytes(dict, b"q") {
                Some(b"ping") => Method::Ping,
                Some(b"find_node") => Method::FindNode,
                Some(b"get_peers") => Method::GetPeers,
                Some(b"announce_peer") => Method::AnnouncePeer,
                _ => Method::Other,
            };
            let info_hash = dict_get(dict, b"a")
                .and_then(as_dict)
                .and_then(|a| dict_bytes(a, b"info_hash"))
                .and_then(to_id);
            Some(Message::Query(Query {
                txid,
                method,
                info_hash,
            }))
        }
        Some(b"r") => {
            let nodes = dict_get(dict, b"r")
                .and_then(as_dict)
                .and_then(|r| dict_bytes(r, b"nodes"))
                .map(parse_compact_nodes)
                .unwrap_or_default();
            Some(Message::Response(Response { nodes }))
        }
        _ => Some(Message::Other),
    }
}

/// Compact node info (`26*N` bayt) → IPv4 soket adresleri.
fn parse_compact_nodes(bytes: &[u8]) -> Vec<SocketAddrV4> {
    bytes
        .chunks_exact(COMPACT_NODE_LEN)
        .map(|c| {
            let ip = Ipv4Addr::new(c[ID_LEN], c[ID_LEN + 1], c[ID_LEN + 2], c[ID_LEN + 3]);
            let port = u16::from_be_bytes([c[ID_LEN + 4], c[ID_LEN + 5]]);
            SocketAddrV4::new(ip, port)
        })
        // 0 portlu / 0.0.0.0 düğümleri ele.
        .filter(|a| a.port() != 0 && !a.ip().is_unspecified())
        .collect()
}

// --- Value yardımcıları ---

fn as_dict(v: &Value) -> Option<&std::collections::HashMap<Vec<u8>, Value>> {
    match v {
        Value::Dict(d) => Some(d),
        _ => None,
    }
}

fn dict_get<'a>(
    d: &'a std::collections::HashMap<Vec<u8>, Value>,
    key: &[u8],
) -> Option<&'a Value> {
    d.get(key)
}

fn dict_bytes<'a>(
    d: &'a std::collections::HashMap<Vec<u8>, Value>,
    key: &[u8],
) -> Option<&'a [u8]> {
    match d.get(key)? {
        Value::Bytes(b) => Some(b),
        _ => None,
    }
}

fn to_id(bytes: &[u8]) -> Option<[u8; ID_LEN]> {
    bytes.try_into().ok()
}

// --- Giden mesaj üretimi (anahtarlar ham bayt sırasına göre sıralı) ---

/// `<len>:<bytes>` bencode string parçası ekler.
fn push_str(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(s.len().to_string().as_bytes());
    out.push(b':');
    out.extend_from_slice(s);
}

/// `find_node` sorgusu üretir. Aktif crawl'da düğümlere gönderilir; bizi
/// yönlendirme tablolarına ekletir ve karşılığında yeni düğümler öğreniriz.
///
/// Sözlük anahtar sırası: `a < q < t < y`; `a` içinde `id < target`.
pub fn build_find_node(txid: &[u8], our_id: &[u8; ID_LEN], target: &[u8; ID_LEN]) -> Vec<u8> {
    let mut o = Vec::with_capacity(80);
    o.push(b'd');
    push_str(&mut o, b"a");
    o.push(b'd');
    push_str(&mut o, b"id");
    push_str(&mut o, our_id);
    push_str(&mut o, b"target");
    push_str(&mut o, target);
    o.push(b'e');
    push_str(&mut o, b"q");
    push_str(&mut o, b"find_node");
    push_str(&mut o, b"t");
    push_str(&mut o, txid);
    push_str(&mut o, b"y");
    push_str(&mut o, b"q");
    o.push(b'e');
    o
}

/// Yalnızca `id` içeren yanıt (`ping`, `announce_peer` ve `find_node` için yeterli ack).
///
/// Sözlük anahtar sırası: `r < t < y`; `r` içinde yalnız `id`.
pub fn build_response_id_only(txid: &[u8], our_id: &[u8; ID_LEN]) -> Vec<u8> {
    let mut o = Vec::with_capacity(48);
    o.push(b'd');
    push_str(&mut o, b"r");
    o.push(b'd');
    push_str(&mut o, b"id");
    push_str(&mut o, our_id);
    o.push(b'e');
    push_str(&mut o, b"t");
    push_str(&mut o, txid);
    push_str(&mut o, b"y");
    push_str(&mut o, b"r");
    o.push(b'e');
    o
}

/// `get_peers` yanıtı: `id`, boş `nodes` ve bir `token`. Token'ı doğrulamıyoruz
/// (peer saklamıyoruz); yalnız sorgulayanı memnun edip tabloda kalmak için yollarız.
///
/// `r` içi anahtar sırası: `id < nodes < token`.
pub fn build_get_peers_response(txid: &[u8], our_id: &[u8; ID_LEN], token: &[u8]) -> Vec<u8> {
    let mut o = Vec::with_capacity(64);
    o.push(b'd');
    push_str(&mut o, b"r");
    o.push(b'd');
    push_str(&mut o, b"id");
    push_str(&mut o, our_id);
    push_str(&mut o, b"nodes");
    push_str(&mut o, b"");
    push_str(&mut o, b"token");
    push_str(&mut o, token);
    o.push(b'e');
    push_str(&mut o, b"t");
    push_str(&mut o, txid);
    push_str(&mut o, b"y");
    push_str(&mut o, b"r");
    o.push(b'e');
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_get_peers_query_extracts_infohash() {
        let ih = [0xabu8; ID_LEN];
        let id = [0x11u8; ID_LEN];
        // d1:ad2:id20:<id>9:info_hash20:<ih>e1:q9:get_peers1:t2:aa1:y1:qe
        let mut pkt = Vec::new();
        pkt.push(b'd');
        push_str(&mut pkt, b"a");
        pkt.push(b'd');
        push_str(&mut pkt, b"id");
        push_str(&mut pkt, &id);
        push_str(&mut pkt, b"info_hash");
        push_str(&mut pkt, &ih);
        pkt.push(b'e');
        push_str(&mut pkt, b"q");
        push_str(&mut pkt, b"get_peers");
        push_str(&mut pkt, b"t");
        push_str(&mut pkt, b"aa");
        push_str(&mut pkt, b"y");
        push_str(&mut pkt, b"q");
        pkt.push(b'e');

        let msg = parse(&pkt).expect("parse");
        match msg {
            Message::Query(q) => {
                assert_eq!(q.method, Method::GetPeers);
                assert_eq!(q.info_hash, Some(ih));
                assert_eq!(q.txid, b"aa");
            }
            other => panic!("beklenen Query, gelen {other:?}"),
        }
    }

    #[test]
    fn parse_response_decodes_compact_nodes() {
        // İki compact düğüm.
        let mut nodes = Vec::new();
        nodes.extend_from_slice(&[1u8; ID_LEN]);
        nodes.extend_from_slice(&[1, 2, 3, 4]); // 1.2.3.4
        nodes.extend_from_slice(&6881u16.to_be_bytes());
        nodes.extend_from_slice(&[2u8; ID_LEN]);
        nodes.extend_from_slice(&[5, 6, 7, 8]); // 5.6.7.8
        nodes.extend_from_slice(&1337u16.to_be_bytes());

        let mut pkt = Vec::new();
        pkt.push(b'd');
        push_str(&mut pkt, b"r");
        pkt.push(b'd');
        push_str(&mut pkt, b"id");
        push_str(&mut pkt, &[9u8; ID_LEN]);
        push_str(&mut pkt, b"nodes");
        push_str(&mut pkt, &nodes);
        pkt.push(b'e');
        push_str(&mut pkt, b"t");
        push_str(&mut pkt, b"aa");
        push_str(&mut pkt, b"y");
        push_str(&mut pkt, b"r");
        pkt.push(b'e');

        match parse(&pkt).expect("parse") {
            Message::Response(r) => {
                assert_eq!(r.nodes.len(), 2);
                assert_eq!(r.nodes[0], "1.2.3.4:6881".parse().unwrap());
                assert_eq!(r.nodes[1], "5.6.7.8:1337".parse().unwrap());
            }
            other => panic!("beklenen Response, gelen {other:?}"),
        }
    }

    #[test]
    fn built_find_node_roundtrips() {
        let id = [0x22u8; ID_LEN];
        let target = [0x33u8; ID_LEN];
        let pkt = build_find_node(b"zz", &id, &target);
        // Kendi ürettiğimiz paket geçerli bir sorgu olarak yeniden çözülebilmeli.
        let value: Value = serde_bencode::from_bytes(&pkt).expect("geçerli bencode");
        match value {
            Value::Dict(_) => {}
            _ => panic!("sözlük bekleniyordu"),
        }
        // find_node bir sorgudur ama info_hash taşımaz.
        match parse(&pkt).expect("parse") {
            Message::Query(q) => {
                assert_eq!(q.method, Method::FindNode);
                assert_eq!(q.info_hash, None);
            }
            other => panic!("beklenen Query, gelen {other:?}"),
        }
    }

    #[test]
    fn built_responses_are_valid_bencode() {
        let id = [0x44u8; ID_LEN];
        for pkt in [
            build_response_id_only(b"aa", &id),
            build_get_peers_response(b"bb", &id, b"tok0"),
        ] {
            let _v: Value = serde_bencode::from_bytes(&pkt).expect("geçerli bencode");
            assert!(matches!(parse(&pkt), Some(Message::Response(_))));
        }
    }
}
