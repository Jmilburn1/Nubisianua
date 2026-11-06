//! Tab management — ALL multi-webview code lives in this module and nowhere
//! else (SPEC-0.0.3a-tabs.md §3/§5), so if Tauri's `unstable` multi-webview
//! API ever shifts, there is exactly one place to fix.
//!
//! Windows rule (from the `Window::add_child` docs): creating a child
//! webview inside a synchronous command or event handler can deadlock
//! WebView2. Every creation path below therefore runs from an async command
//! or from the async restore task spawned in setup — never from a sync
//! handler. Show/hide/resize of already-created webviews is safe anywhere.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::Serialize;
use tauri::{
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, State, Webview, WebviewUrl, Window,
};

use crate::config::{self, TabConfig, TabEntry};

/// Height of the tab strip (logical px). The frontend strip is exactly this
/// tall (ui/index.html); tab webviews fill everything below it.
pub const STRIP_HEIGHT: f64 = 40.0;

const HOST_LABEL: &str = "main";

/// Preset agents for the + picker (spec §2). Pure data: name + start URL,
/// no per-provider code. All URLs verified 2026-11-06; see the spec's §2
/// field notes (Le Chat now brands itself Mistral Vibe, Qwen Chat brands
/// itself Qwen Studio, Doubao is China-first with restricted sign-in,
/// Copilot redirects work accounts to Microsoft 365 Copilot Chat).
pub const PRESETS: &[(&str, &str)] = &[
    ("Muse", "https://muse.ai"),
    ("ChatGPT", "https://chatgpt.com"),
    ("Claude", "https://claude.ai"),
    ("Gemini", "https://gemini.google.com"),
    ("Microsoft Copilot", "https://copilot.microsoft.com"),
    ("Grok", "https://grok.com"),
    ("Perplexity", "https://www.perplexity.ai"),
    ("Meta AI", "https://www.meta.ai"),
    ("DeepSeek", "https://chat.deepseek.com"),
    ("Qwen", "https://chat.qwen.ai"),
    ("Kimi", "https://www.kimi.com"),
    ("Le Chat (Mistral)", "https://chat.mistral.ai"),
    ("Doubao", "https://www.doubao.com"),
];

#[derive(Clone, Serialize)]
pub struct Preset {
    pub name: String,
    pub url: String,
}

/// What the frontend renders from — returned by `get_state` and pushed on
/// every change via the `tabs-changed` event.
#[derive(Clone, Serialize)]
pub struct TabSnapshot {
    pub tabs: Vec<TabEntry>,
    #[serde(rename = "activeTab")]
    pub active_tab: Option<String>,
}

struct Inner {
    config: TabConfig,
    webviews: HashMap<String, Webview>,
}

pub struct TabManager {
    inner: Mutex<Inner>,
}

impl TabManager {
    pub fn new(config: TabConfig) -> Self {
        TabManager {
            inner: Mutex::new(Inner {
                config,
                webviews: HashMap::new(),
            }),
        }
    }
}

fn webview_label(tab_id: &str) -> String {
    format!("tab-{tab_id}")
}

fn snapshot(inner: &Inner) -> TabSnapshot {
    TabSnapshot {
        tabs: inner.config.tabs.clone(),
        active_tab: inner.config.active_tab.clone(),
    }
}

fn emit_snapshot(app: &AppHandle, inner: &Inner) {
    let _ = app.emit_to(HOST_LABEL, "tabs-changed", snapshot(inner));
}

fn host_window(app: &AppHandle) -> Result<Window, String> {
    app.get_window(HOST_LABEL)
        .ok_or_else(|| "host window missing".to_string())
}

/// Bounds for the active tab's webview: full width, everything below the
/// tab strip, in logical units.
fn content_bounds(window: &Window) -> (LogicalPosition<f64>, LogicalSize<f64>) {
    let scale = window.scale_factor().unwrap_or(1.0);
    let physical = window.inner_size().unwrap_or_default();
    let width = physical.width as f64 / scale;
    let height = (physical.height as f64 / scale - STRIP_HEIGHT).max(50.0);
    (
        LogicalPosition::new(0.0, STRIP_HEIGHT),
        LogicalSize::new(width.max(50.0), height),
    )
}

fn set_window_title(app: &AppHandle, inner: &Inner) {
    if let Some(window) = app.get_window(HOST_LABEL) {
        let title = match &inner.config.active_tab {
            Some(active) => match inner.config.tabs.iter().find(|t| &t.id == active) {
                Some(tab) => format!("Nubisianua — {}", tab.name),
                None => "Nubisianua".to_string(),
            },
            None => "Nubisianua".to_string(),
        };
        let _ = window.set_title(&title);
    }
}

