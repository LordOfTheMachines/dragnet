// SPDX-License-Identifier: AGPL-3.0-only
//! dragnet-sign — oto-güncelleme için ed25519 anahtar üretimi ve dosya imzalama.
//!
//! Kullanım:
//!   dragnet-sign generate            # anahtar üret (özel tohum ev dizinine yazılır), pubkey bas
//!   dragnet-sign pubkey              # mevcut tohumdan pubkey (b64) bas
//!   dragnet-sign sign <dosya>        # <dosya>.sig (base64 ed25519) üret
//!
//! Özel tohum repo DIŞINDA saklanır: %USERPROFILE%\.dragnet-updater\ed25519_seed.bin
//! Pubkey uygulamaya (updater.rs PUBKEY_B64) gömülür. Yayınlanan binary'nin .sig'i
//! bu araçla üretilip GitHub Release'e binary'nin yanına yüklenir.

use std::io::Write;
use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use ed25519_dalek::{Signer, SigningKey};

fn seed_dir() -> PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".dragnet-updater")
}

fn seed_path() -> PathBuf {
    seed_dir().join("ed25519_seed.bin")
}

fn load_seed() -> Result<[u8; 32], String> {
    // Öncelik: env DRAGNET_SEED_B64, yoksa dosya.
    if let Ok(b64) = std::env::var("DRAGNET_SEED_B64") {
        let bytes = B64.decode(b64.trim()).map_err(|e| e.to_string())?;
        return bytes.try_into().map_err(|_| "tohum 32 bayt olmalı".to_string());
    }
    let bytes = std::fs::read(seed_path())
        .map_err(|e| format!("tohum okunamadı ({}): {e}", seed_path().display()))?;
    bytes.try_into().map_err(|_| "tohum 32 bayt olmalı".to_string())
}

fn signing_key() -> Result<SigningKey, String> {
    Ok(SigningKey::from_bytes(&load_seed()?))
}

fn pubkey_b64(sk: &SigningKey) -> String {
    B64.encode(sk.verifying_key().to_bytes())
}

fn cmd_generate() -> Result<(), String> {
    let dir = seed_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    if seed_path().exists() {
        return Err(format!(
            "tohum zaten var: {} (üzerine yazmıyorum)",
            seed_path().display()
        ));
    }
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).map_err(|e| e.to_string())?;
    std::fs::write(seed_path(), seed).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("ed25519_seed.b64.txt"), B64.encode(seed)).map_err(|e| e.to_string())?;
    let sk = SigningKey::from_bytes(&seed);
    println!("Anahtar üretildi. Özel tohum: {}", seed_path().display());
    println!("PUBKEY_B64 (updater.rs'e göm): {}", pubkey_b64(&sk));
    Ok(())
}

fn cmd_pubkey() -> Result<(), String> {
    println!("{}", pubkey_b64(&signing_key()?));
    Ok(())
}

fn cmd_sign(path: &str) -> Result<(), String> {
    let sk = signing_key()?;
    let data = std::fs::read(path).map_err(|e| format!("{path} okunamadı: {e}"))?;
    let sig = sk.sign(&data);
    let sig_b64 = B64.encode(sig.to_bytes());
    let out = format!("{path}.sig");
    let mut f = std::fs::File::create(&out).map_err(|e| e.to_string())?;
    f.write_all(sig_b64.as_bytes()).map_err(|e| e.to_string())?;
    println!("İmza yazıldı: {out}");
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let result = match args.get(1).map(String::as_str) {
        Some("generate") => cmd_generate(),
        Some("pubkey") => cmd_pubkey(),
        Some("sign") => match args.get(2) {
            Some(path) => cmd_sign(path),
            None => Err("kullanım: dragnet-sign sign <dosya>".to_string()),
        },
        _ => Err(
            "kullanım: dragnet-sign <generate|pubkey|sign <dosya>>".to_string(),
        ),
    };
    if let Err(e) = result {
        eprintln!("hata: {e}");
        std::process::exit(1);
    }
}
