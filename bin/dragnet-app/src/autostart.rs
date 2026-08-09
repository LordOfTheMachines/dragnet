// SPDX-License-Identifier: AGPL-3.0-only
//! Windows başlangıçta başlatma (HKCU\...\Run). Sello autostart deseni.

use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "Dragnet";

/// Başlangıçta başlatmayı açar/kapatır.
pub fn set(enabled: bool) -> std::io::Result<()> {
    let (run, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(RUN_KEY)?;
    if !enabled {
        let _ = run.delete_value(VALUE_NAME); // yoksa hata değil
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    // --silent: başlangıçta pencere açmadan tepside başlar (main.rs bu bayrağı okur).
    run.set_value(VALUE_NAME, &format!("\"{}\" --silent", exe.display()))
}