/// Show the active tab's webview fitted to the content area and hide every
/// other tab's webview. Switching never reloads a page (spec §1).
fn apply_visibility(app: &AppHandle, inner: &Inner) {
    let Ok(window) = host_window(app) else {
        return;
    };
    let (pos, size) = content_bounds(&window);
    for (id, webview) in &inner.webviews {
        if Some(id) == inner.config.active_tab.as_ref() {
            let _ = webview.set_position(pos);
            let _ = webview.set_size(size);
            let _ = webview.show();
        } else {
            let _ = webview.hide();
        }
    }
    set_window_title(app, inner);
}

/// The one and only child-webview creation path. Callers must be async
/// (see the module docs — Windows deadlock rule).
fn create_webview(app: &AppHandle, entry: &TabEntry, active: bool) -> Result<Webview, String> {
    let window = host_window(app)?;
    let url: tauri::Url = entry
        .url
        .parse()
        .map_err(|e| format!("bad tab URL {:?}: {e}", entry.url))?;
    let profile = config::profile_dir(app, &entry.id).map_err(|e| e.to_string())?;
    let (pos, size) = content_bounds(&window);
    let builder = tauri::webview::WebviewBuilder::new(
        webview_label(&entry.id),
        WebviewUrl::External(url),
    )
    .data_directory(profile);
    let webview = window
        .add_child(builder, pos, size)
        .map_err(|e| e.to_string())?;
    if active {
        let _ = webview.show();
    } else {
        let _ = webview.hide();
    }
    Ok(webview)
}

/// Create webviews for every tab in the loaded config. Runs from the async
/// task spawned in setup — not from setup itself (deadlock rule).
pub async fn restore(app: &AppHandle, state: &State<'_, TabManager>) {
    let entries: Vec<(TabEntry, bool)> = {
        let inner = state.inner.lock().unwrap();
        let active = inner.config.active_tab.clone();
        inner
            .config
            .tabs
            .iter()
            .map(|t| (t.clone(), Some(&t.id) == active.as_ref()))
            .collect()
    };
    for (entry, is_active) in entries {
        match create_webview(app, &entry, is_active) {
            Ok(webview) => {
                let mut inner = state.inner.lock().unwrap();
                inner.webviews.insert(entry.id.clone(), webview);
            }
            Err(e) => eprintln!("nubisianua: failed to restore tab {}: {e}", entry.id),
        }
    }
    let inner = state.inner.lock().unwrap();
    apply_visibility(app, &inner);
    emit_snapshot(app, &inner);
    // If every webview failed to create, the strip still shows the tab list
    // and the + button works — the app never opens windowless (spec §7.6).
}

/// Re-fit the active webview after the host window resizes. Called from the
/// window event handler in main.rs — resizes only, never creates.
pub fn layout(app: &AppHandle, state: &State<'_, TabManager>) {
    let inner = state.inner.lock().unwrap();
    if inner.webviews.is_empty() {
        return;
    }
    apply_visibility(app, &inner);
}

// ---------------------------------------------------------------------------
// Commands (all async — see the module docs)
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn get_state(state: State<'_, TabManager>) -> Result<TabSnapshot, String> {
    let inner = state.inner.lock().unwrap();
    Ok(snapshot(&inner))
}

#[tauri::command]
pub async fn get_presets() -> Result<Vec<Preset>, String> {
    Ok(PRESETS
        .iter()
        .map(|(name, url)| Preset {
            name: name.to_string(),
            url: url.to_string(),
        })
        .collect())
}

/// Validate a user-supplied URL (Custom URL picker entry).
///
/// https is required for anything remote. The one exception is plain http
/// to a **loopback** address (`localhost`, `127.0.0.1`, `::1`) — that is
/// how a model you run on your own machine is reached (e.g. Muse Glimmer
/// or Muse Spark Open served locally, typically
/// `http://localhost:<port>`), and loopback traffic never leaves the
/// machine. http to any non-loopback host is rejected.
fn validate_url(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    let candidate = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let parsed: tauri::Url = candidate
        .parse()
        .map_err(|e| format!("not a valid URL: {e}"))?;
    let host = parsed.host_str().ok_or("URL has no host")?;
    let is_loopback = host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false);
    match parsed.scheme() {
        "https" => Ok(candidate),
        "http" if is_loopback => Ok(candidate),
        "http" => Err(
            "http:// is only allowed for localhost — use https:// for anything remote"
                .to_string(),
        ),
        other => Err(format!("unsupported URL scheme {other:?}")),
    }
}

