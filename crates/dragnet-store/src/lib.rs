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

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
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
    /// Kullanıcı tanımlı engel kelimeleri: adı bunlardan birini (küçük harfe
    /// duyarsız, alt-dize) içeren torrent'ler sonuçlardan gizlenir. Sorgu-anı
    /// (yıkıcı olmayan) filtre — liste değişince eski sonuçlar geri gelir.
    pub block_keywords: Vec<String>,
    /// Bozuk (çözülemeyen kodlama, `�` içeren) adları gizle.
    pub hide_garbled: bool,
}

impl Filter {
    /// Koşulların SQL parçası + sırayla bağlanacak parametreler. `prefix` sütun
    /// önekidir (`t.` FTS join'inde, `""` düz listede). Kategori/engel kelimeleri
    /// parametre olarak bağlanır → SQL enjeksiyonu yok.
    fn where_and_binds(&self, prefix: &str) -> (String, Vec<String>) {
        let mut sql = String::new();
        let mut binds = Vec::new();
        if self.only_alive {
            sql.push_str(&format!(" AND {prefix}peer_count > 0"));
        }
        if self.hide_adult {
            sql.push_str(&format!(" AND {prefix}category != 'adult'"));
        }
        if self.hide_garbled {
            sql.push_str(&format!(" AND {prefix}garbled = 0"));
        }
        if let Some(c) = &self.category {
            sql.push_str(&format!(" AND {prefix}category = ?"));
            binds.push(c.clone());
        }
        for kw in &self.block_keywords {
            let k = kw.trim().to_lowercase();
            if !k.is_empty() {
                sql.push_str(&format!(" AND instr(lower({prefix}name), ?) = 0"));
                binds.push(k);
            }
        }
        (sql, binds)
    }
}

/// Liste/arama sonuçlarının sıralama ölçütü (kod-kontrollü → SQL enjeksiyonu yok).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortKey {
    /// Popülerlik (seen_count) + tazelik — arama için varsayılan.
    #[default]
    Relevance,
    Name,
    /// Kategori (alfabetik) — aynı kategoriyi alt alta gruplar.
    Category,
    Size,
    Seed,
    Files,
    /// Son görülme (last_seen).
    Date,
    /// İlk keşif (first_seen).
    Added,
    Seen,
}

impl SortKey {
    /// Frontend'in gönderdiği anahtarı çözer (bilinmeyen → Relevance).
    pub fn parse(s: &str) -> Self {
        match s {
            "name" => Self::Name,
            "cat" => Self::Category,
            "size" => Self::Size,
            "seed" => Self::Seed,
            "files" => Self::Files,
            "date" => Self::Date,
            "added" => Self::Added,
            "seen" => Self::Seen,
            _ => Self::Relevance,
        }
    }

