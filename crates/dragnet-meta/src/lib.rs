// SPDX-License-Identifier: AGPL-3.0-only
//! dragnet-meta — Metadata fetcher (Faz 2).
//!
//! Bir infohash alır, DHT'den (`get_peers`) peer bulur, peer'lere bağlanıp
//! BEP-10 extension handshake + BEP-9 `ut_metadata` ile torrent metadata'sını
//! **tracker'sız** çeker, SHA-1 ile doğrular ve bir [`dragnet_core::TorrentRecord`]
//! üretir.
//!
//! Wire protokolü [`wire`] modülündedir; bu modül peer bulma, eşzamanlı deneme ve
//! info sözlüğünü `TorrentRecord`'a çözme işini yapar.

mod error;
pub mod wire;

use std::collections::HashSet;
use std::net::SocketAddrV4;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_lite::StreamExt;
use tracing::debug;

use dragnet_core::{InfoHash, TorrentFile, TorrentRecord};
use mainline::{Dht, Id};

pub use error::{FetchError, PeerError};

/// Metadata çekim davranışını ayarlar. [`Default`] makul değerler verir.
#[derive(Debug, Clone)]
pub struct FetchConfig {
    /// DHT'den peer toplamak için ayrılan süre.
    pub peer_gather_timeout: Duration,
    /// Toplanacak azami benzersiz peer sayısı.
    pub max_peers: usize,
    /// Tek bir peer denemesi için zaman aşımı.
    pub per_peer_timeout: Duration,
    /// Aynı anda denenecek peer sayısı.
    pub concurrency: usize,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            peer_gather_timeout: Duration::from_secs(20),
            // Nazik varsayılanlar: eşzamanlı TCP peer bağlantısını düşük tutarak
            // router bağlantı-izleme tablosunu ve yükü sınırlar.
            max_peers: 50,
            per_peer_timeout: Duration::from_secs(8),
            concurrency: 6,
        }
    }
}

/// DHT üzerinden metadata çeken fetcher. İçinde bir mainline DHT istemcisi tutar.
pub struct MetadataFetcher {
    dht: mainline::async_dht::AsyncDht,
    config: FetchConfig,
}

impl MetadataFetcher {
    /// Yeni bir fetcher oluşturur (kendi DHT istemci düğümünü açar).
    pub fn new(config: FetchConfig) -> std::io::Result<Self> {
        let dht = Dht::client()?.as_async();
        Ok(Self { dht, config })
    }

    /// Bir infohash için metadata çeker ve `TorrentRecord` döner.
    pub async fn fetch(&self, infohash: InfoHash) -> Result<TorrentRecord, FetchError> {
        let peers = self.gather_peers(infohash).await;
        if peers.is_empty() {
            return Err(FetchError::NoPeers);
        }
        debug!(infohash = %infohash, peers = peers.len(), "peer bulundu, metadata deneniyor");
        self.try_peers(infohash, peers).await
    }

    /// DHT `get_peers` akışını verilen süre/sayı sınırına kadar benzersiz peer'lere
    /// boşaltır. Hem metadata için peer bulma hem canlılık scrape'i bunu kullanır.
    async fn drain_peers(
        &self,
        infohash: InfoHash,
        deadline: Instant,
        max: usize,
    ) -> HashSet<SocketAddrV4> {
        let id = Id::from_bytes(infohash.as_bytes()).expect("infohash 20 bayttır");
        let mut stream = self.dht.get_peers(id);
        let mut seen = HashSet::new();
        loop {
            let now = Instant::now();
            if now >= deadline || seen.len() >= max {
                break;
            }
            match tokio::time::timeout(deadline - now, stream.next()).await {
                Ok(Some(batch)) => seen.extend(batch),
                Ok(None) | Err(_) => break, // sorgu bitti ya da zaman aşımı
            }
        }
        seen
    }

    /// DHT'den bu infohash için peer toplar (zaman/sayı sınırıyla).
    async fn gather_peers(&self, infohash: InfoHash) -> Vec<SocketAddrV4> {
        let deadline = Instant::now() + self.config.peer_gather_timeout;
        self.drain_peers(infohash, deadline, self.config.max_peers)
            .await
            .into_iter()
            .collect()
    }

    /// Canlılık scrape'i: bir infohash için DHT'de `get_peers` yapıp benzersiz
    /// peer sayısını döner (canlı seeder/leecher vekili). Metadata çekmez.
    pub async fn count_peers(&self, infohash: InfoHash, timeout: Duration) -> usize {
        let deadline = Instant::now() + timeout;
        self.drain_peers(infohash, deadline, usize::MAX).await.len()
    }

    /// Peer'leri sınırlı eşzamanlılıkla dener; ilk başarılı metadata kazanır.
    async fn try_peers(
        &self,
        infohash: InfoHash,
        peers: Vec<SocketAddrV4>,
    ) -> Result<TorrentRecord, FetchError> {
        let ih_bytes = *infohash.as_bytes();
        let per_peer = self.config.per_peer_timeout;
        let mut tried = 0;

        for chunk in peers.chunks(self.config.concurrency.max(1)) {
            // JoinSet: en hızlı biten peer kazanır; başarıda kalan kardeş task'ler
            // set drop olunca otomatik iptal edilir (sokak/task sızıntısı yok).
            let mut set = tokio::task::JoinSet::new();
            for &addr in chunk {
                set.spawn(async move { wire::fetch_info_from_peer(addr, ih_bytes, per_peer).await });
            }
            while let Some(res) = set.join_next().await {
                tried += 1;
                match res {
                    Ok(Ok(info_bytes)) => match parse_info_dict(&info_bytes, infohash) {
                        Ok(record) => return Ok(record), // set drop → kalanlar iptal
                        Err(e) => debug!(error = %e, "info sözlüğü çözülemedi"),
                    },
                    Ok(Err(e)) => debug!(error = %e, "peer denemesi başarısız"),
                    Err(_) => {} // task iptal/panik
                }
            }
        }

        Err(FetchError::AllPeersFailed { tried })
    }
}