#[tauri::command]
pub async fn create_tab(
    app: AppHandle,
    state: State<'_, TabManager>,
    name: String,
    url: String,
) -> Result<TabSnapshot, String> {
    let url = validate_url(&url)?;
    let name = if name.trim().is_empty() {
        "Tab".to_string()
    } else {
        name.trim().to_string()
    };
    let entry = {
        let inner = state.inner.lock().unwrap();
        TabEntry {
            id: inner.config.next_id(),
            name,
            url,
        }
    };
    // Create BEFORE mutating state, with no lock held: if creation fails,
    // the config was never touched.
    let webview = create_webview(&app, &entry, true)?;
    let mut inner = state.inner.lock().unwrap();
    inner.webviews.insert(entry.id.clone(), webview);
    inner.config.tabs.push(entry.clone());
    inner.config.active_tab = Some(entry.id.clone());
    config::save(&app, &inner.config);
    apply_visibility(&app, &inner);
    if let Some(wv) = inner.webviews.get(&entry.id) {
        let _ = wv.set_focus();
    }
    emit_snapshot(&app, &inner);
    Ok(snapshot(&inner))
}

#[tauri::command]
pub async fn activate_tab(
    app: AppHandle,
    state: State<'_, TabManager>,
    id: String,
) -> Result<TabSnapshot, String> {
    let mut inner = state.inner.lock().unwrap();
    if !inner.config.tabs.iter().any(|t| t.id == id) {
        return Err(format!("unknown tab {id}"));
    }
    inner.config.active_tab = Some(id.clone());
    config::save(&app, &inner.config);
    apply_visibility(&app, &inner);
    if let Some(wv) = inner.webviews.get(&id) {
        let _ = wv.set_focus();
    }
    emit_snapshot(&app, &inner);
    Ok(snapshot(&inner))
}

#[tauri::command]
pub async fn close_tab(
    app: AppHandle,
    state: State<'_, TabManager>,
    id: String,
) -> Result<TabSnapshot, String> {
    // Spec §1: closing the last tab closes the app, same as closing the
    // window — so nothing is removed or saved here: exactly like closing
    // the window, the tab list (and every per-tab profile/session) is
    // restored on the next launch.
    let is_last = {
        let inner = state.inner.lock().unwrap();
        inner.config.tabs.len() <= 1 && inner.config.tabs.iter().any(|t| t.id == id)
    };
    if is_last {
        if let Some(window) = app.get_window(HOST_LABEL) {
            let _ = window.close();
        }
        let inner = state.inner.lock().unwrap();
        return Ok(snapshot(&inner));
    }

    let mut inner = state.inner.lock().unwrap();
    let Some(pos) = inner.config.tabs.iter().position(|t| t.id == id) else {
        return Err(format!("unknown tab {id}"));
    };
    let was_active = inner.config.active_tab.as_deref() == Some(id.as_str());
    inner.config.tabs.remove(pos);
    if let Some(webview) = inner.webviews.remove(&id) {
        let _ = webview.close();
    }
    if was_active {
        // Neighbor rule: the tab that slid into the closed tab's slot, or
        // the new last tab if the closed one was last in the list.
        let next = inner
            .config
            .tabs
            .get(pos)
            .or_else(|| inner.config.tabs.last())
            .map(|t| t.id.clone());
        inner.config.active_tab = next;
    }
    config::save(&app, &inner.config);
    apply_visibility(&app, &inner);
    emit_snapshot(&app, &inner);
    Ok(snapshot(&inner))
}

#[tauri::command]
pub async fn rename_tab(
    app: AppHandle,
    state: State<'_, TabManager>,
    id: String,
    name: String,
) -> Result<TabSnapshot, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("tab name cannot be empty".to_string());
    }
    let mut inner = state.inner.lock().unwrap();
    let Some(tab) = inner.config.tabs.iter_mut().find(|t| t.id == id) else {
        return Err(format!("unknown tab {id}"));
    };
    tab.name = name;
    config::save(&app, &inner.config);
    set_window_title(&app, &inner);
    emit_snapshot(&app, &inner);
    Ok(snapshot(&inner))
}

/// Cycle the active tab by `delta` (+1 / -1), wrapping around. Driven by
/// the Ctrl+Tab / Ctrl+Shift+Tab keys in the frontend.
#[tauri::command]
pub async fn cycle_tab(
    app: AppHandle,
    state: State<'_, TabManager>,
    delta: i32,
) -> Result<TabSnapshot, String> {
    let mut inner = state.inner.lock().unwrap();
    let len = inner.config.tabs.len();
    if len == 0 {
        return Ok(snapshot(&inner));
    }
    let current = inner
        .config
        .active_tab
        .as_ref()
        .and_then(|active| inner.config.tabs.iter().position(|t| &t.id == active))
        .unwrap_or(0);
    let next = (current as i32 + delta).rem_euclid(len as i32) as usize;
    inner.config.active_tab = Some(inner.config.tabs[next].id.clone());
    config::save(&app, &inner.config);
    apply_visibility(&app, &inner);
    emit_snapshot(&app, &inner);
    Ok(snapshot(&inner))
}
