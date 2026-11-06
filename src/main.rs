// Nubisianua — a lightweight doorway to your cloud AI.
//
// This is a wrapper, not a local model: one native window (the host) holds
// a slim tab strip — the project's own tiny frontend in ui/index.html —
// and one child webview per tab, each opening onto a cloud AI using the
// operating system's own webview (WebView2 on Windows, WebKitGTK on
// Linux). Nothing thinks locally except the window itself.
//
// All multi-webview code lives in src/tabs.rs; tab persistence lives in
// src/config.rs. See SPEC-0.0.3a-tabs.md for the design.

// Prevents an extra console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod tabs;

use tauri::{Manager, WindowEvent};

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            // Load (or default) the tab list and hand it to the tab manager.
            let cfg = config::load(app.handle());
            app.manage(tabs::TabManager::new(cfg));

            // Create the tab webviews from an async task, NOT from setup
            // itself: creating a child webview inside a synchronous handler
            // can deadlock WebView2 on Windows (see src/tabs.rs docs).
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = handle.state::<tabs::TabManager>();
                tabs::restore(&handle, &state).await;
            });

            Ok(())
        })
        .on_window_event(|window, event| {
            // Keep the active tab fitted under the strip when the host
            // window resizes. Resize only — this handler never creates
            // webviews (Windows deadlock rule).
            if window.label() == "main" {
                if let WindowEvent::Resized(_) = event {
                    let app = window.app_handle();
                    let state = app.state::<tabs::TabManager>();
                    tabs::layout(app, &state);
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            tabs::get_state,
            tabs::get_presets,
            tabs::create_tab,
            tabs::activate_tab,
            tabs::close_tab,
            tabs::rename_tab,
            tabs::cycle_tab,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Nubisianua");
}
