// SPDX-License-Identifier: AGPL-3.0-only
//! dragnet-store — Kalıcılık + tam-metin arama indeksi (Faz 3).
//!
//! SQLite (gömülü) üzerinde `torrents`, `files` ve FTS5 `torrents_fts` tablolarını
//! yönetir. Yazma yolu **idempotent**tir: aynı infohash tekrar görülürse yeni satır
//! açılmaz, `last_seen` / `seen_count` güncellenir (popülerlik vekili).
//!
//! Derleme hermetiktir: compile-time sorgu makrosu yerine runtime `sqlx::query`
//! kullanılır, dolayısıyla `DATABASE_URL` gerekmez.

use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use tracing::debug;

use dragnet_core::{InfoHash, TorrentFile, TorrentRecord};

/// Metadata çekim durumu.
pub const STATUS_PENDING: &str = "pending";
pub const STATUS_FETCHED: &str = "fetched";

/// Depolama hataları.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("veritabanı hatası: {0}")]
    Sqlx(#[from] sqlx::Error),

    #[error("kayıtta geçersiz infohash hex: {0}")]
    BadInfoHash(String),
}

/// Arama sonucu için hafif özet (dosya listesi olmadan).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentSummary {
    pub infohash: InfoHash,
    pub name: String,
    pub total_size: u64,
    pub file_count: u64,
    pub seen_count: u64,
    pub first_seen: i64,
    pub last_seen: i64,
}