    /// `ORDER BY` gövdesi (önek + yön ile). Relevance yönü yok sayar.
    fn order_sql(self, prefix: &str, desc: bool) -> String {
        let dir = if desc { "DESC" } else { "ASC" };
        // Kararlı sıralama için ikincil anahtar olarak infohash.
        match self {
            Self::Relevance => {
                format!("{prefix}seen_count DESC, {prefix}last_seen DESC, {prefix}infohash")
            }
            Self::Name => format!("{prefix}name COLLATE NOCASE {dir}, {prefix}infohash"),
            // Kategoriye göre gruplarken ikincil anahtar ad → grup içinde de düzenli.
            Self::Category => {
                format!("{prefix}category COLLATE NOCASE {dir}, {prefix}name COLLATE NOCASE, {prefix}infohash")
            }
            Self::Size => format!("{prefix}total_size {dir}, {prefix}infohash"),
            Self::Seed => format!("{prefix}peer_count {dir}, {prefix}infohash"),
            Self::Files => format!("{prefix}file_count {dir}, {prefix}infohash"),
            Self::Date => format!("{prefix}last_seen {dir}, {prefix}infohash"),
            Self::Added => format!("{prefix}first_seen {dir}, {prefix}infohash"),
            Self::Seen => format!("{prefix}seen_count {dir}, {prefix}infohash"),
        }
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
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(opts)
            .await?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    /// Test için paylaşımlı bellek-içi (in-memory) depo.
    pub async fn in_memory() -> Result<Self, StoreError> {
        // max_connections(1): bellek-içi DB tek bağlantıya bağlıdır.
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
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
        let _ =
            sqlx::query("ALTER TABLE torrents ADD COLUMN category TEXT NOT NULL DEFAULT 'other'")
                .execute(&self.pool)
                .await;
        let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_category ON torrents(category);")
            .execute(&self.pool)
            .await;
        // Faz E: çekim zamanlayıcısı (öncelik + yeniden deneme) ve keşif zaman damgası.
        for col in [
            "ALTER TABLE torrents ADD COLUMN fetch_attempts INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE torrents ADD COLUMN last_attempt INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE torrents ADD COLUMN hot_seen INTEGER DEFAULT NULL",
            "ALTER TABLE torrents ADD COLUMN hot_count INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE torrents ADD COLUMN fetched_at INTEGER DEFAULT NULL",
            "ALTER TABLE torrents ADD COLUMN hint_peers INTEGER NOT NULL DEFAULT 0",
        ] {
            let _ = sqlx::query(col).execute(&self.pool).await;
        }
        let _ = sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_fetch_queue2 ON torrents(hint_peers DESC, seen_count DESC, last_seen DESC) WHERE metadata_status='pending';",
        )
        .execute(&self.pool)
        .await;
        // Tek seferlik onarım (şema sürümü 1): eski `unreachable` kayıtları Faz E öncesi
        // boru hattının (kısa zaman aşımı, tek deneme) kurbanıydı → 1 denemeyle geri kuyruğa.
        let ver: i64 = sqlx::query("PRAGMA user_version")
            .fetch_one(&self.pool)
            .await?
            .get(0);
        if ver < 1 {
            let r = sqlx::query(
                "UPDATE torrents SET metadata_status = 'pending', fetch_attempts = 1
                  WHERE metadata_status = 'unreachable'",
            )
            .execute(&self.pool)
            .await?;
            if r.rows_affected() > 0 {
                debug!(
                    requeued = r.rows_affected(),
                    "eski unreachable kayıtlar yeniden kuyruğa alındı"
                );
            }
            sqlx::query("PRAGMA user_version = 1")
                .execute(&self.pool)
                .await?;
        }
        // Şema sürümü 2: bozuk adlı (`�`) kayıtlar `garbled=1` işaretlenir — hem UI'da
        // gizlenebilir hem de yeniden çekim kuyruğuna girer (ad kodlaması artık
        // GBK/SJIS/CP1251 tanıyor). Kayıt `fetched` kalır (aranabilir).
        let _ = sqlx::query("ALTER TABLE torrents ADD COLUMN garbled INTEGER NOT NULL DEFAULT 0")
            .execute(&self.pool)
            .await;
        if ver < 2 {
            let r = sqlx::query(
                "UPDATE torrents SET garbled = 1, fetch_attempts = 0
                  WHERE metadata_status = 'fetched' AND instr(name, char(65533)) > 0",
            )
            .execute(&self.pool)
            .await?;
            if r.rows_affected() > 0 {
                debug!(
                    garbled = r.rows_affected(),
                    "bozuk adlı kayıtlar işaretlendi (yeniden çekim)"
                );
            }
            sqlx::query("PRAGMA user_version = 2")
                .execute(&self.pool)
                .await?;
        }
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
        // Faz D: nicemlenmiş embedding vektörleri (int8 + ölçek). model_id ile izlenir;
        // model/kademe değişince başka model_id'li satırlar silinir. Açılışta RAM'e yüklenir.
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS torrent_embeddings (
                infohash TEXT PRIMARY KEY REFERENCES torrents(infohash) ON DELETE CASCADE,
                model_id TEXT NOT NULL,
                dim      INTEGER NOT NULL,
                scale    REAL NOT NULL,
                q        BLOB NOT NULL
            );"#,
        )
        .execute(&self.pool)
        .await?;
        let _ = sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_emb_model ON torrent_embeddings(model_id);",
        )
        .execute(&self.pool)
        .await;
        Ok(())
    }

    /// Harvester yolu: bir infohash görüldüğünde çağrılır. Yeniyse `pending` bir
    /// iskelet satır açar; varsa `last_seen`/`seen_count` günceller. `hot` = pasif
    /// trafikten (get_peers/announce) geldi → `hot_seen`/`hot_count` (çekim önceliği).
    /// Metadata'ya dokunmaz. Kaydın **güncel metadata_status**'ünü döner.
    pub async fn record_sighting(&self, infohash: InfoHash, ts: i64) -> Result<String, StoreError> {
        self.record_sighting_ext(infohash, ts, false).await
    }

    /// [`Store::record_sighting`] — kaynak bilgisiyle. `hint_peers`: bu sighting'le gelen
    /// doğrudan peer sayısı (BEP-51 takip get_peers `values`) — seeder vekili, çekim önceliği.
    pub async fn record_sighting_ext(
        &self,
        infohash: InfoHash,
        ts: i64,
        hot: bool,
    ) -> Result<String, StoreError> {
        self.record_sighting_full(infohash, ts, hot, 0, 1).await
    }

    /// Tam sürüm: `hint_peers` (0 = bilgi yok; MAX ile birleşir) ve `repeats` (kaç görülme
    /// sayılacak; harvester tekrar sayacı flush'ında >1).
    pub async fn record_sighting_full(
        &self,
        infohash: InfoHash,
        ts: i64,
        hot: bool,
        hint_peers: i64,
        repeats: i64,
    ) -> Result<String, StoreError> {
        let hex = infohash.to_hex();
        let hot_i = if hot { 1i64 } else { 0 };
        let repeats = repeats.max(1);
        let row = sqlx::query(
            r#"INSERT INTO torrents
                 (infohash, name, total_size, file_count, first_seen, last_seen, seen_count, metadata_status,
                  hot_seen, hot_count, hint_peers)
               VALUES (?1, '', 0, 0, ?2, ?2, ?5, 'pending', CASE WHEN ?3 = 1 THEN ?2 ELSE NULL END, ?3, ?4)
               ON CONFLICT(infohash) DO UPDATE SET
                 last_seen  = MAX(last_seen, excluded.last_seen),
                 seen_count = seen_count + ?5,
                 hot_seen   = CASE WHEN ?3 = 1 THEN excluded.last_seen ELSE hot_seen END,
                 hot_count  = hot_count + ?3,
                 hint_peers = MAX(hint_peers, ?4)
               RETURNING metadata_status;"#,
        )
        .bind(&hex)
        .bind(ts)
        .bind(hot_i)
        .bind(hint_peers.max(0))
        .bind(repeats)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get::<String, _>("metadata_status"))
    }

    /// Çekim zamanlayıcısı için sıradaki adaylar (öncelikli kuyruk): `pending` olup
    /// hiç denenmemiş ya da soğuma süresi dolmuş (en fazla `MAX_FETCH_ATTEMPTS` deneme)
    /// kayıtlar; **sıcak** (yakın zamanda pasif trafikte görülen) > **popüler**
    /// (`seen_count`) > **taze** (`last_seen`). Seçilenlerin `last_attempt`'ı hemen
    /// `now` yapılır ki eşzamanlı çağrılar aynı adayları almasın.
    pub async fn next_to_fetch(&self, limit: i64, now: i64) -> Result<Vec<InfoHash>, StoreError> {
        let cooldown = now - FETCH_RETRY_COOLDOWN_SECS;
        let hot_window = now - HOT_WINDOW_SECS;
        let rows = sqlx::query(
            "SELECT infohash FROM torrents
              WHERE (metadata_status = 'pending'
                     AND (fetch_attempts = 0 OR (fetch_attempts < ?1 AND last_attempt < ?2)))
                 OR (metadata_status = 'fetched' AND garbled = 1 AND fetch_attempts = 0)
              ORDER BY hint_peers DESC,
                       (hot_seen IS NOT NULL AND hot_seen > ?3) DESC,
                       seen_count DESC, hot_count DESC, last_seen DESC
              LIMIT ?4",
        )
        .bind(MAX_FETCH_ATTEMPTS)
        .bind(cooldown)
        .bind(hot_window)
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
        if !out.is_empty() {
            let placeholders = std::iter::repeat_n("?", out.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "UPDATE torrents SET last_attempt = ?, fetch_attempts = fetch_attempts + 1
                  WHERE infohash IN ({placeholders})"
            );
            let mut q = sqlx::query(&sql).bind(now);
            for ih in &out {
                q = q.bind(ih.to_hex());
            }
            q.execute(&self.pool).await?;
        }
        Ok(out)
    }

    /// Çekim başarısızlığını işler: deneme sınırına ulaşan `pending` kayıt
    /// `unreachable` olur (kalıcı); değilse soğuma sonrası yeniden denenmek üzere
    /// `pending` kalır. (Deneme sayacı `next_to_fetch` seçiminde artırılır.)
    pub async fn mark_fetch_failed(&self, infohash: InfoHash) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE torrents SET metadata_status = 'unreachable'
              WHERE infohash = ?1 AND metadata_status = 'pending' AND fetch_attempts >= ?2",
        )
        .bind(infohash.to_hex())
        .bind(MAX_FETCH_ATTEMPTS)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Çekim kuyruğu istatistikleri: (pending, sıcak-pending, unreachable, son 1 saatte fetched).
    pub async fn fetch_queue_stats(&self, now: i64) -> Result<(i64, i64, i64, i64), StoreError> {
        let r = sqlx::query(
            "SELECT
               SUM(metadata_status='pending') AS pending,
               SUM(metadata_status='pending' AND hot_seen > ?1) AS hot,
               SUM(metadata_status='unreachable') AS unreachable,
               SUM(metadata_status='fetched' AND fetched_at > ?2) AS recent
             FROM torrents",
        )
        .bind(now - HOT_WINDOW_SECS)
        .bind(now - 3600)
        .fetch_one(&self.pool)
        .await?;
        Ok((
            r.get::<Option<i64>, _>("pending").unwrap_or(0),
            r.get::<Option<i64>, _>("hot").unwrap_or(0),
            r.get::<Option<i64>, _>("unreachable").unwrap_or(0),
            r.get::<Option<i64>, _>("recent").unwrap_or(0),
        ))
    }

    /// Fetcher yolu: çekilmiş metadata'yı yazar. Idempotent — tekrar çağrılırsa
    /// alanları tazeler, `seen_count`'u artırır, dosya listesini ve FTS'i yeniler.
    pub async fn upsert_torrent(&self, rec: &TorrentRecord) -> Result<(), StoreError> {
        let hex = rec.infohash.to_hex();
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            r#"INSERT INTO torrents
                 (infohash, name, total_size, file_count, first_seen, last_seen, seen_count, metadata_status, category, fetched_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'fetched', ?8, ?6)
               ON CONFLICT(infohash) DO UPDATE SET
                 fetched_at      = CASE WHEN metadata_status != 'fetched' THEN excluded.last_seen ELSE fetched_at END,
                 name            = excluded.name,
                 total_size      = excluded.total_size,
                 file_count      = excluded.file_count,
                 first_seen      = MIN(first_seen, excluded.first_seen),
                 last_seen       = MAX(last_seen, excluded.last_seen),
                 seen_count      = seen_count + 1,
                 metadata_status = 'fetched',
                 category        = excluded.category,
                 garbled         = (instr(excluded.name, char(65533)) > 0);"#,
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
    /// Geriye dönük sarmalayıcı: [`Store::search_paged`]'i varsayılan sıra/offset ile çağırır.
    pub async fn search(
        &self,
        query: &str,
        limit: i64,
        filter: &Filter,
    ) -> Result<Vec<TorrentSummary>, StoreError> {
        self.search_paged(query, limit, 0, SortKey::Relevance, true, filter)
            .await
    }

    /// FTS5 araması — sıralama + sayfalama (`offset`) ile. Sonsuz-scroll/sayfalı UI için.
    pub async fn search_paged(
        &self,
        query: &str,
        limit: i64,
        offset: i64,
        sort: SortKey,
        desc: bool,
        filter: &Filter,
    ) -> Result<Vec<TorrentSummary>, StoreError> {
        let match_query = to_fts_query(query);
        if match_query.is_empty() {
            return Ok(Vec::new());
        }
        let (fsql, fbinds) = filter.where_and_binds("t.");
        let sql = format!(
            "SELECT t.infohash, t.name, t.total_size, t.file_count, t.seen_count,
                    t.first_seen, t.last_seen, t.peer_count, t.last_check, t.category
               FROM torrents_fts f JOIN torrents t ON t.infohash = f.infohash
              WHERE f.name MATCH ? AND t.metadata_status = 'fetched'{fsql}
              ORDER BY {order}
              LIMIT ? OFFSET ?",
            order = sort.order_sql("t.", desc),
        );
        let mut q = sqlx::query(&sql).bind(&match_query);
        for b in fbinds {
            q = q.bind(b);
        }
        let rows = q
            .bind(limit.max(0))
            .bind(offset.max(0))
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(row_to_summary).collect()
    }

    /// Sorgusuz gözatma: FTS olmadan tüm (çekilmiş) torrent'leri sıralama +
    /// sayfalama + filtreyle listeler. Boş sorguda "gözat" tablosunu besler.
    pub async fn list_paged(
        &self,
        limit: i64,
        offset: i64,
        sort: SortKey,
        desc: bool,
        filter: &Filter,
    ) -> Result<Vec<TorrentSummary>, StoreError> {
        let (fsql, fbinds) = filter.where_and_binds("");
        let sql = format!(
            "SELECT infohash, name, total_size, file_count, seen_count, first_seen, last_seen,
                    peer_count, last_check, category
               FROM torrents WHERE metadata_status = 'fetched'{fsql}
              ORDER BY {order}
              LIMIT ? OFFSET ?",
            order = sort.order_sql("", desc),
        );
        let mut q = sqlx::query(&sql);
        for b in fbinds {
            q = q.bind(b);
        }
        let rows = q
            .bind(limit.max(0))
            .bind(offset.max(0))
            .fetch_all(&self.pool)
            .await?;
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
    /// Geriye dönük sarmalayıcı — [`Store::discovery`]'yi saatlik kovayla çağırır.
    pub async fn hourly_discovery(&self, hours: i64) -> Result<Vec<(i64, i64)>, StoreError> {
        self.discovery(3600, hours).await
    }

    /// Zaman serisi keşif sayıları (grafik): `bucket_secs` kova genişliği (saat=3600,
    /// gün=86400), **şimdiden geriye tam `points` kova** — boş kovalar 0 ile doldurulur
    /// (aksi hâlde "son 48 saat" grafiği yalnız dolu saatleri gösterip günlere yayılıyordu).
    /// En yeni önce: `(kova_başı_unix, sayı)`.
    pub async fn discovery(
        &self,
        bucket_secs: i64,
        points: i64,
    ) -> Result<Vec<(i64, i64)>, StoreError> {
        let bucket = bucket_secs.max(1);
        let points = points.max(1);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let newest = (now / bucket) * bucket;
        let oldest = newest - (points - 1) * bucket;
        let rows = sqlx::query(
            "SELECT (first_seen / ?1) * ?1 AS bkt, COUNT(*) AS n
               FROM torrents WHERE metadata_status = 'fetched' AND first_seen >= ?2
              GROUP BY bkt",
        )
        .bind(bucket)
        .bind(oldest)
        .fetch_all(&self.pool)
        .await?;
        let counts: std::collections::HashMap<i64, i64> = rows
            .iter()
            .map(|r| (r.get::<i64, _>("bkt"), r.get::<i64, _>("n")))
            .collect();
        Ok((0..points)
            .map(|i| {
                let t = newest - i * bucket;
                (t, counts.get(&t).copied().unwrap_or(0))
            })
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
    pub async fn top_by_seen(
        &self,
        limit: i64,
        filter: &Filter,
    ) -> Result<Vec<TorrentSummary>, StoreError> {
        self.list_paged(limit, 0, SortKey::Seen, true, filter).await
    }

    /// Boyuta göre en büyük torrent'ler (dashboard).
    pub async fn top_by_size(
        &self,
        limit: i64,
        filter: &Filter,
    ) -> Result<Vec<TorrentSummary>, StoreError> {
        self.list_paged(limit, 0, SortKey::Size, true, filter).await
    }

    /// En son indekslenen torrent'ler (dashboard).
    pub async fn recent(
        &self,
        limit: i64,
        filter: &Filter,
    ) -> Result<Vec<TorrentSummary>, StoreError> {
        self.list_paged(limit, 0, SortKey::Added, true, filter)
            .await
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
        let row =
            sqlx::query("SELECT COUNT(*) AS n FROM torrents WHERE metadata_status = 'fetched'")
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

    // ------------------------------------------------------------------
    // Faz D — semantik indeks kalıcılığı + hibrit arama
    // ------------------------------------------------------------------

    /// Bu model için henüz embed edilmemiş (metadata'sı çekilmiş) torrent'ler:
    /// `(infohash, name)`. En yeni keşfedilenler önce (kullanıcı taze içeriği hemen bulsun).
    pub async fn embed_backlog(
        &self,
        model_id: &str,
        limit: i64,
    ) -> Result<Vec<(InfoHash, String)>, StoreError> {
        let rows = sqlx::query(
            "SELECT t.infohash, t.name FROM torrents t
               LEFT JOIN torrent_embeddings e ON e.infohash = t.infohash AND e.model_id = ?1
              WHERE t.metadata_status = 'fetched' AND e.infohash IS NULL
                AND instr(t.name, char(65533)) = 0 AND length(t.name) >= 2
              ORDER BY t.first_seen DESC LIMIT ?2",
        )
        .bind(model_id)
        .bind(limit.max(0))
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let hex: String = r.get("infohash");
            if let Some(ih) = InfoHash::from_hex(&hex) {
                out.push((ih, r.get::<String, _>("name")));
            }
        }
        Ok(out)
    }

    /// Nicemlenmiş vektörleri yazar (tek işlem; var olanın üzerine yazar).
    pub async fn insert_embeddings(
        &self,
        model_id: &str,
        rows: &[(InfoHash, Vec<i8>, f32)],
    ) -> Result<(), StoreError> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        for (ih, q, scale) in rows {
            // i8 → u8 bayt görünümü (bire bir bit kopyası).
            let blob: Vec<u8> = q.iter().map(|&x| x as u8).collect();
            sqlx::query(
                "INSERT INTO torrent_embeddings (infohash, model_id, dim, scale, q) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(infohash) DO UPDATE SET model_id = excluded.model_id, dim = excluded.dim,
                                                     scale = excluded.scale, q = excluded.q",
            )
            .bind(ih.to_hex())
            .bind(model_id)
            .bind(q.len() as i64)
            .bind(*scale as f64)
            .bind(blob)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Embedding satırlarını sayfa sayfa okur (açılışta RAM'e yükleme): `rowid > after`
    /// olan ilk `limit` satır, `(rowid, infohash, q_int8, scale)`. Boş dönerse bitmiştir.
    pub async fn load_embeddings_page(
        &self,
        model_id: &str,
        after_rowid: i64,
        limit: i64,
    ) -> Result<Vec<(i64, InfoHash, Vec<i8>, f32)>, StoreError> {
        let rows = sqlx::query(
            "SELECT rowid, infohash, scale, q FROM torrent_embeddings
              WHERE model_id = ?1 AND rowid > ?2 ORDER BY rowid LIMIT ?3",
        )
        .bind(model_id)
        .bind(after_rowid)
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let hex: String = r.get("infohash");
            let Some(ih) = InfoHash::from_hex(&hex) else {
                continue;
            };
            let blob: Vec<u8> = r.get("q");
            let q: Vec<i8> = blob.into_iter().map(|b| b as i8).collect();
            out.push((
                r.get::<i64, _>("rowid"),
                ih,
                q,
                r.get::<f64, _>("scale") as f32,
            ));
        }
        Ok(out)
    }

    /// Bu modele ait embedding sayısı.
    pub async fn count_embeddings(&self, model_id: &str) -> Result<i64, StoreError> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM torrent_embeddings WHERE model_id = ?1")
            .bind(model_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get::<i64, _>("n"))
    }

    /// Verilen model dışındaki tüm embedding'leri siler (kademe/model değişimi).
    /// Silinen satır sayısını döner.
    pub async fn reset_embeddings_except(&self, keep_model_id: &str) -> Result<u64, StoreError> {
        let r = sqlx::query("DELETE FROM torrent_embeddings WHERE model_id != ?1")
            .bind(keep_model_id)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected())
    }

    /// Hibrit arama: FTS adayları + çağıranın verdiği semantik aday sırası (bellek-içi
    /// indeksten, en yakından uzağa) RRF ile harmanlanır; filtre uygulanır; sayfalanır.
    /// `sort == Relevance` → harman sırası; diğer anahtarlarda birleşik aday kümesi
    /// istenen anahtarla sıralanır. Semantik liste boşsa saf FTS'e denk düşer.
    // Arama parametreleri `search_paged` ile simetrik (mevcut çağıran sözleşmesi).
    #[allow(clippy::too_many_arguments)]
    pub async fn search_hybrid_paged(
        &self,
        query: &str,
        semantic: &[InfoHash],
        limit: i64,
        offset: i64,
        sort: SortKey,
        desc: bool,
        filter: &Filter,
    ) -> Result<Vec<TorrentSummary>, StoreError> {
        // 1) FTS adayları (filtreli, popülerlik sırasıyla) — en fazla HYBRID_CANDIDATES.
        let match_query = to_fts_query(query);
        let fts_ids: Vec<InfoHash> = if match_query.is_empty() {
            Vec::new()
        } else {
            let (fsql, fbinds) = filter.where_and_binds("t.");
            let sql = format!(
                "SELECT t.infohash FROM torrents_fts f JOIN torrents t ON t.infohash = f.infohash
                  WHERE f.name MATCH ? AND t.metadata_status = 'fetched'{fsql}
                  ORDER BY {order} LIMIT ?",
                order = SortKey::Relevance.order_sql("t.", true),
            );
            let mut q = sqlx::query(&sql).bind(&match_query);
            for b in fbinds {
                q = q.bind(b);
            }
            q.bind(HYBRID_CANDIDATES)
                .fetch_all(&self.pool)
                .await?
                .iter()
                .filter_map(|r| InfoHash::from_hex(&r.get::<String, _>("infohash")))
                .collect()
        };
        if fts_ids.is_empty() && semantic.is_empty() {
            return Ok(Vec::new());
        }
        // 2) RRF harmanı.
        let fused = dragnet_core::rank::rrf(
            &[fts_ids, semantic.to_vec()],
            &[],
            dragnet_core::rank::RRF_K,
        );
        let ordered: Vec<InfoHash> = fused.into_iter().map(|(ih, _)| ih).collect();

        // 3) Aday satırlarını çek (filtre burada semantik-yalnız adaylara da uygulanır).
        let (fsql, fbinds) = filter.where_and_binds("");
        let placeholders = std::iter::repeat_n("?", ordered.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT infohash, name, total_size, file_count, seen_count, first_seen, last_seen,
                    peer_count, last_check, category
               FROM torrents WHERE metadata_status = 'fetched' AND infohash IN ({placeholders}){fsql}"
        );
        let mut q = sqlx::query(&sql);
        for ih in &ordered {
            q = q.bind(ih.to_hex());
        }
        for b in fbinds {
            q = q.bind(b);
        }
        let rows = q.fetch_all(&self.pool).await?;
        let mut by_id: std::collections::HashMap<InfoHash, TorrentSummary> =
            std::collections::HashMap::with_capacity(rows.len());
        for r in &rows {
            let s = row_to_summary(r)?;
            by_id.insert(s.infohash, s);
        }
        // 4) Sırala + sayfala.
        let mut items: Vec<TorrentSummary> =
            ordered.iter().filter_map(|ih| by_id.remove(ih)).collect();
        if !matches!(sort, SortKey::Relevance) {
            sort_summaries(&mut items, sort, desc);
        }
        let start = (offset.max(0) as usize).min(items.len());
        let end = (start + limit.max(0) as usize).min(items.len());
        Ok(items[start..end].to_vec())
    }
}

/// Bir infohash için en fazla metadata çekim denemesi; sonra kalıcı `unreachable`.
pub const MAX_FETCH_ATTEMPTS: i64 = 3;
/// Başarısız denemeler arası soğuma (sn) — peer'ler zamanla değişir, tekrar denemeye değer.
pub const FETCH_RETRY_COOLDOWN_SECS: i64 = 6 * 3600;
/// "Sıcak" sayılma penceresi (sn): bu süre içinde pasif trafikte görülen infohash öncelikli.
pub const HOT_WINDOW_SECS: i64 = 2 * 3600;

/// Hibrit aramada FTS tarafından alınacak en fazla aday sayısı (semantik taraf da
/// çağıran tarafından benzer sayıda verilir; RRF sonrası sayfalama bunun içinde gezer).
pub const HYBRID_CANDIDATES: i64 = 400;

/// Bellek-içi özet listesini `SortKey`'e göre sıralar (hibrit aday kümesi için —
/// SQL `order_sql` ile aynı anlambilim; Relevance burada dokunulmaz).
fn sort_summaries(items: &mut [TorrentSummary], sort: SortKey, desc: bool) {
    use std::cmp::Ordering;
    let cmp = |a: &TorrentSummary, b: &TorrentSummary| -> Ordering {
        let o = match sort {
            SortKey::Relevance => Ordering::Equal,
            SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortKey::Category => a
                .category
                .to_lowercase()
                .cmp(&b.category.to_lowercase())
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
            SortKey::Size => a.total_size.cmp(&b.total_size),
            SortKey::Seed => a.peer_count.cmp(&b.peer_count),
            SortKey::Files => a.file_count.cmp(&b.file_count),
            SortKey::Date => a.last_seen.cmp(&b.last_seen),
            SortKey::Added => a.first_seen.cmp(&b.first_seen),
            SortKey::Seen => a.seen_count.cmp(&b.seen_count),
        };
        let o = if desc { o.reverse() } else { o };
        o.then_with(|| a.infohash.to_hex().cmp(&b.infohash.to_hex()))
    };
    items.sort_by(cmp);
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
        let rec = record(
            "0123456789abcdef0123456789abcdef01234567",
            "Ubuntu 24.04",
            4096,
        );
        store.upsert_torrent(&rec).await.unwrap();

        let got = store
            .get(rec.infohash)
            .await
            .unwrap()
            .expect("kayıt olmalı");
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

        let results = store
            .search("ubuntu", 10, &Filter::default())
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "Ubuntu 24.04 Desktop amd64");

        // Önek araması: "ubun" da eşleşmeli.
        assert_eq!(
            store
                .search("ubun", 10, &Filter::default())
                .await
                .unwrap()
                .len(),
            1
        );
        // Alakasız sorgu boş dönmeli.
        assert!(store
            .search("archlinux", 10, &Filter::default())
            .await
            .unwrap()
            .is_empty());
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
        assert!(store
            .search("slackware", 10, &Filter::default())
            .await
            .unwrap()
            .is_empty());

        // Sonra metadata gelir → aranabilir olur, sighting artık 'fetched' döner.
        let mut rec = record(
            "3333333333333333333333333333333333333333",
            "Slackware 15",
            900,
        );
        rec.first_seen = 500;
        store.upsert_torrent(&rec).await.unwrap();
        assert_eq!(store.count_fetched().await.unwrap(), 1);
        assert_eq!(store.record_sighting(ih, 950).await.unwrap(), "fetched");
        let hits = store
            .search("slackware", 10, &Filter::default())
            .await
            .unwrap();
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
        let mut b = record(
            "2222222222222222222222222222222222222222",
            "Beta",
            9_000_000,
        );
        b.seen_count = 2;
        store.upsert_torrent(&a).await.unwrap();
        store.upsert_torrent(&b).await.unwrap();

        let by_seen = store.top_by_seen(10, &Filter::default()).await.unwrap();
        assert_eq!(by_seen[0].name, "Alpha"); // en çok görülen önce

        let by_size = store.top_by_size(10, &Filter::default()).await.unwrap();
        assert_eq!(by_size[0].name, "Beta"); // en büyük önce

        assert_eq!(store.recent(10, &Filter::default()).await.unwrap().len(), 2);
        // Saatlik keşif: kayıtlar 1970'te (first_seen=1000) → son 48 saat penceresinin
        // DIŞINDA; seri yine tam 48 bitişik kova (0 dolu) döner.
        let hourly = store.hourly_discovery(48).await.unwrap();
        assert_eq!(hourly.len(), 48);
        assert_eq!(hourly.iter().map(|(_, n)| n).sum::<i64>(), 0);
        assert!(
            hourly.windows(2).all(|w| w[0].0 - w[1].0 == 3600),
            "kovalar bitişik olmalı"
        );
        // Şimdi görülen bir kayıt en yeni kovaya düşer.
        let mut c = record("3333333333333333333333333333333333333333", "Gamma", 1);
        c.first_seen = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        c.last_seen = c.first_seen;
        store.upsert_torrent(&c).await.unwrap();
        let hourly = store.hourly_discovery(48).await.unwrap();
        assert_eq!(
            hourly[0].1,
            1,
            "en yeni kova (şimdi) 1 olmalı: {:?}",
            &hourly[..3]
        );
    }

    #[tokio::test]
    async fn block_keywords_hide_matching_names() {
        let store = Store::in_memory().await.unwrap();
        store
            .upsert_torrent(&record(
                "1111111111111111111111111111111111111111",
                "Clean Movie 1080p",
                1,
            ))
            .await
            .unwrap();
        store
            .upsert_torrent(&record(
                "2222222222222222222222222222222222222222",
                "Some CAM rip",
                1,
            ))
            .await
            .unwrap();

        // Filtresiz: ikisi de gözat listesinde.
        let all = store
            .list_paged(10, 0, SortKey::Name, false, &Filter::default())
            .await
            .unwrap();
        assert_eq!(all.len(), 2);

        // "cam" engellenince yalnız temiz kayıt kalır (küçük harfe duyarsız).
        let f = Filter {
            block_keywords: vec!["cam".into()],
            ..Default::default()
        };
        let filtered = store
            .list_paged(10, 0, SortKey::Name, false, &f)
            .await
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "Clean Movie 1080p");

        // Aramada da uygulanır.
        let s = store
            .search_paged("rip", 10, 0, SortKey::Relevance, true, &f)
            .await
            .unwrap();
        assert!(s.is_empty());
    }

    #[tokio::test]
    async fn list_paged_offset_and_sort() {
        let store = Store::in_memory().await.unwrap();
        for (i, (hex, name, size)) in [
            ("1111111111111111111111111111111111111111", "Alpha", 300u64),
            ("2222222222222222222222222222222222222222", "Bravo", 100),
            ("3333333333333333333333333333333333333333", "Charlie", 200),
        ]
        .into_iter()
        .enumerate()
        {
            let mut r = record(hex, name, size);
            r.first_seen = 1000 + i as i64;
            store.upsert_torrent(&r).await.unwrap();
        }
        // Ada göre artan sırala + sayfalama.
        let p1 = store
            .list_paged(2, 0, SortKey::Name, false, &Filter::default())
            .await
            .unwrap();
        assert_eq!(
            p1.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["Alpha", "Bravo"]
        );
        let p2 = store
            .list_paged(2, 2, SortKey::Name, false, &Filter::default())
            .await
            .unwrap();
        assert_eq!(
            p2.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["Charlie"]
        );
        // Boyuta göre azalan.
        let by_size = store
            .list_paged(10, 0, SortKey::Size, true, &Filter::default())
            .await
            .unwrap();
        assert_eq!(by_size[0].name, "Alpha"); // 300 en büyük
                                              // Gün kovası: 1970 tarihli kayıtlar pencere dışında; seri tam 30 kova.
        let daily = store.discovery(86_400, 30).await.unwrap();
        assert_eq!(daily.len(), 30);
        assert_eq!(daily.iter().map(|(_, n)| n).sum::<i64>(), 0);
    }

    #[test]
    fn sort_key_parse_defaults_to_relevance() {
        assert_eq!(SortKey::parse("size"), SortKey::Size);
        assert_eq!(SortKey::parse("added"), SortKey::Added);
        assert_eq!(SortKey::parse("cat"), SortKey::Category);
        assert_eq!(SortKey::parse("bogus"), SortKey::Relevance);
    }

    #[tokio::test]
    async fn sort_by_category_groups_alphabetically() {
        let store = Store::in_memory().await.unwrap();
        // Kategorileri belli olan kayıtlar (categorize heuristiği ada bakar).
        store
            .upsert_torrent(&record(
                "1111111111111111111111111111111111111111",
                "Ubuntu 24.04 amd64.iso",
                1,
            ))
            .await
            .unwrap();
        store
            .upsert_torrent(&record(
                "2222222222222222222222222222222222222222",
                "Song - Album FLAC",
                1,
            ))
            .await
            .unwrap();
        store
            .upsert_torrent(&record(
                "3333333333333333333333333333333333333333",
                "Movie 1080p x264.mkv",
                1,
            ))
            .await
            .unwrap();

        let rows = store
            .list_paged(10, 0, SortKey::Category, false, &Filter::default())
            .await
            .unwrap();
        let cats: Vec<&str> = rows.iter().map(|r| r.category.as_str()).collect();
        // Alfabetik artan → aynı kategoriler bitişik ve sıralı.
        let mut sorted = cats.clone();
        sorted.sort();
        assert_eq!(cats, sorted, "kategoriler alfabetik gruplanmalı");
    }

    #[tokio::test]
    async fn embeddings_persist_page_and_reset() {
        let store = Store::in_memory().await.unwrap();
        for (hex, name) in [
            ("1111111111111111111111111111111111111111", "Alpha"),
            ("2222222222222222222222222222222222222222", "Bravo"),
            ("3333333333333333333333333333333333333333", "Charlie"),
        ] {
            store.upsert_torrent(&record(hex, name, 1)).await.unwrap();
        }
        let backlog = store.embed_backlog("m1", 10).await.unwrap();
        assert_eq!(backlog.len(), 3);
        assert!(backlog.iter().any(|(_, n)| n == "Bravo"));

        let a = InfoHash::from_hex("1111111111111111111111111111111111111111").unwrap();
        let b = InfoHash::from_hex("2222222222222222222222222222222222222222").unwrap();
        store
            .insert_embeddings(
                "m1",
                &[
                    (a, vec![1i8, -2, 3, 127], 0.5),
                    (b, vec![-128i8, 0, 0, 1], 0.25),
                ],
            )
            .await
            .unwrap();
        assert_eq!(store.count_embeddings("m1").await.unwrap(), 2);
        assert_eq!(store.embed_backlog("m1", 10).await.unwrap().len(), 1);
        assert_eq!(store.embed_backlog("m2", 10).await.unwrap().len(), 3);

        // Sayfalı yükleme: 1'erli; int8 bit-kopyası kayıpsız (−128/127 dahil).
        let p1 = store.load_embeddings_page("m1", 0, 1).await.unwrap();
        assert_eq!(p1.len(), 1);
        let p2 = store.load_embeddings_page("m1", p1[0].0, 1).await.unwrap();
        assert_eq!(p2.len(), 1);
        assert!(store
            .load_embeddings_page("m1", p2[0].0, 1)
            .await
            .unwrap()
            .is_empty());
        let mut all: Vec<_> = p1
            .into_iter()
            .chain(p2)
            .map(|(_, ih, q, s)| (ih, q, s))
            .collect();
        all.sort_by_key(|x| x.0.to_hex());
        assert_eq!(all[0], (a, vec![1i8, -2, 3, 127], 0.5));
        assert_eq!(all[1], (b, vec![-128i8, 0, 0, 1], 0.25));

        // Üzerine yazma + model değişimi.
        store
            .insert_embeddings("m2", &[(a, vec![9i8, 9, 9, 9], 1.0)])
            .await
            .unwrap();
        assert_eq!(store.count_embeddings("m1").await.unwrap(), 1);
        assert_eq!(store.reset_embeddings_except("m2").await.unwrap(), 1);
        assert_eq!(store.count_embeddings("m1").await.unwrap(), 0);
        assert_eq!(store.count_embeddings("m2").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn hybrid_search_fuses_filters_and_pages() {
        let store = Store::in_memory().await.unwrap();
        let mut r1 = record(
            "1111111111111111111111111111111111111111",
            "The Matrix Reloaded 2003 1080p",
            100,
        );
        r1.seen_count = 5;
        let mut r2 = record(
            "2222222222222222222222222222222222222222",
            "Matrix Revolutions 2003 720p",
            200,
        );
        r2.seen_count = 50;
        let r3 = record(
            "3333333333333333333333333333333333333333",
            "Matriks Filmi TR Dublaj",
            300,
        );
        let r4 = record(
            "4444444444444444444444444444444444444444",
            "Some CAM rip",
            400,
        );
        for r in [&r1, &r2, &r3, &r4] {
            store.upsert_torrent(r).await.unwrap();
        }
        let ih = |h: &str| InfoHash::from_hex(h).unwrap();
        // Sahte semantik sıra: 3 (FTS'in "matrix" ile bulamadığı), sonra 1.
        let sem = vec![
            ih("3333333333333333333333333333333333333333"),
            ih("1111111111111111111111111111111111111111"),
        ];

        let fts = store
            .search_paged(
                "matrix",
                10,
                0,
                SortKey::Relevance,
                true,
                &Filter::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            fts.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            [
                "Matrix Revolutions 2003 720p",
                "The Matrix Reloaded 2003 1080p"
            ]
        );

        // Hibrit: 1 iki listede de → ilk; 3 semantikten gelir; 4 gelmez.
        let hy = store
            .search_hybrid_paged(
                "matrix",
                &sem,
                10,
                0,
                SortKey::Relevance,
                true,
                &Filter::default(),
            )
            .await
            .unwrap();
        let names: Vec<&str> = hy.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names[0], "The Matrix Reloaded 2003 1080p", "{names:?}");
        assert!(names.contains(&"Matriks Filmi TR Dublaj"));
        assert_eq!(names.len(), 3);

        // Semantik boş → FTS kümesi.
        assert_eq!(
            store
                .search_hybrid_paged(
                    "matrix",
                    &[],
                    10,
                    0,
                    SortKey::Relevance,
                    true,
                    &Filter::default()
                )
                .await
                .unwrap()
                .len(),
            2
        );
        // FTS'in hiç eşleşmediği sorgu → yalnız semantik adaylar.
        let only_sem = store
            .search_hybrid_paged(
                "zzqq",
                &sem,
                10,
                0,
                SortKey::Relevance,
                true,
                &Filter::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            only_sem.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["Matriks Filmi TR Dublaj", "The Matrix Reloaded 2003 1080p"]
        );
        // İkisi de boş → boş.
        assert!(store
            .search_hybrid_paged(
                "zzqq",
                &[],
                10,
                0,
                SortKey::Relevance,
                true,
                &Filter::default()
            )
            .await
            .unwrap()
            .is_empty());

        // Filtre semantik-yalnız adaya da uygulanır.
        let f = Filter {
            block_keywords: vec!["dublaj".into()],
            ..Default::default()
        };
        let filtered = store
            .search_hybrid_paged("matrix", &sem, 10, 0, SortKey::Relevance, true, &f)
            .await
            .unwrap();
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|s| !s.name.contains("Dublaj")));

        // Sayfalama.
        let page = store
            .search_hybrid_paged(
                "matrix",
                &sem,
                1,
                1,
                SortKey::Relevance,
                true,
                &Filter::default(),
            )
            .await
            .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].name, hy[1].name);

        // Boyuta göre azalan sıralama aday kümesine uygulanır.
        let by_size = store
            .search_hybrid_paged(
                "matrix",
                &sem,
                10,
                0,
                SortKey::Size,
                true,
                &Filter::default(),
            )
            .await
            .unwrap();
        assert_eq!(by_size[0].name, "Matriks Filmi TR Dublaj");
        assert_eq!(by_size[2].name, "The Matrix Reloaded 2003 1080p");
    }

    #[tokio::test]
    async fn fetch_queue_prioritizes_hot_then_popular_and_retries_with_cooldown() {
        let store = Store::in_memory().await.unwrap();
        let ih = |n: u8| InfoHash::from_bytes([n; 20]);
        let now = 1_000_000i64;
        // A: 5 kez görülmüş (popüler); B: 1 kez ama sıcak; C: 1 kez soğuk; D: fetched.
        for _ in 0..5 {
            store
                .record_sighting_ext(ih(1), now - 100, false)
                .await
                .unwrap();
        }
        store
            .record_sighting_ext(ih(2), now - 50, true)
            .await
            .unwrap();
        store
            .record_sighting_ext(ih(3), now - 10, false)
            .await
            .unwrap();
        store
            .upsert_torrent(&record(
                "0404040404040404040404040404040404040404",
                "Done",
                1,
            ))
            .await
            .unwrap();

        // Sıra: sıcak (B) > popüler (A) > taze-soğuk (C); D (fetched) yok.
        let q = store.next_to_fetch(10, now).await.unwrap();
        assert_eq!(q, vec![ih(2), ih(1), ih(3)]);
        // Seçilenler işaretlendi → hemen tekrar sorulunca gelmezler (soğuma).
        assert!(store.next_to_fetch(10, now).await.unwrap().is_empty());
        // Soğuma dolunca tekrar gelirler (deneme 2).
        let later = now + FETCH_RETRY_COOLDOWN_SECS + 1;
        assert_eq!(store.next_to_fetch(10, later).await.unwrap().len(), 3);
        // Sıcaklık penceresi geçince B artık öne çıkmaz → popüler A önce.
        let much_later = later + FETCH_RETRY_COOLDOWN_SECS + 1;
        let q3 = store.next_to_fetch(10, much_later).await.unwrap();
        assert_eq!(q3[0], ih(1));
        // 3. deneme yapıldı → başarısızlık artık kalıcı unreachable.
        store.mark_fetch_failed(ih(1)).await.unwrap();
        let (pending, _hot, unreachable, _recent) =
            store.fetch_queue_stats(much_later).await.unwrap();
        assert_eq!(unreachable, 1);
        assert_eq!(pending, 2);
        // Kuyruk tükendi (hepsi 3 denemede).
        assert!(store
            .next_to_fetch(10, much_later + FETCH_RETRY_COOLDOWN_SECS + 1)
            .await
            .unwrap()
            .is_empty());
        // Başarı: upsert → fetched + fetched_at set, sayaçtan düşer.
        let mut r = record("0202020202020202020202020202020202020202", "Hot Item", 1);
        r.first_seen = much_later;
        r.last_seen = much_later;
        store.upsert_torrent(&r).await.unwrap();
        let (_p, _h, _u, recent) = store.fetch_queue_stats(much_later + 10).await.unwrap();
        assert_eq!(recent, 1, "fetched_at ile son 1 saat sayacı");
    }

    #[test]
    fn fts_query_sanitizes_special_chars() {
        assert_eq!(to_fts_query("ubuntu 24.04"), "\"ubuntu\"* \"2404\"*");
        assert_eq!(to_fts_query("  a-b (c) "), "\"ab\"* \"c\"*");
        assert_eq!(to_fts_query("!!!"), "");
        assert_eq!(to_fts_query(""), "");
    }
}
