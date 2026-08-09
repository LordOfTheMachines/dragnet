// SPDX-License-Identifier: AGPL-3.0-only
//! dragnet-tray — dragnetd için sistem tepsisi (system tray) kontrolcüsü.
//!
//! Hafif, webview'siz bir tray uygulaması: `dragnetd` daemon'ını bir çocuk süreç
//! olarak başlatır/durdurur, durumunu (`/stats`) gösterir, Windows'ta başlangıçta
//! başlatmayı açıp kapatır ve ayar dosyasını açar.
//!
//! Menü (tepsi simgesine tıkla):
//! - Taramayı Başlat / Durdur
//! - Durum: indekslenen / bilinen (canlı)
//! - Ayarları Düzenle (dragnetd.toml)
//! - Windows'ta Başlangıçta Başlat (aç/kapa)
//! - Durumu Tarayıcıda Aç
//! - Çıkış

#![windows_subsystem = "windows"]

use std::fs::File;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tao::event_loop::{ControlFlow, EventLoop};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};

/// Konsol penceresi açılmasını engeller (çocuk süreç için).
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// API portu (dragnetd varsayılanı).
const API_PORT: u16 = 8080;
/// Autostart kayıt defteri değeri adı ve anahtarı.
const AUTOSTART_NAME: &str = "Dragnet";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

fn main() {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));

    let dragnetd_path =
        find_upwards(&exe_dir, "dragnetd.exe", 4).unwrap_or_else(|| exe_dir.join("dragnetd.exe"));
    let config_path = find_upwards(&exe_dir, "dragnetd.toml", 5);
    let example_config = find_upwards(&exe_dir, "dragnetd.example.toml", 5);
    let work_dir = config_path
        .as_ref()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| exe_dir.clone());

    let event_loop = EventLoop::new();

    // --- Menü ---
    let menu = Menu::new();
    let title = MenuItem::new("Dragnet", false, None);
    let status = MenuItem::new("Durum: kapalı", false, None);
    let start = MenuItem::new("Taramayı Başlat", true, None);
    let stop = MenuItem::new("Taramayı Durdur", false, None);
    let settings = MenuItem::new("Ayarları Düzenle (dragnetd.toml)", true, None);
    let autostart =
        CheckMenuItem::new("Windows'ta Başlangıçta Başlat", true, is_autostart(), None);
    let open_api = MenuItem::new("Durumu Tarayıcıda Aç", true, None);
    let quit = MenuItem::new("Çıkış", true, None);

    let _ = menu.append_items(&[
        &title,
        &PredefinedMenuItem::separator(),
        &status,
        &start,
        &stop,
        &PredefinedMenuItem::separator(),
        &settings,
        &autostart,
        &open_api,
        &PredefinedMenuItem::separator(),
        &quit,
    ]);

    // Menü öğe kimliklerini önceden klonla (olay eşleştirmesi için).
    let (start_id, stop_id, settings_id, autostart_id, open_api_id, quit_id) = (
        start.id().clone(),
        stop.id().clone(),
        settings.id().clone(),
        autostart.id().clone(),
        open_api.id().clone(),
        quit.id().clone(),
    );

    // Tepsi simgesini kur (Windows'ta event loop'tan önce güvenli). Canlı kalmalı.
    let _tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Dragnet — DHT indeksleyici")
        .with_icon(make_icon())
        .with_menu_on_left_click(true)
        .build()
        .expect("tepsi simgesi oluşturulamadı");

    let menu_channel = MenuEvent::receiver();

    let mut child: Option<Child> = None;
    let mut scanning = false;
    let mut last_poll = Instant::now();

    event_loop.run(move |_event, _target, control_flow| {
        let _keep = &_tray; // simgeyi canlı tut
        *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_secs(2));

        // Menü tıklamaları.
        while let Ok(ev) = menu_channel.try_recv() {
            if ev.id == start_id {
                if !scanning {
                    match spawn_daemon(&dragnetd_path, config_path.as_deref(), &work_dir) {
                        Ok(c) => {
                            child = Some(c);
                            scanning = true;
                            start.set_enabled(false);
                            stop.set_enabled(true);
                            status.set_text("Durum: tarama başlıyor…");
                        }
                        Err(e) => status.set_text(format!("Başlatılamadı: {e}")),
                    }
                }
            } else if ev.id == stop_id {
                stop_daemon(&mut child);
                scanning = false;
                start.set_enabled(true);
                stop.set_enabled(false);
                status.set_text("Durum: kapalı");
            } else if ev.id == settings_id {
                if let Some(p) = config_path.as_ref().or(example_config.as_ref()) {
                    let _ = Command::new("notepad").arg(p).spawn();
                }
            } else if ev.id == autostart_id {
                autostart.set_checked(toggle_autostart());
            } else if ev.id == open_api_id {
                let _ = Command::new("explorer")
                    .arg(format!("http://127.0.0.1:{API_PORT}/stats"))
                    .spawn();
            } else if ev.id == quit_id {
                stop_daemon(&mut child);
                *control_flow = ControlFlow::Exit;
                return;
            }
        }

        // Periyodik durum güncellemesi.
        if scanning && last_poll.elapsed() >= Duration::from_secs(2) {
            last_poll = Instant::now();

            if let Some(c) = &mut child {
                if matches!(c.try_wait(), Ok(Some(_))) {
                    child = None;
                    scanning = false;
                    start.set_enabled(true);
                    stop.set_enabled(false);
                    status.set_text("Durum: durdu (çocuk süreç kapandı)");
                }
            }

            if scanning {
                match get_stats(API_PORT) {
                    Some((fetched, total)) => status.set_text(format!(
                        "Durum: açık — indekslenen {fetched} / bilinen {total}"
                    )),
                    None => status.set_text("Durum: açık — API başlatılıyor…"),
                }
            }
        }
    });
}

