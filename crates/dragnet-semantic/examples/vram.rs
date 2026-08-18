// SPDX-License-Identifier: AGPL-3.0-only
//! VRAM sızıntı testi: modeli yükle → bekle → düşür → bekle. Dışarıdan `nvidia-smi` ile izlenir.
use dragnet_semantic::{Device, Semantic, SemanticConfig, Tier};
fn main() {
    let tier = Tier::parse(&std::env::args().nth(1).unwrap_or_default());
    let cfg = SemanticConfig {
        tier,
        device: Device::Auto,
        models_dir: "C:/dgcache/dragnet-models".into(),
    };
    println!("PID {} — yükleniyor", std::process::id());
    let sem = Semantic::load(&cfg).unwrap();
    let _ = sem.embed_and_add(&[(
        dragnet_core::InfoHash::from_bytes([1; 20]),
        "warmup query text".into(),
    )]);
    println!("YÜKLÜ ({}) — 8 sn", sem.device());
    std::thread::sleep(std::time::Duration::from_secs(8));
    drop(sem);
    println!("DÜŞÜRÜLDÜ — 8 sn");
    std::thread::sleep(std::time::Duration::from_secs(8));
    // İkinci yükleme (model değişimi senaryosu): yeni oturum + eskisi yok.
    let sem2 = Semantic::load(&cfg).unwrap();
    let _ = sem2.embed_and_add(&[(dragnet_core::InfoHash::from_bytes([2; 20]), "again".into())]);
    println!("YENİDEN YÜKLÜ — 8 sn");
    std::thread::sleep(std::time::Duration::from_secs(8));
    drop(sem2);
    println!("DÜŞÜRÜLDÜ 2 — 8 sn");
    std::thread::sleep(std::time::Duration::from_secs(8));
    println!("ÇIKIŞ");
}
