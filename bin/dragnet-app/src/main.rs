// SPDX-License-Identifier: AGPL-3.0-only
//! dragnet-app — Dragnet masaüstü kabuğu (tek exe).
//!
//! Tauri 2 kabuğu (Sello deseni, AGPL): tray, dashboard penceresi, arama, ayarlar,
//! Windows başlangıçta başlat ve GitHub Releases oto-güncelleme. Boru hattını
//! `dragnet-engine` çekirdeğiyle **süreç içinde** çalıştırır — ayrı daemon yok.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod commands;
mod settings;
mod updater;

use std::sync::Mutex as StdMutex;
use std::time::Instant;

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WindowEvent};
use tokio::sync::Mutex as TokioMutex;

use dragnet_engine::Engine;
use dragnet_store::Store;
use settings::Settings;

/// Paylaşılan uygulama durumu.
pub struct AppState {
    /// Sorgu deposu — tarama açık/kapalı fark etmeksizin okunur.
    pub store: Store,
    /// Açılışta sabitlenen etkin db yolu. Ayarlarda db_path değişse bile çalışan
    /// depo/motor/API tutarlı kalsın diye motor hep buna yazar; yeni yol yeniden
    /// başlatınca geçerli olur.
    pub db_path: String,
    /// Çalışan çekirdek (tarama açıkken `Some`).
    pub engine: TokioMutex<Option<Engine>>,
    pub settings: StdMutex<Settings>,
    /// BEP-51 örnek/sn hesabı için önceki örnek sayacı + zamanı.
    pub rate_prev: StdMutex<(u64, Instant)>,
}

fn main() {
    let _ = init_logging();

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main(app);
        }))
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let settings = Settings::load();

            // Etkin db yolunu açılışta sabitle (depo + API + motor aynı dosyayı görsün).
            let db_path = settings.db_path_abs();
            let store = match tauri::async_runtime::block_on(Store::open(&db_path)) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = %e, path = %db_path, "veritabanı açılamadı");
                    return Err(format!("Veritabanı açılamadı ({db_path}): {e}").into());
                }
            };

            // Arama API'si çekirdekten AYRI, uzun ömürlü: tarama durdurulsa bile
            // (qBittorrent eklentisi dahil) arama erişimi kesilmez. Depoya karşı sunar.
            match settings.api_addr() {
                Ok(bind) => {
                    let api_cfg = dragnet_api::ApiConfig {
                        bind,
                        token: None,
                        ..Default::default()
                    };
                    let api_store = store.clone();
                    tauri::async_runtime::spawn(async move {
                        if let Err(e) = dragnet_api::serve(api_cfg, api_store).await {
                            tracing::error!(error = %e, "API sunucusu durdu");
                        }
                    });
                }
                Err(e) => tracing::error!(error = %e, "API başlatılamadı (geçersiz adres)"),
            }

            // İsteğe göre taramayı açılışta başlat (etkin db yoluna).
            let engine = if settings.auto_scan {
                let cfg = settings.to_engine_config(db_path.clone());
                match tauri::async_runtime::block_on(Engine::start(cfg)) {
                    Ok(e) => Some(e),
                    Err(e) => {
                        tracing::error!(error = %e, "çekirdek başlatılamadı");
                        None
                    }
                }
            } else {
                None
            };

            app.manage(AppState {
                store,
                db_path,
                engine: TokioMutex::new(engine),
                settings: StdMutex::new(settings),
                rate_prev: StdMutex::new((0, Instant::now())),
            });

            build_tray(app.handle())?;

            // --silent ile başlarsa tepside kal, pencereyi açma.
            if !std::env::args().any(|a| a == "--silent") {
                show_main(app.handle());
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Pencere X'e basınca kapanmak yerine tepsiye gizlen.
            if let WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::app_info,
            commands::get_stats,
            commands::search,
            commands::dashboard,
            commands::network_health,
            commands::start_scan,
            commands::stop_scan,
            commands::get_settings,
            commands::set_settings,
            commands::set_autostart,
            commands::check_update,
            commands::install_update,
        ])
        .run(tauri::generate_context!())
        .expect("Tauri uygulaması başlatılamadı");
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Panoyu Aç", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Çıkış", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &sep, &quit])?;

    let mut builder = TrayIconBuilder::with_id("dragnet-tray")
        .tooltip(format!("Dragnet v{}", env!("CARGO_PKG_VERSION")))
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_main(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } = event
            {
                show_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon().cloned() {
        builder = builder.icon(icon);
    }
    builder.build(app)?;
    Ok(())
}

/// exe yanında `dragnet-app.log` dosyasına yapılandırılmış log (teşhis için).
fn init_logging() -> Option<()> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("dragnet-app.log"))
        .ok()?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,dragnet_engine=info,dragnet_dht=info,mainline=error".into()),
        )
        .with_writer(move || file.try_clone().expect("log dosyası klonlanamadı"))
        .with_ansi(false)
        .init();
    Some(())
}
