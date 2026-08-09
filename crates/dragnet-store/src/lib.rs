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

use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous,
};
use sqlx::{Row, SqlitePool};
use tracing::debug;

use dragnet_core::{InfoHash, TorrentFile, TorrentRecord};

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
    /// Son DHT scrape'inde görülen canlı peer sayısı (None = henüz kontrol edilmedi).
    pub peer_count: Option<i64>,
    /// Son canlılık kontrolü zamanı (unix ts, None = hiç).
    pub last_check: Option<i64>,
    /// İçerik kategorisi (video/audio/software/game/book/adult/archive/other).
    pub category: String,
}

/// Arama/liste sonuçlarını süzmek için ölçütler.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// Yalnız canlı (peer_count > 0) torrent'ler.
    pub only_alive: bool,
    /// Yetişkin içeriği gizle (category != 'adult').
    pub hide_adult: bool,
    /// Belirli bir kategoriye sınırla.
    pub category: Option<String>,
}

impl Filter {
    /// Kod-kontrollü boolean koşullarının SQL parçası (`t.` önekiyle).
    fn bool_sql(&self, prefix: &str) -> String {
        let mut s = String::new();
        if self.only_alive {
            s.push_str(&format!(" AND {prefix}peer_count > 0"));
        }
        if self.hide_adult {
            s.push_str(&format!(" AND {prefix}category != 'adult'"));
        }
        s
    }
}

/// Ağ/indeks genel görünümü (dashboard analiz paneli).
#[derive(Debug, Clone)]
pub struct Overview {
    pub fetched: i64,
    pub total_infohashes: i64,
    pub total_size: i64,
    pub total_files: i64,
    /// İndeksteki canlı torrent'lerin toplam peer sayısı (anlık swarm büyüklüğü vekili).
    pub total_peers: i64,
    pub alive: i64,
    pub dead: i64,
    pub unchecked: i64,
    /// Kategoriye göre: (kategori, adet, toplam_boyut).
    pub categories: Vec<(String, i64, i64)>,
}

