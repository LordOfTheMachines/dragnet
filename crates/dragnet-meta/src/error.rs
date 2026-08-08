// SPDX-License-Identifier: AGPL-3.0-only
//! dragnet-meta hata tipleri.

/// Tek bir peer'den metadata çekerken oluşabilecek hatalar.
#[derive(Debug, thiserror::Error)]
pub enum PeerError {
    #[error("bağlantı/IO hatası: {0}")]
    Io(#[from] std::io::Error),

    #[error("işlem zaman aşımına uğradı")]
    Timeout,

    #[error("geçersiz handshake (protokol uyuşmuyor)")]
    BadHandshake,

    #[error("peer BEP-10 extension protokolünü desteklemiyor")]
    NoExtension,

    #[error("peer ut_metadata (BEP-9) desteklemiyor")]
    NoUtMetadata,

    #[error("peer info_hash uyuşmuyor")]
    InfoHashMismatch,

    #[error("metadata boyutu geçersiz ya da çok büyük: {0}")]
    BadMetadataSize(i64),

    #[error("peer metadata parçasını reddetti (piece {0})")]
    PieceRejected(u32),

    #[error("bencode çözümleme hatası")]
    Bencode,

    #[error("indirilen metadata sha1'i infohash ile eşleşmiyor")]
    HashMismatch,

    #[error("metadata info sözlüğü eksik/bozuk alan: {0}")]
    BadInfoDict(&'static str),

    #[error("peer beklenmedik biçimde bağlantıyı kapattı")]
    ConnectionClosed,
}

/// Bir infohash için metadata çekiminin (birden çok peer denemesi) sonucu.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("DHT'de peer bulunamadı")]
    NoPeers,

    #[error("hiçbir peer metadata veremedi (denenen: {tried})")]
    AllPeersFailed { tried: usize },

    #[error("DHT hatası: {0}")]
    Dht(#[from] std::io::Error),

    #[error("genel zaman aşımı")]
    Timeout,
}
