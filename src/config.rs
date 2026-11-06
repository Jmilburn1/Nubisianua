//! Tab list persistence (SPEC-0.0.3a-tabs.md §4).
//!
//! The tab list lives in `<app config dir>/tabs.json` and stores labels and
//! URLs only — never credentials. Sign-in state lives in each tab's own
//! webview profile directory (`<app data dir>/profiles/<tab-id>/`), which
//! Rust resolves here but only `tabs.rs` hands to a webview builder.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

pub const DEFAULT_TAB_NAME: &str = "Muse";
pub const DEFAULT_TAB_URL: &str = "https://muse.ai";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TabEntry {
    pub id: String,
    pub name: String,
    pub url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TabConfig {
    pub version: u32,
    #[serde(rename = "activeTab")]
    pub active_tab: Option<String>,
    pub tabs: Vec<TabEntry>,
}

impl Default for TabConfig {
    /// First run ever (or a corrupt/missing config): one default Muse tab.
    fn default() -> Self {
        TabConfig {
            version: 1,
            active_tab: Some("t1".to_string()),
            tabs: vec![TabEntry {
                id: "t1".to_string(),
                name: DEFAULT_TAB_NAME.to_string(),
                url: DEFAULT_TAB_URL.to_string(),
            }],
        }
    }
}

impl TabConfig {
    /// Next free id (`t<n>`), derived from the ids already in use so a tab
    /// keeps a stable id — and therefore a stable profile directory —
    /// across renames and restarts.
    pub fn next_id(&self) -> String {
        let max = self
            .tabs
            .iter()
            .filter_map(|t| t.id.strip_prefix('t'))
            .filter_map(|n| n.parse::<u32>().ok())
            .max()
            .unwrap_or(0);
        format!("t{}", max + 1)
    }
}

fn config_path(app: &AppHandle) -> tauri::Result<PathBuf> {
    Ok(app.path().app_config_dir()?.join("tabs.json"))
}

/// Load the tab list. Any failure — missing file, bad JSON, unknown shape —
/// falls back to the single default Muse tab rather than failing to start
/// (spec §4).
pub fn load(app: &AppHandle) -> TabConfig {
    let fallback = TabConfig::default();
    let Ok(path) = config_path(app) else {
        return fallback;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return fallback;
    };
    let Ok(mut cfg) = serde_json::from_str::<TabConfig>(&text) else {
        return fallback;
    };
    if cfg.tabs.is_empty() {
        return fallback;
    }
    // Keep the stored active tab only if it still names a real tab.
    if let Some(active) = &cfg.active_tab {
        if !cfg.tabs.iter().any(|t| &t.id == active) {
            cfg.active_tab = Some(cfg.tabs[0].id.clone());
        }
    } else {
        cfg.active_tab = Some(cfg.tabs[0].id.clone());
    }
    cfg
}

/// Persist the tab list. Written on every tab change (add / close / rename /
/// activate). Errors are swallowed on purpose: a failed save must never take
/// the window down with it; the in-memory state is still correct.
pub fn save(app: &AppHandle, cfg: &TabConfig) {
    let Ok(path) = config_path(app) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(cfg) {
        let _ = std::fs::write(&path, text);
    }
}

/// The per-tab profile directory passed to `WebviewBuilder::data_directory`.
/// Two tabs never share one, so two tabs never share a session (spec §4).
pub fn profile_dir(app: &AppHandle, tab_id: &str) -> tauri::Result<PathBuf> {
    let dir = app.path().app_data_dir()?.join("profiles").join(tab_id);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