/// Basit 32×32 mavi daire simgesi üretir.
fn make_icon() -> Icon {
    let size: u32 = 32;
    let (cx, cy, r) = (15.5f32, 15.5f32, 14.0f32);
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let d = (dx * dx + dy * dy).sqrt();
            let i = ((y * size + x) * 4) as usize;
            if d <= r {
                rgba[i] = 0x1f;
                rgba[i + 1] = 0x6f;
                rgba[i + 2] = 0xeb;
                rgba[i + 3] = if d > r - 1.0 {
                    ((r - d).clamp(0.0, 1.0) * 255.0) as u8
                } else {
                    255
                };
            }
        }
    }
    Icon::from_rgba(rgba, size, size).expect("simge oluşturulamadı")
}

/// dragnetd'yi çocuk süreç olarak başlatır (konsolsuz, log dosyasına yazar).
fn spawn_daemon(dragnetd: &Path, config: Option<&Path>, work_dir: &Path) -> std::io::Result<Child> {
    let log = File::options()
        .create(true)
        .append(true)
        .open(work_dir.join("dragnet.log"))?;
    let mut cmd = Command::new(dragnetd);
    if let Some(cfg) = config {
        cmd.arg(cfg);
    }
    cmd.current_dir(work_dir)
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .creation_flags(CREATE_NO_WINDOW);
    cmd.spawn()
}

/// Çalışan daemon'ı durdurur.
fn stop_daemon(child: &mut Option<Child>) {
    if let Some(mut c) = child.take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// `start`'tan başlayıp yukarı doğru `levels` seviye `name` dosyasını arar.
fn find_upwards(start: &Path, name: &str, levels: usize) -> Option<PathBuf> {
    let mut dir = Some(start.to_path_buf());
    for _ in 0..=levels {
        let d = dir.as_ref()?;
        let candidate = d.join(name);
        if candidate.exists() {
            return Some(candidate);
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    None
}

/// `127.0.0.1:port/stats`'a basit bir GET atıp `(fetched, total)` döner.
fn get_stats(port: u16) -> Option<(i64, i64)> {
    let addr = format!("127.0.0.1:{port}");
    let mut stream =
        TcpStream::connect_timeout(&addr.parse().ok()?, Duration::from_millis(800)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(800)))
        .ok()?;
    stream
        .write_all(b"GET /stats HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n")
        .ok()?;
    let mut buf = String::new();
    let _ = stream.read_to_string(&mut buf);
    let fetched = extract_int(&buf, "\"fetched_torrents\":")?;
    let total = extract_int(&buf, "\"total_infohashes\":")?;
    Some((fetched, total))
}

/// JSON gövdesinden `key`'den sonraki tam sayıyı çeker.
fn extract_int(s: &str, key: &str) -> Option<i64> {
    let start = s.find(key)? + key.len();
    let rest = &s[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '-')
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

// --- Windows başlangıçta başlatma (HKCU\...\Run) ---

fn is_autostart() -> bool {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    hkcu.open_subkey(RUN_KEY)
        .and_then(|k| k.get_value::<String, _>(AUTOSTART_NAME))
        .is_ok()
}

/// Autostart'ı açıp kapatır; yeni durumu (açık mı) döner.
fn toggle_autostart() -> bool {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_ALL_ACCESS};
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = match hkcu.open_subkey_with_flags(RUN_KEY, KEY_ALL_ACCESS) {
        Ok(k) => k,
        Err(_) => match hkcu.create_subkey(RUN_KEY) {
            Ok((k, _)) => k,
            Err(_) => return is_autostart(),
        },
    };
    if is_autostart() {
        let _ = key.delete_value(AUTOSTART_NAME);
        false
    } else if let Ok(exe) = std::env::current_exe() {
        let value = format!("\"{}\"", exe.display());
        let _ = key.set_value(AUTOSTART_NAME, &value);
        true
    } else {
        false
    }
}
