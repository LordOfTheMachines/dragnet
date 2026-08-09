// SPDX-License-Identifier: AGPL-3.0-only
//! Oto-güncelleme — GitHub Releases tabanlı, ed25519 imzalı, taşınabilir tek exe
//! (Sello updater deseni; AGPL olarak yeniden yazıldı). Tauri updater plugin'i
//! KULLANILMAZ. Yayınlanan `dragnet-app.exe` ve `dragnet-app.exe.sig` çekilir,
//! imza gömülü public key ile doğrulanır, exe yerinde değiştirilir.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::Serialize;

/// Uygulamaya gömülü ed25519 public key (özel tohum repo DIŞINDA).
const PUBKEY_B64: &str = "iwtIW/WGLTdI/kOuuuOKUq52pOF+e5XNqzHtvLiuIN8=";
/// Kaynak repo kod içinde sabitlenir (hijack/downgrade koruması).
const CANONICAL_REPO: &str = "LordOfTheMachines/dragnet";
const EXE_ASSET: &str = "dragnet-app.exe";
const SIG_ASSET: &str = "dragnet-app.exe.sig";

#[derive(Debug, Clone, Serialize)]
pub struct UpdateInfo {
    pub version: String,
    pub notes: String,
    pub exe_url: String,
    pub sig_url: String,
}

fn http() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent("dragnet-updater")
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())
}

/// GitHub'daki en son sürümü kontrol eder. Yeni sürüm varsa `Some(UpdateInfo)`.
pub fn check() -> Result<Option<UpdateInfo>, String> {
    let url = format!("https://api.github.com/repos/{CANONICAL_REPO}/releases/latest");
    let resp = http()?
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .map_err(|e| e.to_string())?;

    if resp.status().as_u16() == 404 {
        return Ok(None); // henüz sürüm yok
    }
    if !resp.status().is_success() {
        return Err(format!("GitHub API {}", resp.status()));
    }

    let json: serde_json::Value = resp.json().map_err(|e| e.to_string())?;
    let tag = json["tag_name"].as_str().unwrap_or_default();
    let latest = parse_version(tag);
    let current = parse_version(env!("CARGO_PKG_VERSION"));
    if latest <= current {
        return Ok(None);
    }

    let assets = json["assets"].as_array().cloned().unwrap_or_default();
    let find = |name: &str| -> Option<String> {
        assets.iter().find_map(|a| {
            if a["name"].as_str() == Some(name) {
                a["browser_download_url"].as_str().map(String::from)
            } else {
                None
            }
        })
    };
    let exe_url = find(EXE_ASSET).ok_or("sürümde dragnet-app.exe yok")?;
    let sig_url = find(SIG_ASSET).ok_or("sürümde dragnet-app.exe.sig yok")?;

    Ok(Some(UpdateInfo {
        version: tag.trim_start_matches('v').to_string(),
        notes: json["body"].as_str().unwrap_or_default().to_string(),
        exe_url,
        sig_url,
    }))
}

/// Güncellemeyi indirir, imzayı doğrular ve çalışan exe'yi yerinde değiştirir.
pub fn install(info: &UpdateInfo) -> Result<(), String> {
    let client = http()?;
    let exe_bytes = client
        .get(&info.exe_url)
        .timeout(std::time::Duration::from_secs(300))
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.bytes())
        .map_err(|e| format!("exe indirilemedi: {e}"))?;
    let sig_b64 = client
        .get(&info.sig_url)
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.text())
        .map_err(|e| format!("imza indirilemedi: {e}"))?;

    verify(&exe_bytes, sig_b64.trim())?;

    // Doğrulanmış yeni exe'yi geçici dosyaya yaz, sonra çalışan exe ile değiştir.
    let tmp = std::env::temp_dir().join("dragnet-app-new.exe");
    std::fs::write(&tmp, &exe_bytes).map_err(|e| e.to_string())?;
    self_replace::self_replace(&tmp).map_err(|e| format!("exe değiştirilemedi: {e}"))?;
    let _ = std::fs::remove_file(&tmp);
    Ok(())
}

/// ed25519 imza doğrulaması (gömülü public key ile, strict).
fn verify(data: &[u8], sig_b64: &str) -> Result<(), String> {
    let pk_bytes: [u8; 32] = B64
        .decode(PUBKEY_B64)
        .map_err(|e| e.to_string())?
        .try_into()
        .map_err(|_| "public key 32 bayt değil".to_string())?;
    let sig_bytes: [u8; 64] = B64
        .decode(sig_b64)
        .map_err(|e| e.to_string())?
        .try_into()
        .map_err(|_| "imza 64 bayt değil".to_string())?;
    let vk = VerifyingKey::from_bytes(&pk_bytes).map_err(|e| e.to_string())?;
    vk.verify_strict(data, &Signature::from_bytes(&sig_bytes))
        .map_err(|_| "imza doğrulanamadı — güncelleme reddedildi".to_string())
}

/// "v1.2.3" / "1.2.3" → (1,2,3). Ayrıştırılamayan parça 0.
fn parse_version(s: &str) -> (u32, u32, u32) {
    let s = s.trim().trim_start_matches('v');
    let mut it = s.split('.').map(|p| p.trim().parse::<u32>().unwrap_or(0));
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}