/// SQLite tabanlı indeks deposu.
#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    /// Bir dosya yolundan depo açar (yoksa oluşturur) ve şemayı hazırlar.
    pub async fn open(path: &str) -> Result<Self, StoreError> {
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{path}"))?
            .create_if_missing(true)
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = SqlitePoolOptions::new().max_connections(5).connect_with(opts).await?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    /// Test için paylaşımlı bellek-içi (in-memory) depo.
    pub async fn in_memory() -> Result<Self, StoreError> {
        // max_connections(1): bellek-içi DB tek bağlantıya bağlıdır.
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")?;
        let pool = SqlitePoolOptions::new().max_connections(1).connect_with(opts).await?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    /// Şemayı oluşturur (idempotent — `IF NOT EXISTS`).
    async fn migrate(&self) -> Result<(), StoreError> {
        sqlx::query("PRAGMA journal_mode=WAL;").execute(&self.pool).await.ok();
        sqlx::query("PRAGMA foreign_keys=ON;").execute(&self.pool).await?;
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS torrents (
                infohash        TEXT PRIMARY KEY,
                name            TEXT NOT NULL,
                total_size      INTEGER NOT NULL,
                file_count      INTEGER NOT NULL,
                first_seen      INTEGER NOT NULL,
                last_seen       INTEGER NOT NULL,
                seen_count      INTEGER NOT NULL,
                metadata_status TEXT NOT NULL DEFAULT 'pending'
            );"#,
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS files (
                id       INTEGER PRIMARY KEY AUTOINCREMENT,
                infohash TEXT NOT NULL REFERENCES torrents(infohash) ON DELETE CASCADE,
                path     TEXT NOT NULL,
                size     INTEGER NOT NULL
            );"#,
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_files_infohash ON files(infohash);")
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "CREATE VIRTUAL TABLE IF NOT EXISTS torrents_fts USING fts5(name, infohash UNINDEXED);",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Harvester yolu: bir infohash görüldüğünde çağrılır. Yeniyse `pending` bir
    /// iskelet satır açar; varsa `last_seen`/`seen_count` günceller. Metadata'ya
    /// dokunmaz (FTS'e yazmaz).
    pub async fn record_sighting(&self, infohash: InfoHash, ts: i64) -> Result<(), StoreError> {
        let hex = infohash.to_hex();
        sqlx::query(
            r#"INSERT INTO torrents
                 (infohash, name, total_size, file_count, first_seen, last_seen, seen_count, metadata_status)
               VALUES (?1, '', 0, 0, ?2, ?2, 1, 'pending')
               ON CONFLICT(infohash) DO UPDATE SET
                 last_seen  = MAX(last_seen, excluded.last_seen),
                 seen_count = seen_count + 1;"#,
        )
        .bind(&hex)
        .bind(ts)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetcher yolu: çekilmiş metadata'yı yazar. Idempotent — tekrar çağrılırsa
    /// alanları tazeler, `seen_count`'u artırır, dosya listesini ve FTS'i yeniler.
    pub async fn upsert_torrent(&self, rec: &TorrentRecord) -> Result<(), StoreError> {
        let hex = rec.infohash.to_hex();
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            r#"INSERT INTO torrents
                 (infohash, name, total_size, file_count, first_seen, last_seen, seen_count, metadata_status)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'fetched')
               ON CONFLICT(infohash) DO UPDATE SET
                 name            = excluded.name,
                 total_size      = excluded.total_size,
                 file_count      = excluded.file_count,
                 first_seen      = MIN(first_seen, excluded.first_seen),
                 last_seen       = MAX(last_seen, excluded.last_seen),
                 seen_count      = seen_count + 1,
                 metadata_status = 'fetched';"#,
        )
        .bind(&hex)
        .bind(&rec.name)
        .bind(rec.total_size as i64)
        .bind(rec.files.len() as i64)
        .bind(rec.first_seen)
        .bind(rec.last_seen)
        .bind(rec.seen_count.max(1) as i64)
        .execute(&mut *tx)
        .await?;

        // Dosya listesini tazele.
        sqlx::query("DELETE FROM files WHERE infohash = ?1")
            .bind(&hex)
            .execute(&mut *tx)
            .await?;
        for f in &rec.files {
            sqlx::query("INSERT INTO files (infohash, path, size) VALUES (?1, ?2, ?3)")
                .bind(&hex)
                .bind(&f.path)
                .bind(f.size as i64)
                .execute(&mut *tx)
                .await?;
        }

        // FTS'i tazele.
        sqlx::query("DELETE FROM torrents_fts WHERE infohash = ?1")
            .bind(&hex)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO torrents_fts (name, infohash) VALUES (?1, ?2)")
            .bind(&rec.name)
            .bind(&hex)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        debug!(infohash = %rec.infohash, name = %rec.name, "torrent yazıldı");
        Ok(())
    }

    /// Bir infohash'in tam kaydını (dosyalarıyla) getirir.
    pub async fn get(&self, infohash: InfoHash) -> Result<Option<TorrentRecord>, StoreError> {
        let hex = infohash.to_hex();
        let row = sqlx::query(
            "SELECT name, total_size, first_seen, last_seen, seen_count
               FROM torrents WHERE infohash = ?1 AND metadata_status = 'fetched'",
        )
        .bind(&hex)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else { return Ok(None) };
        let files = sqlx::query("SELECT path, size FROM files WHERE infohash = ?1 ORDER BY id")
            .bind(&hex)
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|r| TorrentFile {
                path: r.get::<String, _>("path"),
                size: r.get::<i64, _>("size") as u64,
            })
            .collect();

        Ok(Some(TorrentRecord {
            infohash,
            name: row.get::<String, _>("name"),
            total_size: row.get::<i64, _>("total_size") as u64,
            files,
            first_seen: row.get::<i64, _>("first_seen"),
            last_seen: row.get::<i64, _>("last_seen"),
            seen_count: row.get::<i64, _>("seen_count") as u64,
        }))
    }

    /// FTS5 üzerinde `name` araması. Popülerliğe (`seen_count`) göre sıralar.
    pub async fn search(&self, query: &str, limit: i64) -> Result<Vec<TorrentSummary>, StoreError> {
        let match_query = to_fts_query(query);
        if match_query.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            r#"SELECT t.infohash, t.name, t.total_size, t.file_count,
                      t.seen_count, t.first_seen, t.last_seen
                 FROM torrents_fts f
                 JOIN torrents t ON t.infohash = f.infohash
                WHERE f.name MATCH ?1 AND t.metadata_status = 'fetched'
                ORDER BY t.seen_count DESC, t.last_seen DESC
                LIMIT ?2"#,
        )
        .bind(&match_query)
        .bind(limit.max(0))
        .fetch_all(&self.pool)
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let hex: String = r.get("infohash");
            let infohash = InfoHash::from_hex(&hex).ok_or(StoreError::BadInfoHash(hex))?;
            out.push(TorrentSummary {
                infohash,
                name: r.get::<String, _>("name"),
                total_size: r.get::<i64, _>("total_size") as u64,
                file_count: r.get::<i64, _>("file_count") as u64,
                seen_count: r.get::<i64, _>("seen_count") as u64,
                first_seen: r.get::<i64, _>("first_seen"),
                last_seen: r.get::<i64, _>("last_seen"),
            });
        }
        Ok(out)
    }

    /// Bu infohash için metadata çekilmiş mi? (Dosyaları yüklemeden hızlı kontrol.)
    pub async fn has_metadata(&self, infohash: InfoHash) -> Result<bool, StoreError> {
        let hex = infohash.to_hex();
        let row = sqlx::query(
            "SELECT 1 AS x FROM torrents WHERE infohash = ?1 AND metadata_status = 'fetched'",
        )
        .bind(&hex)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    /// Metadata'sı çekilmiş (aranabilir) torrent sayısı.
    pub async fn count_fetched(&self) -> Result<i64, StoreError> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM torrents WHERE metadata_status = 'fetched'")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get::<i64, _>("n"))
    }

    /// Toplam bilinen infohash sayısı (pending dahil).
    pub async fn count_total(&self) -> Result<i64, StoreError> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM torrents")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get::<i64, _>("n"))
    }
}

