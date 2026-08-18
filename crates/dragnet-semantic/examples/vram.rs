// SPDX-License-Identifier: AGPL-3.0-only
//! VRAM ölçüm/sızıntı testi: modeli yükle → ilk çıkarım → bekle → düşür → bekle.
//! Ölçüm `hw::gpu_memory()` (DXGI, süreç-yerel) ile **uygulama içinden** yapılır; her aşamada
//! ve beklerken periyodik yazdırılır — F0'daki "yükleme anında 0 MB" hatasının kaynağı budur:
//! DirectML ağırlıkları/tamponları ilk çıkarımda tahsis eder.
use dragnet_semantic::{hw, Device, Semantic, SemanticConfig, Tier};

fn mem(tag: &str) -> u64 {
    match hw::gpu_memory() {
        Some(g) => {
            println!(
                "{tag:<22} kullanım {:>5} MB · bütçe {:>5} MB · toplam {:>5} MB · {}",
                g.current_usage / 1_048_576,
                g.budget / 1_048_576,
                g.dedicated_total / 1_048_576,
                g.adapter
            );
            g.current_usage / 1_048_576
        }
        None => {
            println!("{tag:<22} GPU yok (DXGI okunamadı)");
            0
        }
    }
}

/// `secs` saniye boyunca 2 sn'de bir ölç (uygulamanın yoklama aralığına yakın).
fn watch(tag: &str, secs: u64) {
    for i in 0..secs / 2 {
        std::thread::sleep(std::time::Duration::from_secs(2));
        mem(&format!("{tag} +{}s", (i + 1) * 2));
    }
}

fn main() {
    let tier = Tier::parse(&std::env::args().nth(1).unwrap_or_default());
    let cfg = SemanticConfig {
        tier,
        device: Device::Auto,
        models_dir: "C:/dgcache/dragnet-models".into(),
    };
    println!("PID {} — {tier:?} yükleniyor", std::process::id());
    mem("başlangıç");
    let sem = Semantic::load(&cfg).unwrap();
    // F0'ın çekirdek gözlemi: burada genelde hâlâ ~0 MB'dır.
    mem("yüklendi (çıkarımsız)");
    let _ = sem.embed_and_add(&[(
        dragnet_core::InfoHash::from_bytes([1; 20]),
        "warmup query text".into(),
    )]);
    mem("ilk çıkarım sonrası");
    println!("-- YÜKLÜ ({}) — 8 sn izleniyor", sem.device());
    watch("yüklü", 8);
    drop(sem);
    std::thread::sleep(std::time::Duration::from_millis(400));
    println!("-- DÜŞÜRÜLDÜ — 8 sn izleniyor");
    watch("düşürüldü", 8);
    // İkinci yükleme (model değişimi senaryosu): yeni oturum + eskisi yok.
    let sem2 = Semantic::load(&cfg).unwrap();
    let _ = sem2.embed_and_add(&[(dragnet_core::InfoHash::from_bytes([2; 20]), "again".into())]);
    println!("-- YENİDEN YÜKLÜ — 8 sn izleniyor");
    watch("yeniden yüklü", 8);
    drop(sem2);
    std::thread::sleep(std::time::Duration::from_millis(400));
    println!("-- DÜŞÜRÜLDÜ 2 — 8 sn izleniyor");
    watch("düşürüldü 2", 8);
    println!("ÇIKIŞ");
}