/// Doğrulanmış ham info sözlüğü baytlarını `TorrentRecord`'a çözer.
///
/// BEP-3 info sözlüğü: `name`, ve ya `length` (tek dosya) ya da `files` (çok dosya).
pub fn parse_info_dict(
    info_bytes: &[u8],
    infohash: InfoHash,
) -> Result<TorrentRecord, PeerError> {
    use serde_bencode::value::Value;

    let value: Value = serde_bencode::from_bytes(info_bytes).map_err(|_| PeerError::Bencode)?;
    let Value::Dict(dict) = value else {
        return Err(PeerError::BadInfoDict("info bir sözlük değil"));
    };

    let name = match dict.get(b"name".as_ref()) {
        Some(Value::Bytes(b)) => String::from_utf8_lossy(b).into_owned(),
        _ => return Err(PeerError::BadInfoDict("name")),
    };

    let (files, total_size) = if let Some(Value::List(list)) = dict.get(b"files".as_ref()) {
        // Çok dosyalı: her giriş {length, path:[bileşenler]}.
        let mut files = Vec::with_capacity(list.len());
        let mut total = 0u64;
        for entry in list {
            let Value::Dict(fd) = entry else {
                return Err(PeerError::BadInfoDict("files girişi sözlük değil"));
            };
            let size = match fd.get(b"length".as_ref()) {
                Some(Value::Int(n)) if *n >= 0 => *n as u64,
                _ => return Err(PeerError::BadInfoDict("files.length")),
            };
            let mut parts = vec![name.clone()];
            match fd.get(b"path".as_ref()) {
                Some(Value::List(comps)) => {
                    for c in comps {
                        if let Value::Bytes(b) = c {
                            parts.push(String::from_utf8_lossy(b).into_owned());
                        }
                    }
                }
                _ => return Err(PeerError::BadInfoDict("files.path")),
            }
            total += size;
            files.push(TorrentFile {
                path: parts.join("/"),
                size,
            });
        }
        (files, total)
    } else if let Some(Value::Int(n)) = dict.get(b"length".as_ref()) {
        // Tek dosyalı.
        if *n < 0 {
            return Err(PeerError::BadInfoDict("length"));
        }
        let size = *n as u64;
        (
            vec![TorrentFile {
                path: name.clone(),
                size,
            }],
            size,
        )
    } else {
        return Err(PeerError::BadInfoDict("length/files ikisi de yok"));
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    Ok(TorrentRecord {
        infohash,
        name,
        total_size,
        files,
        first_seen: now,
        last_seen: now,
        seen_count: 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bilinen içeriğe göre bir info sözlüğü kurar ve gerçek infohash'ini hesaplar.
    fn build_single_file_info() -> (Vec<u8>, InfoHash) {
        // d6:lengthi1024e4:name8:test.isoe
        let info = b"d6:lengthi1024e4:name8:test.isoe".to_vec();
        let digest = sha1_smol::Sha1::from(&info).digest().bytes();
        (info, InfoHash::from_bytes(digest))
    }

    #[test]
    fn parses_single_file_info() {
        let (info, ih) = build_single_file_info();
        let rec = parse_info_dict(&info, ih).expect("çözülmeli");
        assert_eq!(rec.name, "test.iso");
        assert_eq!(rec.total_size, 1024);
        assert_eq!(rec.files.len(), 1);
        assert_eq!(rec.files[0].path, "test.iso");
        assert_eq!(rec.files[0].size, 1024);
        assert_eq!(rec.seen_count, 1);
    }

    #[test]
    fn parses_multi_file_info() {
        // name=pack, files: [{length:10, path:[a.txt]}, {length:20, path:[sub, b.txt]}]
        let info =
            b"d5:filesld6:lengthi10e4:pathl5:a.txteed6:lengthi20e4:pathl3:sub5:b.txteee4:name4:packe"
                .to_vec();
        let ih = InfoHash::from_bytes([0u8; 20]);
        let rec = parse_info_dict(&info, ih).expect("çözülmeli");
        assert_eq!(rec.name, "pack");
        assert_eq!(rec.total_size, 30);
        assert_eq!(rec.files.len(), 2);
        assert_eq!(rec.files[0].path, "pack/a.txt");
        assert_eq!(rec.files[1].path, "pack/sub/b.txt");
    }

    #[test]
    fn rejects_missing_name() {
        let info = b"d6:lengthi1024ee".to_vec();
        let ih = InfoHash::from_bytes([0u8; 20]);
        assert!(matches!(
            parse_info_dict(&info, ih),
            Err(PeerError::BadInfoDict("name"))
        ));
    }

    #[test]
    fn default_config_is_sane() {
        let c = FetchConfig::default();
        assert!(c.max_peers > 0);
        assert!(c.concurrency > 0);
    }
}
