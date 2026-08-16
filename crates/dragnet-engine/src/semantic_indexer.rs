// SPDX-License-Identifier: AGPL-3.0-only
//! Semantik indeksleyici (Faz D): açılışta kalıcı embedding'leri RAM'e yükler, sonra
//! arka planda embed edilmemiş torrent'leri partiler hâlinde embed edip hem indekse hem
//! SQLite'a yazar. Engine'den **bağımsız** çalışır — semantik arama açıkken tarama
//! kapalı olsa da yeni/eksik kayıtlar indekslenir. Nazik: küçük partiler, arada bekleme.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use dragnet_semantic::Semantic;
use dragnet_store::Store;

/// Açılışta bir sayfada okunacak satır sayısı.
const LOAD_PAGE: i64 = 5_000;
/// Backlog boşken bekleme (yeni metadata gelmesini bekle).
const IDLE_SLEEP: Duration = Duration::from_secs(20);
/// Partiler arası nefes (CPU'yu boğma; GPU'da da SQLite yazımına yer bırakır).
const BATCH_PAUSE: Duration = Duration::from_millis(150);

/// Kalıcı embedding'leri (bu model için) bellek-içi indekse yükler. Başka modele ait
/// satırlar önce silinir (kademe değişimi → yeniden indeksleme). Yüklenen satır sayısı.
pub async fn load_index(
    store: &Store,
    sem: &Arc<Semantic>,
) -> Result<usize, dragnet_store::StoreError> {
    let model_id = sem.model_id().to_string();
    let dropped = store.reset_embeddings_except(&model_id).await?;
    if dropped > 0 {
        info!(dropped, model = %model_id, "eski modele ait embedding'ler silindi; yeniden indekslenecek");
    }
    let mut after = 0i64;
    let mut loaded = 0usize;
    loop {
        let page = store
            .load_embeddings_page(&model_id, after, LOAD_PAGE)
            .await?;
        if page.is_empty() {
            break;
        }
        after = page.last().map(|r| r.0).unwrap_or(after);
        let mut idx = sem.index().write().unwrap_or_else(|p| p.into_inner());
        for (_, ih, q, scale) in &page {
            if idx.add_quantized(*ih, q, *scale) {
                loaded += 1;
            }
        }
    }
    info!(loaded, model = %model_id, device = sem.device(), "semantik indeks yüklendi");
    Ok(loaded)
}

/// Arka plan indeksleyiciyi başlatır. Döndürülen görev iptal edilene kadar çalışır
/// (semantik kapatılınca `abort()`). Hatalar loglanır, görev düşmez.
pub fn spawn_indexer(store: Store, sem: Arc<Semantic>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let batch = if sem.device() == "directml" { 256 } else { 64 };
        let model_id = sem.model_id().to_string();
        loop {
            let items = match store.embed_backlog(&model_id, batch as i64).await {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, "embed backlog okunamadı");
                    tokio::time::sleep(IDLE_SLEEP).await;
                    continue;
                }
            };
            if items.is_empty() {
                tokio::time::sleep(IDLE_SLEEP).await;
                continue;
            }
            let n = items.len();
            let sem2 = Arc::clone(&sem);
            let rows = match tokio::task::spawn_blocking(move || sem2.embed_and_add(&items)).await {
                Ok(Ok(rows)) => rows,
                Ok(Err(e)) => {
                    warn!(error = %e, "embed hatası; bekleyip yeniden denenecek");
                    tokio::time::sleep(IDLE_SLEEP).await;
                    continue;
                }
                Err(e) => {
                    warn!(error = %e, "embed görevi çöktü");
                    tokio::time::sleep(IDLE_SLEEP).await;
                    continue;
                }
            };
            if let Err(e) = store.insert_embeddings(&model_id, &rows).await {
                warn!(error = %e, "embedding'ler yazılamadı");
            } else {
                debug!(n, model = %model_id, "parti indekslendi");
            }
            tokio::time::sleep(BATCH_PAUSE).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dragnet_core::{InfoHash, TorrentFile, TorrentRecord};
    use dragnet_semantic::{MockEmbedder, Tier};

    fn record(hex: &str, name: &str) -> TorrentRecord {
        TorrentRecord {
            infohash: InfoHash::from_hex(hex).unwrap(),
            name: name.to_string(),
            total_size: 1,
            files: vec![TorrentFile {
                path: name.to_string(),
                size: 1,
            }],
            first_seen: 1000,
            last_seen: 1000,
            seen_count: 1,
        }
    }

    #[tokio::test]
    async fn indexer_embeds_backlog_and_reload_restores_index() {
        let store = Store::in_memory().await.unwrap();
        store
            .upsert_torrent(&record(
                "1111111111111111111111111111111111111111",
                "The.Matrix.1999",
            ))
            .await
            .unwrap();
        store
            .upsert_torrent(&record(
                "2222222222222222222222222222222222222222",
                "ubuntu.iso",
            ))
            .await
            .unwrap();

        let sem = Arc::new(Semantic::with_embedder(
            Box::new(MockEmbedder::new(32)),
            Tier::Light,
            0.0,
        ));
        assert_eq!(load_index(&store, &sem).await.unwrap(), 0);

        let h = spawn_indexer(store.clone(), Arc::clone(&sem));
        // Kısa sürede iki kayıt da indekslenmeli (mock anlık).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while store.count_embeddings("mock").await.unwrap() < 2
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        h.abort();
        assert_eq!(store.count_embeddings("mock").await.unwrap(), 2);
        assert_eq!(sem.status().indexed, 2);
        let hits = sem.search("matrix", 1).unwrap();
        assert_eq!(
            hits[0].infohash,
            InfoHash::from_hex("1111111111111111111111111111111111111111").unwrap()
        );

        // Yeni (boş) Semantic'e yükleme → aynı sonuç, yeniden embed etmeden.
        let sem2 = Arc::new(Semantic::with_embedder(
            Box::new(MockEmbedder::new(32)),
            Tier::Light,
            0.0,
        ));
        assert_eq!(load_index(&store, &sem2).await.unwrap(), 2);
        assert_eq!(
            sem2.search("ubuntu", 1).unwrap()[0].infohash,
            InfoHash::from_hex("2222222222222222222222222222222222222222").unwrap()
        );

        // Farklı model_id'li bir Semantic yüklenirse eski satırlar silinir (backlog yeniden dolar).
        struct OtherMock(MockEmbedder);
        impl dragnet_semantic::Embedder for OtherMock {
            fn model_id(&self) -> &str {
                "mock2"
            }
            fn dim(&self) -> usize {
                self.0.dim()
            }
            fn device(&self) -> &str {
                "cpu"
            }
            fn embed_docs(
                &self,
                t: &[String],
            ) -> Result<Vec<Vec<f32>>, dragnet_semantic::SemanticError> {
                self.0.embed_docs(t)
            }
            fn embed_query(&self, t: &str) -> Result<Vec<f32>, dragnet_semantic::SemanticError> {
                self.0.embed_query(t)
            }
        }
        let sem3 = Arc::new(Semantic::with_embedder(
            Box::new(OtherMock(MockEmbedder::new(32))),
            Tier::Balanced,
            0.0,
        ));
        assert_eq!(load_index(&store, &sem3).await.unwrap(), 0);
        assert_eq!(store.count_embeddings("mock").await.unwrap(), 0);
        assert_eq!(store.embed_backlog("mock2", 10).await.unwrap().len(), 2);
    }
}