/// SQLite tabanlı indeks deposu.
#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    /// Bir dosya yolundan depo açar (yoksa oluşturur) ve şemayı hazırlar.
    pub async fn open(path: &str) -> Result<Self, StoreError> {
        // PRAGMA'lar connect options'ta: her havuz bağlantısı bunları devralır
        // (migrate tek bağlantıda çalıştığından PRAGMA-per-query etkisiz kalırdı).
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{path}"))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = SqlitePoolOptions::new().max_connections(5).connect_with(opts).await?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    /// Test için paylaşımlı bellek-içi (in-memory) depo.
    pub async fn in_memory() -> Result<Self, StoreError> {
        // max_connections(1): bellek-içi DB tek bağlantıya bağlıdır.
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
        let pool = SqlitePoolOptions::new().max_connections(1).connect_with(opts).await?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    /// Şemayı oluşturur (idempotent — `IF NOT EXISTS`).
    async fn migrate(&self) -> Result<(), StoreError> {
        // journal_mode/synchronous/foreign_keys artık connect options'ta (her bağlantı).
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS torrents (
                infohash        TEXT PRIMARY KEY,
                name            TEXT NOT NULL,
                total_size      INTEGER NOT NULL,
                file_count      INTEGER NOT NULL,
                first_seen      INTEGER NOT NULL,
                last_seen       INTEGER NOT NULL,
                seen_count      INTEGER NOT NULL,
                metadata_status TEXT NOT NULL DEFAULT 'pending',
                peer_count      INTEGER DEFAULT NULL,
                last_check      INTEGER DEFAULT NULL,
                category        TEXT NOT NULL DEFAULT 'other'
            );"#,
        )
        .execute(&self.pool)
        .await?;
        // Eski veritabanları için kolonları ekle (varsa hata yok sayılır).
        let _ = sqlx::query("ALTER TABLE torrents ADD COLUMN peer_count INTEGER DEFAULT NULL")
            .execute(&self.pool)
            .await;
        let _ = sqlx::query("ALTER TABLE torrents ADD COLUMN last_check INTEGER DEFAULT NULL")
            .execute(&self.pool)
            .await;
        let _ = sqlx::query("ALTER TABLE torrents ADD COLUMN category TEXT NOT NULL DEFAULT 'other'")
            .execute(&self.pool)
            .await;
        let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_category ON torrents(category);")
            .execute(&self.pool)
            .await;
        // Sık kullanılan ORDER BY / WHERE / canlılık kolonları için (kısmi) indexler —
        // aksi halde her dashboard/liveness sorgusu tam tablo taraması yapar.
        for idx in [
            "CREATE INDEX IF NOT EXISTS idx_status ON torrents(metadata_status);",
            "CREATE INDEX IF NOT EXISTS idx_last_check ON torrents(last_check) WHERE metadata_status='fetched';",
            "CREATE INDEX IF NOT EXISTS idx_seen ON torrents(seen_count DESC) WHERE metadata_status='fetched';",
            "CREATE INDEX IF NOT EXISTS idx_size ON torrents(total_size DESC) WHERE metadata_status='fetched';",
            "CREATE INDEX IF NOT EXISTS idx_first_seen ON torrents(first_seen) WHERE metadata_status='fetched';",
        ] {
            let _ = sqlx::query(idx).execute(&self.pool).await;
        }
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
    /// dokunmaz (FTS'e yazmaz). Kaydın **güncel metadata_status**'ünü döner —
    /// böylece çağıran ayrı bir SELECT yapmadan çekim gerekip gerekmediğini bilir
    /// (`'pending'` = çekilmeli).
    pub async fn record_sighting(&self, infohash: InfoHash, ts: i64) -> Result<String, StoreError> {
        let hex = infohash.to_hex();
        let row = sqlx::query(
            r#"INSERT INTO torrents
                 (infohash, name, total_size, file_count, first_seen, last_seen, seen_count, metadata_status)
               VALUES (?1, '', 0, 0, ?2, ?2, 1, 'pending')
               ON CONFLICT(infohash) DO UPDATE SET
                 last_seen  = MAX(last_seen, excluded.last_seen),
                 seen_count = seen_count + 1
               RETURNING metadata_status;"#,
        )
        .bind(&hex)
        .bind(ts)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get::<String, _>("metadata_status"))
    }

    /// Fetcher yolu: çekilmiş metadata'yı yazar. Idempotent — tekrar çağrılırsa
    /// alanları tazeler, `seen_count`'u artırır, dosya listesini ve FTS'i yeniler.
    pub async fn upsert_torrent(&self, rec: &TorrentRecord) -> Result<(), StoreError> {
        let hex = rec.infohash.to_hex();
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            r#"INSERT INTO torrents
                 (infohash, name, total_size, file_count, first_seen, last_seen, seen_count, metadata_status, category)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'fetched', ?8)
               ON CONFLICT(infohash) DO UPDATE SET
                 name            = excluded.name,
                 total_size      = excluded.total_size,
                 file_count      = excluded.file_count,
                 first_seen      = MIN(first_seen, excluded.first_seen),
                 last_seen       = MAX(last_seen, excluded.last_seen),
                 seen_count      = seen_count + 1,
                 metadata_status = 'fetched',
                 category        = excluded.category;"#,
        )
        .bind(&hex)
        .bind(&rec.name)
        .bind(rec.total_size as i64)
        .bind(rec.files.len() as i64)
        .bind(rec.first_seen)
        .bind(rec.last_seen)
        .bind(rec.seen_count.max(1) as i64)
        .bind(rec.category())
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

    /// FTS5 üzerinde `name` araması (filtreyle). Popülerliğe göre sıralar.
    pub async fn search(
        &self,
        query: &str,
        limit: i64,
        filter: &Filter,
    ) -> Result<Vec<TorrentSummary>, StoreError> {
        let match_query = to_fts_query(query);
        if match_query.is_empty() {
            return Ok(Vec::new());
        }
        let has_cat = filter.category.is_some();
        let sql = format!(
            "SELECT t.infohash, t.name, t.total_size, t.file_count, t.seen_count,
                    t.first_seen, t.last_seen, t.peer_count, t.last_check, t.category
               FROM torrents_fts f JOIN torrents t ON t.infohash = f.infohash
              WHERE f.name MATCH ?1 AND t.metadata_status = 'fetched'{bools}{cat}
              ORDER BY t.seen_count DESC, t.last_seen DESC
              LIMIT ?{lim}",
            bools = filter.bool_sql("t."),
            cat = if has_cat { " AND t.category = ?2" } else { "" },
            lim = if has_cat { "3" } else { "2" },
        );
        let mut q = sqlx::query(&sql).bind(&match_query);
        if let Some(c) = &filter.category {
            q = q.bind(c.clone());
        }
        let rows = q.bind(limit.max(0)).fetch_all(&self.pool).await?;
        rows.iter().map(row_to_summary).collect()
    }

    /// İndeks/ağ genel görünümü (analiz paneli için toplu istatistikler).
    pub async fn overview(&self) -> Result<Overview, StoreError> {
        let row = sqlx::query(
            "SELECT COUNT(*) AS n,
                    COALESCE(SUM(total_size),0) AS size,
                    COALESCE(SUM(file_count),0) AS files,
                    COALESCE(SUM(CASE WHEN peer_count>0 THEN peer_count ELSE 0 END),0) AS peers,
                    COALESCE(SUM(CASE WHEN peer_count>0 THEN 1 ELSE 0 END),0) AS alive,
                    COALESCE(SUM(CASE WHEN peer_count=0 THEN 1 ELSE 0 END),0) AS dead,
                    COALESCE(SUM(CASE WHEN last_check IS NULL THEN 1 ELSE 0 END),0) AS unchecked
               FROM torrents WHERE metadata_status = 'fetched'",
        )
        .fetch_one(&self.pool)
        .await?;
        let total_infohashes = self.count_total().await?;
        let cats = sqlx::query(
            "SELECT category, COUNT(*) AS n, COALESCE(SUM(total_size),0) AS size
               FROM torrents WHERE metadata_status = 'fetched'
              GROUP BY category ORDER BY n DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(Overview {
            fetched: row.get("n"),
            total_infohashes,
            total_size: row.get("size"),
            total_files: row.get("files"),
            total_peers: row.get("peers"),
            alive: row.get("alive"),
            dead: row.get("dead"),
            unchecked: row.get("unchecked"),
            categories: cats
                .iter()
                .map(|r| {
                    (
                        r.get::<String, _>("category"),
                        r.get::<i64, _>("n"),
                        r.get::<i64, _>("size"),
                    )
                })
                .collect(),
        })
    }

    /// Saatlik keşif sayıları (grafik için): `(saat_başı_unix, sayı)`, en yeni önce.
    pub async fn hourly_discovery(&self, hours: i64) -> Result<Vec<(i64, i64)>, StoreError> {
        let rows = sqlx::query(
            "SELECT (first_seen / 3600) * 3600 AS hour, COUNT(*) AS n
               FROM torrents WHERE metadata_status = 'fetched'
              GROUP BY hour ORDER BY hour DESC LIMIT ?1",
        )
        .bind(hours.max(1))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| (r.get::<i64, _>("hour"), r.get::<i64, _>("n")))
            .collect())
    }

    /// Canlılık kontrolü için sıradaki torrent'ler: en eski kontrol edilenler
    /// (hiç kontrol edilmeyenler = NULL, önce gelir). Nazik yeniden-tarama.
    pub async fn torrents_to_check(&self, limit: i64) -> Result<Vec<InfoHash>, StoreError> {
        let rows = sqlx::query(
            "SELECT infohash FROM torrents WHERE metadata_status = 'fetched'
              ORDER BY last_check ASC LIMIT ?1",
        )
        .bind(limit.max(0))
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let hex: String = r.get("infohash");
            if let Some(ih) = InfoHash::from_hex(&hex) {
                out.push(ih);
            }
        }
        Ok(out)
    }

    /// 'other' kategorili mevcut kayıtları yeniden sınıflandırır (isim + dosyalar).
    /// Şema eklenmeden önce indekslenmiş torrent'lerin kategorilerini düzeltir.
    /// Döndürdüğü: güncellenen kayıt sayısı.
    pub async fn recategorize(&self, limit: i64) -> Result<u64, StoreError> {
        let rows = sqlx::query(
            "SELECT infohash, name FROM torrents
              WHERE metadata_status = 'fetched' AND category = 'other' LIMIT ?1",
        )
        .bind(limit.max(0))
        .fetch_all(&self.pool)
        .await?;

        let mut updated = 0u64;
        for r in rows {
            let hex: String = r.get("infohash");
            let name: String = r.get("name");
            let files: Vec<TorrentFile> =
                sqlx::query("SELECT path, size FROM files WHERE infohash = ?1")
                    .bind(&hex)
                    .fetch_all(&self.pool)
                    .await?
                    .into_iter()
                    .map(|f| TorrentFile {
                        path: f.get::<String, _>("path"),
                        size: f.get::<i64, _>("size") as u64,
                    })
                    .collect();
            let cat = dragnet_core::categorize(&name, &files);
            if cat != "other" {
                sqlx::query("UPDATE torrents SET category = ?2 WHERE infohash = ?1")
                    .bind(&hex)
                    .bind(cat)
                    .execute(&self.pool)
                    .await?;
                updated += 1;
            }
        }
        Ok(updated)
    }

    /// Bir torrent'in canlı peer sayısını ve kontrol zamanını günceller.
    pub async fn update_liveness(
        &self,
        infohash: InfoHash,
        peer_count: i64,
        ts: i64,
    ) -> Result<(), StoreError> {
        sqlx::query("UPDATE torrents SET peer_count = ?2, last_check = ?3 WHERE infohash = ?1")
            .bind(infohash.to_hex())
            .bind(peer_count)
            .bind(ts)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Popülerliğe (`seen_count`) göre en çok görülen torrent'ler (dashboard).
    pub async fn top_by_seen(&self, limit: i64, filter: &Filter) -> Result<Vec<TorrentSummary>, StoreError> {
        self.top_summaries("seen_count", limit, filter).await
    }

    /// Boyuta göre en büyük torrent'ler (dashboard).
    pub async fn top_by_size(&self, limit: i64, filter: &Filter) -> Result<Vec<TorrentSummary>, StoreError> {
        self.top_summaries("total_size", limit, filter).await
    }

    /// En son indekslenen torrent'ler (dashboard).
    pub async fn recent(&self, limit: i64, filter: &Filter) -> Result<Vec<TorrentSummary>, StoreError> {
        self.top_summaries("first_seen", limit, filter).await
    }

    /// Verilen sütuna göre azalan sırada özet listesi (filtreyle). `order_col` yalnız
    /// kod içi sabittir (kullanıcı girdisi değil) → SQL enjeksiyonu yok.
    async fn top_summaries(
        &self,
        order_col: &str,
        limit: i64,
        filter: &Filter,
    ) -> Result<Vec<TorrentSummary>, StoreError> {
        let has_cat = filter.category.is_some();
        let sql = format!(
            "SELECT infohash, name, total_size, file_count, seen_count, first_seen, last_seen,
                    peer_count, last_check, category
               FROM torrents WHERE metadata_status = 'fetched'{bools}{cat}
              ORDER BY {order_col} DESC LIMIT ?{lim}",
            bools = filter.bool_sql(""),
            cat = if has_cat { " AND category = ?1" } else { "" },
            lim = if has_cat { "2" } else { "1" },
        );
        let mut q = sqlx::query(&sql);
        if let Some(c) = &filter.category {
            q = q.bind(c.clone());
        }
        let rows = q.bind(limit.max(0)).fetch_all(&self.pool).await?;
        rows.iter().map(row_to_summary).collect()
    }

    /// Metadata çekilemeyen bir infohash'i `unreachable` işaretler (yalnız `pending` ise).
    /// Böylece gelecekte tekrar tekrar denenmez.
    pub async fn mark_unreachable(&self, infohash: InfoHash) -> Result<(), StoreError> {
        let hex = infohash.to_hex();
        sqlx::query(
            "UPDATE torrents SET metadata_status = 'unreachable' \
             WHERE infohash = ?1 AND metadata_status = 'pending'",
        )
        .bind(&hex)
        .execute(&self.pool)
        .await?;
        Ok(())
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

/// Bir SQLite satırını [`TorrentSummary`]'ye çevirir (özet sorguları için ortak).
fn row_to_summary(r: &sqlx::sqlite::SqliteRow) -> Result<TorrentSummary, StoreError> {
    let hex: String = r.get("infohash");
    let infohash = InfoHash::from_hex(&hex).ok_or(StoreError::BadInfoHash(hex))?;
    Ok(TorrentSummary {
        infohash,
        name: r.get::<String, _>("name"),
        total_size: r.get::<i64, _>("total_size") as u64,
        file_count: r.get::<i64, _>("file_count") as u64,
        seen_count: r.get::<i64, _>("seen_count") as u64,
        first_seen: r.get::<i64, _>("first_seen"),
        last_seen: r.get::<i64, _>("last_seen"),
        peer_count: r.get::<Option<i64>, _>("peer_count"),
        last_check: r.get::<Option<i64>, _>("last_check"),
        category: r.get::<String, _>("category"),
    })
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

        let results = store.search("ubuntu", 10, &Filter::default()).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "Ubuntu 24.04 Desktop amd64");

        // Önek araması: "ubun" da eşleşmeli.
        assert_eq!(store.search("ubun", 10, &Filter::default()).await.unwrap().len(), 1);
        // Alakasız sorgu boş dönmeli.
        assert!(store.search("archlinux", 10, &Filter::default()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn sighting_then_fetch_transitions_to_searchable() {
        let store = Store::in_memory().await.unwrap();
        let ih = InfoHash::from_hex("3333333333333333333333333333333333333333").unwrap();

        // Önce harvester görür (pending, aranamaz). record_sighting durumu döner.
        assert_eq!(store.record_sighting(ih, 500).await.unwrap(), "pending");
        assert_eq!(store.record_sighting(ih, 600).await.unwrap(), "pending");
        assert_eq!(store.count_total().await.unwrap(), 1);
        assert_eq!(store.count_fetched().await.unwrap(), 0);
        assert!(store.search("slackware", 10, &Filter::default()).await.unwrap().is_empty());

        // Sonra metadata gelir → aranabilir olur, sighting artık 'fetched' döner.
        let mut rec = record("3333333333333333333333333333333333333333", "Slackware 15", 900);
        rec.first_seen = 500;
        store.upsert_torrent(&rec).await.unwrap();
        assert_eq!(store.count_fetched().await.unwrap(), 1);
        assert_eq!(store.record_sighting(ih, 950).await.unwrap(), "fetched");
        let hits = store.search("slackware", 10, &Filter::default()).await.unwrap();
        assert_eq!(hits.len(), 1);
        // first_seen, ilk görülme (500) korunmalı.
        let got = store.get(ih).await.unwrap().unwrap();
        assert_eq!(got.first_seen, 500);
    }

    #[tokio::test]
    async fn liveness_check_and_update() {
        let store = Store::in_memory().await.unwrap();
        let rec = record("4444444444444444444444444444444444444444", "Live Test", 100);
        store.upsert_torrent(&rec).await.unwrap();

        // Yeni kayıt: last_check NULL → kontrol edilecekler listesinde, peer_count None.
        let todo = store.torrents_to_check(10).await.unwrap();
        assert_eq!(todo.len(), 1);
        assert_eq!(todo[0], rec.infohash);
        let before = store.search("live", 10, &Filter::default()).await.unwrap();
        assert_eq!(before[0].peer_count, None);

        // Canlılık güncelle → peer_count 7, last_check set.
        store.update_liveness(rec.infohash, 7, 1234).await.unwrap();
        let after = store.search("live", 10, &Filter::default()).await.unwrap();
        assert_eq!(after[0].peer_count, Some(7));
        assert_eq!(after[0].last_check, Some(1234));

        // Artık kontrol edildi → listenin sonuna düşer (tek kayıt olduğu için hâlâ var ama last_check dolu).
        let todo2 = store.torrents_to_check(10).await.unwrap();
        assert_eq!(todo2.len(), 1); // tekrar kontrol edilebilir (en eski)
    }

    #[tokio::test]
    async fn dashboard_queries_order_correctly() {
        let store = Store::in_memory().await.unwrap();
        // A: küçük ama çok görülen; B: büyük ama az görülen.
        let mut a = record("1111111111111111111111111111111111111111", "Alpha", 100);
        a.seen_count = 50;
        let mut b = record("2222222222222222222222222222222222222222", "Beta", 9_000_000);
        b.seen_count = 2;
        store.upsert_torrent(&a).await.unwrap();
        store.upsert_torrent(&b).await.unwrap();

        let by_seen = store.top_by_seen(10, &Filter::default()).await.unwrap();
        assert_eq!(by_seen[0].name, "Alpha"); // en çok görülen önce

        let by_size = store.top_by_size(10, &Filter::default()).await.unwrap();
        assert_eq!(by_size[0].name, "Beta"); // en büyük önce

        assert_eq!(store.recent(10, &Filter::default()).await.unwrap().len(), 2);
        // Saatlik keşif: iki kayıt aynı saatte → toplam 2.
        let hourly = store.hourly_discovery(48).await.unwrap();
        assert_eq!(hourly.iter().map(|(_, n)| n).sum::<i64>(), 2);
    }

    #[test]
    fn fts_query_sanitizes_special_chars() {
        assert_eq!(to_fts_query("ubuntu 24.04"), "\"ubuntu\"* \"2404\"*");
        assert_eq!(to_fts_query("  a-b (c) "), "\"ab\"* \"c\"*");
        assert_eq!(to_fts_query("!!!"), "");
        assert_eq!(to_fts_query(""), "");
    }
}