/// Kullanıcı sorgusunu güvenli bir FTS5 MATCH ifadesine çevirir.
///
/// Her kelimeyi alfanümerik karakterlere indirger ve önek (`*`) araması yapar;
/// terimleri AND (boşluk) ile birleştirir. FTS5 özel sözdizimini nötrler.
fn to_fts_query(input: &str) -> String {
    let mut terms = Vec::new();
    for word in input.split_whitespace() {
        let cleaned: String = word
            .chars()
            .filter(|c| c.is_alphanumeric())
            .collect::<String>();
        if !cleaned.is_empty() {
            // Terimi tırnak içine al ve önek `*` ekle: "term"*
            terms.push(format!("\"{cleaned}\"*"));
        }
    }
    terms.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(hex: &str, name: &str, size: u64) -> TorrentRecord {
        TorrentRecord {
            infohash: InfoHash::from_hex(hex).unwrap(),
            name: name.to_string(),
            total_size: size,
            files: vec![TorrentFile {
                path: name.to_string(),
                size,
            }],
            first_seen: 1000,
            last_seen: 1000,
            seen_count: 1,
        }
    }

    #[tokio::test]
    async fn upsert_and_get_roundtrip() {
        let store = Store::in_memory().await.unwrap();
        let rec = record("0123456789abcdef0123456789abcdef01234567", "Ubuntu 24.04", 4096);
        store.upsert_torrent(&rec).await.unwrap();

        let got = store.get(rec.infohash).await.unwrap().expect("kayıt olmalı");
        assert_eq!(got.name, "Ubuntu 24.04");
        assert_eq!(got.total_size, 4096);
        assert_eq!(got.files.len(), 1);
        assert_eq!(store.count_fetched().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn upsert_is_idempotent_and_bumps_seen_count() {
        let store = Store::in_memory().await.unwrap();
        let rec = record("0123456789abcdef0123456789abcdef01234567", "Debian", 512);
        store.upsert_torrent(&rec).await.unwrap();
        store.upsert_torrent(&rec).await.unwrap();
        store.upsert_torrent(&rec).await.unwrap();

        // Tek satır, seen_count artmış olmalı.
        assert_eq!(store.count_fetched().await.unwrap(), 1);
        let got = store.get(rec.infohash).await.unwrap().unwrap();
        assert_eq!(got.seen_count, 3);
        assert_eq!(got.files.len(), 1, "dosyalar çoğaltılmamalı");
    }

    #[tokio::test]
    async fn search_matches_by_name_prefix() {
        let store = Store::in_memory().await.unwrap();
        store
            .upsert_torrent(&record(
                "1111111111111111111111111111111111111111",
                "Ubuntu 24.04 Desktop amd64",
                100,
            ))
            .await
            .unwrap();
        store
            .upsert_torrent(&record(
                "2222222222222222222222222222222222222222",
                "Fedora Workstation 40",
                200,
            ))
            .await
            .unwrap();

        let results = store.search("ubuntu", 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "Ubuntu 24.04 Desktop amd64");

        // Önek araması: "ubun" da eşleşmeli.
        assert_eq!(store.search("ubun", 10).await.unwrap().len(), 1);
        // Alakasız sorgu boş dönmeli.
        assert!(store.search("archlinux", 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn sighting_then_fetch_transitions_to_searchable() {
        let store = Store::in_memory().await.unwrap();
        let ih = InfoHash::from_hex("3333333333333333333333333333333333333333").unwrap();

        // Önce harvester görür (pending, aranamaz).
        store.record_sighting(ih, 500).await.unwrap();
        store.record_sighting(ih, 600).await.unwrap();
        assert_eq!(store.count_total().await.unwrap(), 1);
        assert_eq!(store.count_fetched().await.unwrap(), 0);
        assert!(!store.has_metadata(ih).await.unwrap());
        assert!(store.search("slackware", 10).await.unwrap().is_empty());

        // Sonra metadata gelir → aranabilir olur.
        let mut rec = record("3333333333333333333333333333333333333333", "Slackware 15", 900);
        rec.first_seen = 500;
        store.upsert_torrent(&rec).await.unwrap();
        assert_eq!(store.count_fetched().await.unwrap(), 1);
        assert!(store.has_metadata(ih).await.unwrap());
        let hits = store.search("slackware", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        // first_seen, ilk görülme (500) korunmalı.
        let got = store.get(ih).await.unwrap().unwrap();
        assert_eq!(got.first_seen, 500);
    }

    #[test]
    fn fts_query_sanitizes_special_chars() {
        assert_eq!(to_fts_query("ubuntu 24.04"), "\"ubuntu\"* \"2404\"*");
        assert_eq!(to_fts_query("  a-b (c) "), "\"ab\"* \"c\"*");
        assert_eq!(to_fts_query("!!!"), "");
        assert_eq!(to_fts_query(""), "");
    }
}
