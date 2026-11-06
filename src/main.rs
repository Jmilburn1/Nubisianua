// Nubisianua — a lightweight doorway to your cloud AI.
//
// This is a wrapper, not a local model: it opens one native window onto
// https://muse.ai using the operating system's own webview
// (WebView2 on Windows, WebKitGTK on Linux). Nothing is bundled,
// nothing runs locally except the window itself.

// Prevents an extra console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use tauri::{WebviewUrl, WebviewWindowBuilder};

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let url = "https://muse.ai"
                .parse()
                .expect("hardcoded muse.ai URL must parse");

            WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url))
                .title("Nubisianua")
                .inner_size(1200.0, 850.0)
                .min_inner_size(800.0, 600.0)
                .resizable(true)
                // Dark background so the window never flashes white while
                // the page loads. RGBA matching the icon's deep-navy field.
                .background_color(tauri::window::Color(11, 14, 20, 255))
                .build()?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Nubisianua");
}
