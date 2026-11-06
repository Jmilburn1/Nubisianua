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

/// Which window edge the tab shelf docks to (v0.0.5a). John's preference
/// is the right edge — vertical tabs that start on the left are what he
/// does NOT like — so `Right` is the default. The side is chosen from a
/// settings-style menu on the shelf (mirroring Firefox's Settings toggle),
/// never by dragging the shelf itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DockSide {
    Left,
    Right,
}

impl Default for DockSide {
    fn default() -> Self {
        DockSide::Right
    }
}

impl DockSide {
    pub fn as_str(&self) -> &'static str {
        match self {
            DockSide::Left => "left",
            DockSide::Right => "right",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "left" => Some(DockSide::Left),
            "right" => Some(DockSide::Right),
            _ => None,
        }
    }
}

/// Shelf width on first run with the shelf feature: fully expanded
/// (logo + name). Bounds live in `tabs.rs` and are enforced on load too.
pub const DEFAULT_SHELF_WIDTH: f64 = 208.0;

fn default_shelf_width() -> f64 {
    DEFAULT_SHELF_WIDTH
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TabConfig {
    pub version: u32,
    #[serde(rename = "activeTab")]
    pub active_tab: Option<String>,
    pub tabs: Vec<TabEntry>,
    /// Shelf width in logical px (v0.0.5a). `serde(default)` keeps configs
    /// written by 0.0.3a/0.0.4a — which have no such field — loading fine.
    #[serde(rename = "shelfWidth", default = "default_shelf_width")]
    pub shelf_width: f64,
    /// Shelf dock side (v0.0.5a); defaults to `DockSide::Right`.
    #[serde(rename = "dockSide", default)]
    pub dock_side: DockSide,
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
            shelf_width: DEFAULT_SHELF_WIDTH,
            dock_side: DockSide::default(),
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
    // Enforce the shelf width bounds on the way in, whatever wrote the file.
    cfg.shelf_width = cfg
        .shelf_width
        .clamp(crate::tabs::SHELF_MIN_WIDTH, crate::tabs::SHELF_MAX_WIDTH);
    cfg
}

/// Persist the tab list. Written on every change (tab add / close / rename /
/// activate, shelf width on drag release, dock side). Errors are swallowed
/// on purpose: a failed save must never take the window down with it; the
/// in-memory state is still correct.
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

