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
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::webview::NewWindowResponse;
use tauri::{
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, State, Webview, WebviewUrl, Window,
};
use tauri_plugin_opener::OpenerExt;

use crate::config::{self, DockSide, TabConfig, TabEntry};

/// Shelf width bounds (logical px). At `SHELF_MIN_WIDTH` the shelf is a
/// slim rail of centered logos; at `SHELF_MAX_WIDTH` every entry shows
/// logo + full name. The frontend hides names below its own threshold
/// (120px) — these bounds only govern what Rust accepts and persists.
pub const SHELF_MIN_WIDTH: f64 = 52.0;
pub const SHELF_MAX_WIDTH: f64 = 248.0;

const HOST_LABEL: &str = "main";

/// Preset agents for the + picker (spec §2). Pure data: id + name + start
/// URL, no per-provider code. The id names the bundled logo file
/// (`ui/logos/<id>.png`, v0.0.5a) and is how the shelf matches a tab to a
/// logo. All URLs verified 2026-11-06; see the spec's §2 field notes
/// (Le Chat now brands itself Mistral Vibe, Qwen Chat brands itself Qwen
/// Studio, Doubao is China-first with restricted sign-in, Copilot
/// redirects work accounts to Microsoft 365 Copilot Chat).
/// v0.0.18a — the Screening Room: a LOCAL app page, not a remote
/// agent. Its preset URL is a sentinel, never a network address:
/// `create_webview` branches on it to `WebviewUrl::App("player.html")`
/// (served from `frontendDist` = ui/), and `validate_url` accepts
/// this exact string and no other `app:` URL. tabs.json stores the
/// sentinel, so restore takes the same branch. Phone-orientation
/// rotation for videos is a Muse phone-app concern — a desktop
/// Tauri window has no orientation to follow, so it is deliberately
/// out of scope here.
pub const PLAYER_URL: &str = "app://player";
pub const PLAYER_PAGE: &str = "player.html";

pub const PRESETS: &[(&str, &str, &str)] = &[
    ("muse", "Muse", "https://muse.ai"),
    ("player", "Screening Room", PLAYER_URL),
    ("chatgpt", "ChatGPT", "https://chatgpt.com"),
    ("claude", "Claude", "https://claude.ai"),
    ("gemini", "Gemini", "https://gemini.google.com"),
    ("copilot", "Microsoft Copilot", "https://copilot.microsoft.com"),
    ("grok", "Grok", "https://grok.com"),
    ("perplexity", "Perplexity", "https://www.perplexity.ai"),
    ("meta", "Meta AI", "https://www.meta.ai"),
    ("deepseek", "DeepSeek", "https://chat.deepseek.com"),
    ("qwen", "Qwen", "https://chat.qwen.ai"),
    ("kimi", "Kimi", "https://www.kimi.com"),
    ("lechat", "Le Chat (Mistral)", "https://chat.mistral.ai"),
    ("doubao", "Doubao", "https://www.doubao.com"),
];

#[derive(Clone, Serialize)]
pub struct Preset {
    pub id: String,
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
    #[serde(rename = "shelfWidth")]
    pub shelf_width: f64,
    #[serde(rename = "dockSide")]
    pub dock_side: String,
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
        shelf_width: inner.config.shelf_width,
        dock_side: inner.config.dock_side.as_str().to_string(),
    }
}

fn emit_snapshot(app: &AppHandle, inner: &Inner) {
    let _ = app.emit_to(HOST_LABEL, "tabs-changed", snapshot(inner));
}

fn host_window(app: &AppHandle) -> Result<Window, String> {
    app.get_window(HOST_LABEL)
        .ok_or_else(|| "host window missing".to_string())
}

/// Bounds for a tab's webview: the whole window minus the shelf, in
/// logical units. Docked left, the content starts at x = shelf width;
/// docked right, it starts at x = 0 and the shelf takes the right edge.
fn content_bounds(
    window: &Window,
    side: DockSide,
    shelf_width: f64,
) -> (LogicalPosition<f64>, LogicalSize<f64>) {
    let scale = window.scale_factor().unwrap_or(1.0);
    let physical = window.inner_size().unwrap_or_default();
    let win_w = physical.width as f64 / scale;
    let win_h = physical.height as f64 / scale;
    let x = match side {
        DockSide::Left => shelf_width,
        DockSide::Right => 0.0,
    };
    (
        LogicalPosition::new(x, 0.0),
        LogicalSize::new((win_w - shelf_width).max(50.0), win_h.max(50.0)),
    )
}

/// John's display spelling of the build version, derived from the
/// crate version — the single source of truth (Cargo.toml, kept in
/// step with tauri.conf.json every run): "0.0.26-a" -> "v0.0.26a".
fn display_version() -> String {
    let raw = env!("CARGO_PKG_VERSION");
    match raw.split_once('-') {
        Some((core, suffix)) => format!("v{core}{suffix}"),
        None => format!("v{raw}"),
    }
}

fn set_window_title(app: &AppHandle, inner: &Inner) {
    if let Some(window) = app.get_window(HOST_LABEL) {
        // v0.0.25a: this function has always been the title's real
        // owner — it overwrote the config title on every tab change
        // with "Nubisianua — {name}" (em dash, no version), which is
        // why the running title bar never showed the version John
        // ordered at 0.0.20a even though tauri.conf.json carried it.
        // The composed title now ends with the version, in his
        // format: "Nubisianua -- Muse -- v0.0.26a".
        let version = display_version();
        let title = match &inner.config.active_tab {
            Some(active) => match inner.config.tabs.iter().find(|t| &t.id == active) {
                Some(tab) => format!("Nubisianua -- {} -- {}", tab.name, version),
                None => format!("Nubisianua -- {version}"),
            },
            None => format!("Nubisianua -- {version}"),
        };
        let _ = window.set_title(&title);
    }
}

/// Linux layout pass (v0.0.8a) — the fix for the Dell 2 / Rawhide canary
/// finding, where the shelf rendered "in the top margin only" and the
/// active tab webview filled the whole window below it at full width.
///
/// Diagnosis, from the pinned crates' own sources:
/// - On Linux, tauri-runtime-wry (2.12.3) builds BOTH the host (window
///   content) webview and every `Window::add_child` webview into the
///   window's `default_vbox()` — a vertical `gtk::Box` (the
///   `WebviewKind::WindowChild` arm: "only way to account for menu bar
///   height, and also works for multiwebviews :)").
/// - wry (0.57.1) `add_to_container` honors a webview's bounds ONLY in a
///   `GtkFixed` parent; in a `GtkBox` it does
///   `pack_start(webview, true, true, 0)` and the bounds are ignored.
/// - wry's `set_bounds` — what `Webview::set_position`/`set_size` call —
///   is a no-op on Wayland unless the webview sits in a fixed parent.
///   So on Linux/Wayland `content_bounds`/`apply_visibility` never moved
///   anything: the geometry was pure GtkBox — host and child stacked
///   vertically, both expanding, the child taking the window at full
///   width under a host squeezed to a band at the top. Exactly the field
///   report.
///
/// The fix works WITH the box instead of fighting it, using only public
/// Tauri/GTK APIs, all of it from the GTK main thread:
/// - orient the window's vbox HORIZONTAL, so box order is side-by-side;
/// - the host webview (identified by a widget name set on first pass —
///   it is the box's first child by construction: created first, and
///   children are only ever appended) is re-packed non-expanding via
///   the GtkBox "expand" child property and given an exact size
///   request = the shelf width, so the box hands it precisely the
///   shelf's strip and the active child (still expand=true) takes the
///   rest — the same rectangle `content_bounds` computes on Windows;
/// - dock side becomes box order: host first (left) or last (right);
/// - window resizes and show/hide need no code at all — the box
///   re-lays-out on its own, exactly as `apply_visibility`'s show/hide
///   already drives it.
///
/// The pass is idempotent and re-derives everything from the box's
/// current children, so the calls below (restore start/end, tab
/// creation, shelf width and dock changes) converge under any
/// interleaving with webview creation on the main thread. Nubisianua
/// has no window menu, so the vbox carries webviews only. On Windows
/// (and macOS) this is a no-op stub — geometry there already works.
#[cfg(target_os = "linux")]
fn linux_layout_pass(app: &AppHandle, side: DockSide, shelf_width: f64) {
    use gtk::prelude::*;

    const HOST_WIDGET_NAME: &str = "nubisianua-host";

    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let Some(window) = handle.get_window(HOST_LABEL) else {
            return;
        };
        let Ok(vbox) = window.default_vbox() else {
            return;
        };
        vbox.set_orientation(gtk::Orientation::Horizontal);

        let children = vbox.children();
        // Find the host webview's widget: the one named on an earlier
        // pass, else the box's first child (see the header comment) —
        // name it so later passes find it wherever the dock order put it.
        let host = children
            .iter()
            .find(|w| w.widget_name().as_str() == HOST_WIDGET_NAME)
            .cloned()
            .or_else(|| {
                let first = children.into_iter().next()?;
                first.set_widget_name(HOST_WIDGET_NAME);
                Some(first)
            });
        let Some(host) = host else {
            return;
        };

        // The shelf takes exactly its requested width, never a share of
        // the leftover space.
        vbox.child_set_property(&host, "expand", &false.to_value());
        host.set_size_request(shelf_width.round() as i32, -1);
        match side {
            DockSide::Left => vbox.reorder_child(&host, 0),
            DockSide::Right => vbox.reorder_child(&host, -1),
        }
    });
}

#[cfg(not(target_os = "linux"))]
fn linux_layout_pass(_app: &AppHandle, _side: DockSide, _shelf_width: f64) {}

/// Show the active tab's webview fitted to the content area and hide every
/// other tab's webview. Switching never reloads a page (spec §1).
///
/// v0.0.26a deadlock fix (Kubuntu/X11 field report): `content_bounds`
/// calls `Window::scale_factor`/`inner_size`, which from any non-main
/// thread round-trip to the main thread and block until it answers.
/// Every old call site held `state.inner` across this call — and the
/// main thread's resize handler (`layout`, via `on_window_event`)
/// blocks on that same lock. Startup on X11 fires a resize while
/// `restore` holds the lock and waits on the main thread: both sides
/// wait forever, the window stays black, the process dies. So this
/// function takes `state`, not a locked `&Inner`: it reads the layout
/// inputs under a SHORT lock, drops it, computes bounds with no lock
/// held, and only then re-locks to apply. No lock is ever held across
/// a main-thread round-trip, from either side. Callers must never hold
/// the guard across this call — drop it first.
fn apply_visibility(app: &AppHandle, state: &State<'_, TabManager>) {
    let (side, shelf_width) = {
        let inner = state.inner.lock().unwrap();
        (inner.config.dock_side, inner.config.shelf_width)
    };
    let Ok(window) = host_window(app) else {
        return;
    };
    let bounds = content_bounds(&window, side, shelf_width);
    let inner = state.inner.lock().unwrap();
    let (pos, size) = bounds;
    for (id, webview) in &inner.webviews {
        if Some(id) == inner.config.active_tab.as_ref() {
            let _ = webview.set_position(pos);
            let _ = webview.set_size(size);
            let _ = webview.show();
        } else {
            let _ = webview.hide();
        }
    }
    set_window_title(app, &inner);
}

/// Same-origin test for new-window routing (v0.0.11a): scheme + host +
/// port all equal. Origins are what browsers compare; subdomains count
/// as different origins, which is the behavior we want here.
fn same_origin(a: &tauri::Url, b: &tauri::Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// Short origin label for the v0.0.12a link diagnostics
/// (`scheme://host:port`, or `scheme(no-host)` for about:blank & co).
fn origin_label(url: &tauri::Url) -> String {
    match url.host_str() {
        Some(host) => match url.port_or_known_default() {
            Some(port) => format!("{}://{host}:{port}", url.scheme()),
            None => format!("{}://{host}", url.scheme()),
        },
        None => format!("{}(no-host)", url.scheme()),
    }
}

/// v0.0.13a: if `url`'s fragment carries the marker the in-page
/// interception script appended, return the clean URL to hand to the
/// OS browser; otherwise None. The script only ever appends the
/// marker in two shapes — as the whole fragment (`#nubisianua-ext`,
/// when the link had no fragment of its own) or as a `&nubisianua-ext`
/// suffix on an existing fragment — so strip exactly those; any other
/// occurrence is removed as a bare substring, so a marked navigation
/// can never slip past the gate half-cleaned.
fn strip_ext_marker(url: &tauri::Url) -> Option<tauri::Url> {
    const MARKER: &str = "nubisianua-ext";
    let fragment = url.fragment()?;
    if !fragment.contains(MARKER) {
        return None;
    }
    let clean_fragment: Option<String> = if fragment == MARKER {
        None
    } else if let Some(prefix) = fragment.strip_suffix(&format!("&{MARKER}")) {
        if prefix.is_empty() {
            None
        } else {
            Some(prefix.to_string())
        }
    } else {
        Some(fragment.replacen(MARKER, "", 1))
    };
    let mut clean = url.clone();
    clean.set_fragment(clean_fragment.as_deref());
    Some(clean)
}

/// Is this path an image? Extension first, magic bytes as the
/// backstop (a screenshot saved without an extension still counts).
/// The only files ever opened here are ones the user's own drag
/// delivered in a native drop event — see `handle_drag_drop`.
fn is_image_path(path: &std::path::Path) -> bool {
    use std::io::Read as _;

    let ext_ok = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            matches!(
                e.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
            )
        })
        .unwrap_or(false);
    if ext_ok {
        return true;
    }
    let mut buf = [0u8; 12];
    let n = std::fs::File::open(path)
        .and_then(|mut f| f.read(&mut buf))
        .unwrap_or(0);
    let b = &buf[..n];
    (n >= 4 && b[..4] == *b"\x89PNG")
        || (n >= 3 && b[..3] == *b"\xff\xd8\xff")
        || (n >= 4 && b[..4] == *b"GIF8")
        || (n >= 2 && b[..2] == *b"BM")
        || (n >= 12 && b[..4] == *b"RIFF" && b[8..12] == *b"WEBP")
}

/// How a `wl_copy` hand-off can fail. `Missing` is kept apart
/// from an ordinary failure because the field can fix it without
/// a rebuild — it gets its own self-explaining log line.
#[cfg(target_os = "linux")]
enum WlCopyError {
    Missing,
    Failed(String),
}

/// Pipe `bytes` to `wl-copy`'s stdin (the wl-clipboard tool) and
/// reap the process. wl-copy forks a daemon that keeps holding
/// the clipboard offer after the foreground process exits, so a
/// clean exit status here means the offer changed hands into a
/// process whose whole job is to persist it — the model KWin's
/// clipboard respects on Wayland. Closing stdin is what tells
/// wl-copy the payload is complete; `wait()` then collects the
/// foreground process, which exits as soon as the daemon half
/// has taken over the offer.
#[cfg(target_os = "linux")]
fn wl_copy(args: &[&str], bytes: &[u8]) -> Result<(), WlCopyError> {
    use std::io::Write as _;

    let mut child = std::process::Command::new("wl-copy")
        .args(args)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                WlCopyError::Missing
            } else {
                WlCopyError::Failed(format!("could not spawn wl-copy: {e}"))
            }
        })?;
    if let Some(mut stdin) = child.stdin.take() {
        let written = stdin.write_all(bytes);
        // Drop the handle either way — a child left waiting on an
        // open stdin would never finish, failed write or not.
        drop(stdin);
        written
            .map_err(|e| WlCopyError::Failed(format!("could not write to wl-copy: {e}")))?;
    }
    let status = child
        .wait()
        .map_err(|e| WlCopyError::Failed(format!("could not wait on wl-copy: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(WlCopyError::Failed(format!(
            "wl-copy exited with {status}"
        )))
    }
}

/// One place for the two shapes a wl-copy failure takes in the
/// log: the missing-binary line explains itself to the field,
/// everything else wears the usual FAILED tag.
#[cfg(target_os = "linux")]
fn log_wl_copy_error(tag: &str, err: &WlCopyError, context: &str) {
    match err {
        WlCopyError::Missing => {
            eprintln!(
                "[nubisianua-dragdrop] {tag}: wl-copy not found — install the wl-clipboard package"
            )
        }
        WlCopyError::Failed(e) => {
            eprintln!("[nubisianua-dragdrop] {tag}: clipboard FAILED: {e} ({context})")
        }
    }
}

/// The drop pipeline behind the native drag-drop events.
/// `handle_drag_drop` (below) is the single funnel every drag-drop
/// event in the app flows through; on a Drop over the content area
/// this is the clipboard half. Pick the FIRST dropped path that is
/// an existing regular file AND an image and put that image on the
/// system clipboard; if none is an image, put the uri-list text
/// itself on the clipboard instead (a pasted path is still more
/// useful than silence). Only paths the user's own drag delivered
/// are ever opened.
///
/// v0.0.15a ran this act through the GTK3 clipboard API, and the
/// field gave its verdict through Klipper: John's clipboard
/// history held text entries only — no image anywhere — and
/// Ctrl+V in the page came up empty. GTK3's Clipboard is a weak
/// bridge on Wayland: `set_image` returned without complaint and
/// the offer simply never landed. v0.0.16a replaces the COURIER,
/// nothing else: the payload is piped to `wl-copy` (above), a
/// Wayland-native holder whose persistence model KWin respects.
/// Two consequences worth recording: the act no longer needs the
/// GTK main thread, so it runs directly in the event funnel (the
/// Pixbuf load stays — for validation, dimensions, and the
/// in-memory PNG re-encode that feeds wl-copy's stdin); and the
/// app now leans on an external tool, so the Linux packages
/// declare a `wl-clipboard` dependency and a missing binary logs
/// its own remedy.
///
/// v0.0.17a: the act now reports its outcome, because the funnel
/// follows a successfully loaded image with an auto-paste (see
/// `auto_paste_after_drop`). Only the image branch's wl-copy
/// success earns `ImageLoaded`; the uri-list fallback, a failed
/// hand-off, or an unloadable image are all `NotImage` — the
/// clipboard stands as it was left and no trigger is pulled.
#[cfg(target_os = "linux")]
#[derive(PartialEq, Eq)]
enum DropClipboard {
    /// wl-copy took the image: the clipboard holds a paste-able
    /// picture and an auto-paste may follow.
    ImageLoaded,
    /// Anything else happened — no paste is attempted.
    NotImage,
}

#[cfg(target_os = "linux")]
fn drop_clipboard_linux(tag: &str, paths: &[std::path::PathBuf]) -> DropClipboard {
    let image = paths
        .iter()
        .find(|p| p.is_file() && is_image_path(p))
        .cloned();
    let count = paths.len();
    match image {
        Some(path) => {
            eprintln!(
                "[nubisianua-dragdrop] {tag}: drop received: {count} path(s); loading image {} for the clipboard",
                path.display()
            );
            match gtk::gdk_pixbuf::Pixbuf::from_file(&path) {
                Ok(pixbuf) => {
                    let (w, h) = (pixbuf.width(), pixbuf.height());
                    match pixbuf.save_to_bufferv("png", &[]) {
                        Ok(bytes) => {
                            let nbytes = bytes.len();
                            match wl_copy(&["--type", "image/png"], &bytes) {
                                Ok(()) => {
                                    eprintln!(
                                        "[nubisianua-dragdrop] {tag}: clipboard <- image {} ({w}x{h}) [via wl-copy, image/png, {nbytes} bytes]",
                                        path.display()
                                    );
                                    return DropClipboard::ImageLoaded;
                                }
                                Err(e) => log_wl_copy_error(
                                    tag,
                                    &e,
                                    &format!("image was {}", path.display()),
                                ),
                            }
                        }
                        Err(e) => eprintln!(
                            "[nubisianua-dragdrop] {tag}: clipboard FAILED: {e} (could not re-encode {} as PNG)",
                            path.display()
                        ),
                    }
                }
                Err(e) => eprintln!(
                    "[nubisianua-dragdrop] {tag}: clipboard FAILED: {e} (image was {})",
                    path.display()
                ),
            }
        }
        None => {
            let text = paths
                .iter()
                .map(|p| {
                    tauri::Url::from_file_path(p)
                        .map(|u| u.to_string())
                        .unwrap_or_else(|_| p.display().to_string())
                })
                .collect::<Vec<String>>()
                .join("\n");
            match wl_copy(&["--type", "text/uri-list"], text.as_bytes()) {
                Ok(()) => eprintln!(
                    "[nubisianua-dragdrop] {tag}: no image in drop; clipboard <- uri-list text ({count} path(s)) [via wl-copy]"
                ),
                Err(e) => log_wl_copy_error(tag, &e, "uri-list text was not delivered"),
            }
        }
    }
    DropClipboard::NotImage
}

/// v0.0.17a auto-paste, step A — the focus aim, evaluated in the
/// dropped-on tab just before the native paste command fires.
/// John's flow drops a screenshot on a tab and then talks to
/// that agent; the paste must land in the tab's composer (its
/// chat input), which usually — but not always — already holds
/// focus. The aim is deliberately a generic heuristic for chat
/// pages, not a per-site selector: if focus already sits in an
/// editable element (a textarea, a text-ish input, anything
/// contentEditable) it is left exactly where the user put it;
/// otherwise the largest visible candidate — a
/// `contenteditable` element (Muse's composer is a ProseMirror
/// one) or a textarea (ChatGPT's shape) — is focused. Fully
/// try/catch wrapped: an aim failure is a silent no-op, because
/// the clipboard stays loaded and the manual Ctrl+V fallback
/// stands regardless. Like every `eval` in this app, this runs
/// from the Rust side as an embedder power — the page gains no
/// IPC and the tab keeps zero capabilities.
#[cfg(target_os = "linux")]
const AUTO_PASTE_FOCUS_SCRIPT: &str = r##"
(() => {
  try {
    function isEditable(el) {
      if (!el) return false;
      if (el.isContentEditable) return true;
      if (el.tagName === 'TEXTAREA') return true;
      if (el.tagName === 'INPUT') {
        var t = (el.getAttribute('type') || 'text').toLowerCase();
        return ['text', 'search', 'url', 'email', 'password', 'tel', 'number', '']
          .indexOf(t) !== -1;
      }
      return false;
    }
    if (isEditable(document.activeElement)) return;
    var nodes = document.querySelectorAll(
      '[contenteditable="true"], [contenteditable=""], [contenteditable="plaintext-only"], textarea'
    );
    var best = null;
    var bestArea = 0;
    for (var i = 0; i < nodes.length; i++) {
      var r = nodes[i].getBoundingClientRect();
      if (r.width <= 0 || r.height <= 0) continue;
      var area = r.width * r.height;
      if (area > bestArea) {
        bestArea = area;
        best = nodes[i];
      }
    }
    if (best) best.focus();
  } catch (e) {
    /* Swallowed on purpose — see the header comment. */
  }
})();
"##;

/// v0.0.17a auto-paste — John's refinement of the bridge, in his
/// words: "now that we have it landing can we have the software
/// do the pasting?" 0.0.16a proved the whole chain with a human
/// pulling the trigger (drop → wl-copy holds the image → Ctrl+V
/// attaches it), so this round only removes the human from that
/// last step; the flow becomes drop-only.
///
/// Two steps, both Rust-side embedder powers on the dropped-on
/// tab (the page gains no IPC; tabs keep zero capabilities):
/// step A evaluates `AUTO_PASTE_FOCUS_SCRIPT` to aim the focus
/// at the composer; step B, after a ~120 ms beat on the GTK main
/// context so the focus lands first, calls WebKit's own Paste
/// editing command on the tab's `webkit2gtk::WebView`, reached
/// through Tauri's `Webview::with_webview` →
/// `PlatformWebview::inner()` (the pinned tauri-2.12.5 sources
/// document exactly this path, webview/mod.rs:152-178 and the
/// `with_webview` example). The command is the very pipeline a
/// physical Ctrl+V keystroke resolves to inside WebKitGTK, so
/// the page receives a genuine paste event whose DataTransfer
/// carries the clipboard's image as a File and the composer
/// attaches it — the software pulls the same trigger the user's
/// fingers did.
///
/// The clipboard load ALWAYS ran first and stands regardless:
/// if the aim missed or the page ignores the paste, the image
/// is still on the clipboard and the proven manual Ctrl+V
/// remains. A missed auto-paste costs nothing but the keystroke
/// it tried to save.
#[cfg(target_os = "linux")]
fn auto_paste_after_drop(app: &AppHandle, state: &State<'_, TabManager>, tag: &str) {
    // The only tab visible in the content area is the active
    // one, so it is necessarily the tab the drop landed on.
    let webview = {
        let inner = state.inner.lock().unwrap();
        inner
            .config
            .active_tab
            .as_ref()
            .and_then(|id| inner.webviews.get(id).cloned())
    };
    let Some(webview) = webview else {
        eprintln!(
            "[nubisianua-dragdrop] {tag}: auto-paste skipped — dropped-on tab webview not found; clipboard stays loaded for Ctrl+V"
        );
        return;
    };
    // Step A — aim the focus in-page.
    let _ = webview.eval(AUTO_PASTE_FOCUS_SCRIPT);
    // Step B — pull the paste trigger natively, after a beat.
    // The glib timeout is registered from the main thread (the
    // same run_on_main_thread idiom as the Linux layout pass),
    // so its 120 ms are measured from the main loop's own clock,
    // after the eval dispatched above has had its turn.
    let tag = tag.to_string();
    let _ = app.run_on_main_thread(move || {
        gtk::glib::timeout_add_once(std::time::Duration::from_millis(120), move || {
            use webkit2gtk::WebViewExt as _;
            let _ = webview.with_webview(|platform| {
                // "Paste" is WEBKIT_EDITING_COMMAND_PASTE, the
                // literal the WebKit C header macro expands to
                // (webkit2gtk-sys 2.0.2 carries it as a byte
                // string; the editing-command API takes the str).
                platform.inner().execute_editing_command("Paste");
            });
            eprintln!(
                "[nubisianua-dragdrop] {tag}: auto-paste dispatched (native editing command, after focus aim)"
            );
        });
    });
}

/// Shelf/content geometry for the v0.0.15a drop guard, in physical
/// pixels (the units drag-drop event positions arrive in): the
/// shelf strip is `shelf_width` logical px scaled by the window's
/// scale factor, hugging the dock edge; the content area is
/// everything else. Mirrors `content_bounds`, which computes the
/// same split in logical units for webview placement.
fn drop_over_content(
    window: &Window,
    side: DockSide,
    shelf_width: f64,
    window_pos: &tauri::PhysicalPosition<f64>,
) -> bool {
    let scale = window.scale_factor().unwrap_or(1.0);
    let inner = window.inner_size().unwrap_or_default();
    let win_w = inner.width as f64;
    let win_h = inner.height as f64;
    if window_pos.y < 0.0 || window_pos.y >= win_h {
        return false;
    }
    let shelf_phys = shelf_width * scale;
    match side {
        DockSide::Left => window_pos.x >= shelf_phys && window_pos.x < win_w,
        DockSide::Right => window_pos.x >= 0.0 && window_pos.x < (win_w - shelf_phys).max(0.0),
    }
}

/// `Over` events arrive on every mouse move during a drag; logging
/// each one would bury the terminal. Throttle them to one line per
/// 500 ms — every variant still testifies, Enter/Drop/Leave are
/// never throttled.
static LAST_OVER_LOG_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn over_log_due() -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let last = LAST_OVER_LOG_MS.load(std::sync::atomic::Ordering::Relaxed);
    now.saturating_sub(last) >= 500
        && LAST_OVER_LOG_MS
            .compare_exchange(last, now, std::sync::atomic::Ordering::Relaxed, std::sync::atomic::Ordering::Relaxed)
            .is_ok()
}

/// v0.0.15a: the single funnel for native drag-drop events.
///
/// Why native, and why TWO adapters feed this funnel: the
/// 0.0.14a field test proved the in-page drop arm starves —
/// WebKitGTK will not let page JavaScript read an external drop's
/// payload (`getData('text/uri-list')` comes back empty at drop
/// time; John's cursor showed the widget's green + accept, but
/// the Muse page never even raised its "drag files here"
/// overlay). The uri-list demonstrably exists one layer down, at
/// the GTK widget, where wry's own drag-drop handler reads it —
/// so the interception moves there. In the pinned runtime
/// (tauri-runtime-wry 2.12.3, lib.rs:4783-4812) that handler is
/// attached per webview when `drag_drop_handler_enabled` (the
/// default — hence this round REMOVES both 0.0.11a/0.0.12a
/// disables) and forwards the event according to webview kind:
/// the host webview (WindowContent) becomes a synthesized
/// `WindowEvent::DragDrop`, each tab webview (WindowChild)
/// becomes a `WebviewEvent::DragDrop` on that webview. The two
/// registrations in this module — `Window::on_window_event` on
/// the host window (installed once by `restore`) and
/// `Webview::on_webview_event` per tab (in `create_webview`) —
/// normalize their different position frames into window-relative
/// physical pixels and meet here.
///
/// `tag` names the surface for the log lines ("tab t1" / "shelf").
/// A Drop is answered with the clipboard bridge only when
/// `drop_over_content` places it in the content area; a drop over
/// the shelf is logged and ignored — the shelf is our own UI, and
/// feeding its drops back as pastes would be a surprise.
fn handle_drag_drop(
    app: &AppHandle,
    state: &State<'_, TabManager>,
    tag: &str,
    event: &tauri::DragDropEvent,
    window_pos: tauri::PhysicalPosition<f64>,
) {
    use tauri::DragDropEvent;
    match event {
        DragDropEvent::Enter { paths, .. } => {
            eprintln!(
                "[nubisianua-dragdrop] {tag}: drag enter: {} path(s) at ({:.0}, {:.0}) [window-relative, physical px]",
                paths.len(),
                window_pos.x,
                window_pos.y
            );
            for p in paths {
                eprintln!("[nubisianua-dragdrop] {tag}:   enter path: {}", p.display());
            }
        }
        DragDropEvent::Over { .. } => {
            if over_log_due() {
                eprintln!(
                    "[nubisianua-dragdrop] {tag}: drag over at ({:.0}, {:.0}) [window-relative, physical px]",
                    window_pos.x, window_pos.y
                );
            }
        }
        DragDropEvent::Drop { paths, .. } => {
            let Ok(window) = host_window(app) else {
                eprintln!("[nubisianua-dragdrop] {tag}: drop with no host window — ignored");
                return;
            };
            let (side, shelf_width) = {
                let inner = state.inner.lock().unwrap();
                (inner.config.dock_side, inner.config.shelf_width)
            };
            if !drop_over_content(&window, side, shelf_width, &window_pos) {
                eprintln!(
                    "[nubisianua-dragdrop] {tag}: drop over shelf — ignored ({} path(s) at ({:.0}, {:.0}))",
                    paths.len(),
                    window_pos.x,
                    window_pos.y
                );
                return;
            }
            #[cfg(target_os = "linux")]
            {
                // v0.0.17a: a loaded image earns an auto-paste on
                // the dropped-on tab; anything else (uri-list
                // fallback, a failed hand-off) pulls no trigger.
                if drop_clipboard_linux(tag, paths) == DropClipboard::ImageLoaded {
                    auto_paste_after_drop(app, state, tag);
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = paths;
                eprintln!(
                    "[nubisianua-dragdrop] {tag}: drop over content on unsupported platform — clipboard bridge is Linux-only, ignored"
                );
            }
        }
        DragDropEvent::Leave => {
            eprintln!("[nubisianua-dragdrop] {tag}: drag leave");
        }
        _ => {}
    }
}

/// Whether the host-window drag-drop adapter has been installed
/// (restore runs once, but the guard keeps the invariant obvious).
static HOST_DROP_ADAPTER: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// v0.0.15a host adapter: drops over the shelf surface arrive as
/// `WindowEvent::DragDrop` (the host webview is WindowContent —
/// see `handle_drag_drop`'s header). The event position is
/// relative to the host widget; on Linux that widget's allocation
/// IS the shelf strip (0.0.8a's GtkBox pass), so window-relative x
/// is the shelf's own origin plus the event x. Elsewhere the host
/// webview fills the window and the position is already
/// window-relative. Either way the funnel's content test lands a
/// shelf drop on 'drop over shelf — ignored'.
fn install_host_drag_drop_adapter(app: &AppHandle) {
    use std::sync::atomic::Ordering;
    if HOST_DROP_ADAPTER.swap(true, Ordering::Relaxed) {
        return;
    }
    let Some(window) = app.get_window(HOST_LABEL) else {
        return;
    };
    let handle = app.clone();
    window.on_window_event(move |event| {
        let tauri::WindowEvent::DragDrop(dd) = event else {
            return;
        };
        let position = match dd {
            tauri::DragDropEvent::Enter { position, .. }
            | tauri::DragDropEvent::Over { position }
            | tauri::DragDropEvent::Drop { position, .. } => *position,
            _ => tauri::PhysicalPosition::new(0.0, 0.0),
        };
        let state = handle.state::<TabManager>();
        let (side, shelf_width) = {
            let inner = state.inner.lock().unwrap();
            (inner.config.dock_side, inner.config.shelf_width)
        };
        let Ok(window) = host_window(&handle) else {
            return;
        };
        let scale = window.scale_factor().unwrap_or(1.0);
        let origin_x = {
            #[cfg(target_os = "linux")]
            {
                let win_w = window.inner_size().unwrap_or_default().width as f64;
                let shelf_phys = shelf_width * scale;
                match side {
                    DockSide::Left => 0.0,
                    DockSide::Right => (win_w - shelf_phys).max(0.0),
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (side, shelf_width, scale);
                0.0
            }
        };
        let window_pos = tauri::PhysicalPosition::new(position.x + origin_x, position.y);
        handle_drag_drop(&handle, &state, "shelf", dd, window_pos);
    });
    eprintln!(
        "[nubisianua-dragdrop] host window: native drag-drop adapter installed (WindowEvent::DragDrop)"
    );
}

/// v0.0.13a: the in-page external-link interceptor, injected into
/// every tab webview as an initialization script (main frame only,
/// at document start, via the embedder's user-script mechanism —
/// privileged embedder context, not subject to the page's CSP).
///
/// Why it exists: the 0.0.12a field test proved the native
/// new-window path is a dead end on Linux/WebKitGTK — clicking an
/// external link in the Muse tab produced total silence: no
/// new-window request, no navigation event. The request dies inside
/// WebKitGTK before any native hook sees it. So the interception
/// moves into the page, where the click is still visible:
///
/// - a capture-phase document click listener finds the anchor
///   (`e.target.closest('a[href]')`); for http(s) hrefs whose origin
///   differs from the page's, or whose target opens a new context
///   (anything but `_self`), it preventDefaults the click and
///   navigates THIS frame to the same URL with the `#nubisianua-ext`
///   marker appended to the fragment;
/// - `window.open` is wrapped the same way for cross-origin http(s)
///   URLs (marked navigate + return null); same-origin and
///   non-http calls pass through to the real `window.open`;
/// - the Rust `on_navigation` gate recognises the marker, strips
///   it, hands the clean URL to the OS browser, and blocks the
///   navigation — the tab stays on its own page.
///
/// v0.0.14a added a second arm to this script — an in-page
/// drop-to-clipboard bridge that read the drop's file:// entries
/// and passed them up through a sentinel navigation. v0.0.15a
/// REMOVES that arm, plainly: the 0.0.14a field test fired
/// nothing at all. WebKitGTK starves page JavaScript of external
/// drop payloads (`getData('text/uri-list')` yields nothing
/// usable at drop time), and the arm's `dragover` preventDefault
/// could only risk interfering with the native path that
/// replaces it — Tauri's own drag-drop event, handled natively
/// in this module (see `handle_drag_drop`). Only the link arm
/// remains, byte-identical to 0.0.13a.
///
/// The script uses NO Tauri APIs and no IPC (the zero-IPC invariant
/// holds): it only navigates its own frame. Everything is wrapped
/// defensively — a global flag makes double-injection a no-op, and
/// any throw is swallowed so page JS can never be broken by it.
const EXT_LINK_SCRIPT: &str = r#"
(function () {
  try {
    if (window.__nubisianuaExtLink) return;
    window.__nubisianuaExtLink = true;

    function isHttp(u) {
      return u.protocol === 'http:' || u.protocol === 'https:';
    }

    function marked(u) {
      if (!u.hash) {
        u.hash = 'nubisianua-ext';
      } else {
        u.hash = u.hash.slice(1) + '&nubisianua-ext';
      }
      return u.href;
    }

    function handOff(u) {
      try {
        location.assign(marked(u));
      } catch (e) { /* never break the page */ }
    }

    document.addEventListener('click', function (e) {
      try {
        var t = e.target;
        if (!t || typeof t.closest !== 'function') return;
        var a = t.closest('a[href]');
        if (!a) return;
        var u;
        try {
          u = new URL(a.href);
        } catch (err) {
          return;
        }
        if (!isHttp(u)) return;
        var target = (a.target || '').toLowerCase();
        var newContext = target !== '' && target !== '_self';
        if (!newContext && u.origin === location.origin) return;
        e.preventDefault();
        e.stopPropagation();
        handOff(u);
      } catch (e2) { /* never break the page */ }
    }, true);

    var origOpen = window.open;
    if (typeof origOpen === 'function') {
      window.open = function (url) {
        try {
          if (typeof url === 'string' && url) {
            var u = new URL(url, location.href);
            if (isHttp(u) && u.origin !== location.origin) {
              handOff(u);
              return null;
            }
          }
        } catch (e) { /* fall through to the original */ }
        return origOpen.apply(window, arguments);
      };
    }

  } catch (e) { /* never break the page */ }
})();
"#;

/// v0.0.19a: Nubisianua's OWN player, in the chat — the correction
/// to 0.0.18a, from John's field verdict on that build, which he
/// built and installed on Windows himself. Verdict, in substance:
/// the Screening Room player sat in its tab and worked there, but
/// a video posted in the chat never used it — chat videos still
/// came up in the same old browser/Muse player, whose picture
/// plays ~1 second (the cold-open frame) and freezes while the
/// audio continues, which starts muted, which cannot go
/// fullscreen, and in which closed captions CANNOT be turned off
/// (there is no caption toggle at all). John's exact want: "the
/// player that is in the tab, into my chat" as the DEFAULT player
/// in the chat — when Albert posts a video in the Muse tab it
/// must come up IN PLACE in Nubisianua's own player UI.
///
/// So this run replaces the 0.0.18a enhancer (which only tweaked
/// the native <video>: volume, resume, a double-click and one
/// overlay button) with a full custom control bar built by this
/// script and attached to each remote-page <video> in place —
/// the Screening Room's controls (ui/player.html), adapted for
/// a video that lives inside someone else's page:
///
/// - Play/pause: our own button, plus click-on-video toggles
///   (single-click is debounce-guarded so a double-click aimed
///   at fullscreen doesn't also toggle, and so a toggle the
///   page's own player already performed isn't toggled back).
/// - Seek bar + time display (current / duration).
/// - Volume: NOT muted by default. The global preference kept
///   in localStorage `nubisianua-player-volume` (JSON
///   {volume, muted, byBar} — the 0.0.18a key, unchanged;
///   the byBar provenance stamp is v0.0.23a, see below) is applied
///   on `loadedmetadata`, `canplay`, `play`, and `playing`;
///   with no saved preference the default is volume 1, muted
///   false. A short, bounded grace window after each `play`
///   re-asserts exactly that saved state if the page's own
///   player overrides it at startup. Our mute button and
///   volume slider feed the same preference, and touching
///   either ends enforcement for that source.
/// - Closed captions: a CC toggle button. Default OFF
///   (localStorage `nubisianua-player-cc`, '1' = on). Toggling
///   sets every caption/subtitle textTrack's mode — 'showing'
///   for the first such track when on, 'disabled' for all when
///   off — and is re-applied on `loadedmetadata` and whenever
///   the track list grows (`addtrack`), because tracks often
///   only appear after metadata. Only mode changes are written
///   (setting a track to the mode it already has is a no-op we
///   skip), so no change-event churn.
/// - Playback speed: a selector (0.5 / 0.75 / 1 / 1.25 / 1.5 /
///   2), persisted in `nubisianua-player-rate`, applied to
///   each enhanced video.
/// - Fullscreen: a real button on the bar, plus double-click.
///   It fullscreens the player container (the video's parent,
///   when our bar is overlaid on it — the wrapper case), and
///   falls back to the <video> element itself otherwise.
/// - Resume position: the 0.0.18a behaviour, kept — per-video
///   localStorage (`nubisianua-player-pos:<hash>`, src with the
///   signed query/fragment stripped, then FNV-1a hashed),
///   saved throttled on `timeupdate` and on `pause`, cleared
///   when a video ends or reaches its last 5 s.
///
/// FREEZE DIAGNOSIS (0.0.18a, read before rewriting): three
/// things in the old enhancer could themselves stall or fight
/// playback, and all three are designed out here —
/// (1) It called applyVolPref on EVERY `play` event: if the
///     page's player re-mutes as part of its own play flow,
///     enhancer and page fight over `muted`/`volume` for the
///     life of the video. 0.0.19a therefore applied volume
///     only once per source, on `loadedmetadata`, and never
///     on `play`. 0.0.22a adds a deliberately bounded
///     exception, not a return to the lifelong fight: field
///     evidence showed the page overriding that one early
///     application at playback start, so re-assertion now
///     lasts only for a ~5-second grace window after `play`
///     (and stops sooner once the saved state holds, or as
///     soon as John uses our own volume controls).
/// (2) It called restorePos from `durationchange` as well as
///     `loadedmetadata`, with its done-flag only set after a
///     seek attempt ran with a valid duration — repeated
///     duration changes (streaming/MSE sources do this) could
///     produce repeated seeks. A seek into an unbuffered range
///     is exactly the shape of "picture stalls, audio (buffered
///     separately) carries on". Here restore runs at most once
///     per video source: the key is marked restored BEFORE the
///     seek is attempted, `durationchange` never seeks at all,
///     and out-of-range saves simply don't seek.
/// (3) Its `seeked` listener saved position — every restore
///     seek (and every scrub) fed straight back into storage
///     writes and re-derived keys. Here `seeked` has no
///     listener; saving is timeupdate-throttled + pause only.
/// Honest remainder: the ~1-second freeze itself was reported
/// against the OLD browser player BEFORE 0.0.18a existed, so
/// the enhancer cannot be its sole cause — but 0.0.19a is
/// built so it cannot add a freeze of its own, and its
/// picture comes from the same <video> element, untouched:
/// we never clone or replace the element, never touch its
/// `src`, and never move it in the DOM (moving a media element
/// can restart its resource loading — the bar is inserted as
/// an overlay on, or a sibling after, the video instead).
///
/// v0.0.20a: the scan pierces. John's field verdict on
/// 0.0.19a was the same as on 0.0.18a — "still the original
/// in app player", no Nubisianua bar at all — and code
/// inspection found why that could be: 0.0.19a's scan() only
/// searched the main document (`querySelectorAll('video')`),
/// so a <video> living inside a shadow root or a same-origin
/// iframe was invisible to it. The scan below is now a
/// recursive walk: it descends into every open shadow root
/// (plus any root captured by an `attachShadow` hook
/// installed before the page's own scripts run, which also
/// catches closed roots), scans same-origin iframe documents
/// (cross-origin frames are skipped silently on access
/// error), observes shadow/iframe roots for later mutations,
/// and re-walks the whole tree every few seconds so
/// late-attached shadow content is still found. A video
/// found in a shadow root or an iframe gets its own
/// `[nubisianua-player]` log line saying where it was found.
/// The window title also carries the version in this run
/// ("Nubisianua -- Muse -- v0.0.20a"), so the build John is
/// looking at is identifiable at a glance. NOTE: this is a
/// code-level fix only — whether Muse's chat videos actually
/// live in a shadow root / iframe, and whether the bar now
/// appears in the field, is for John's Windows test to prove.
///
/// v0.0.21a: John's inspector screenshots (2026-11-08) settled
/// where the chat videos live — and it is NOT a shadow root or
/// an iframe. Both the preview bubble and the opened player are
/// in the MAIN document: a plain
/// `<video playsinline loop src="blob:https://muse.ai/...">`
/// inside a `data-hatch-video-wrapper="true"` div, under
/// `data-testid="hatch-chat-attachment-presentation-video"`,
/// and the opened player is that same wrapper (relative,
/// overflow-hidden, 16/9) inside a Radix "Media preview"
/// dialog, with Muse's own Pause / progress-slider / Unmute
/// controls in a pointer-events overlay. The shadow/iframe
/// hypothesis is CLOSED — the piercing scan above stays
/// (harmless, helps other sites), but it was never the fix.
/// The remaining suspects were: (a) this script never executes
/// in the Windows WebView2 tab — the link script PROVABLY
/// executes there (0.0.17a field test), but it was a FIRST
/// `initialization_script` call and this enhancer rode a
/// SECOND call, on an accumulate assumption that was never
/// field-verified; and (b) it executes but the bar fails or
/// hides on this exact wrapper structure. 0.0.21a answers
/// both: the Rust side now makes exactly ONE
/// `initialization_script` call per remote tab whose body is
/// the link interceptor followed by this enhancer; videos
/// inside a `[data-hatch-video-wrapper]` ancestor are
/// first-class targets whose bar is appended to the wrapper
/// itself (absolute, bottom, high z-index) so the wrapper's
/// overflow-hidden cannot clip it away; and the enhancer arm
/// plants a visible diagnostic badge ("NUB player active",
/// fixed corner, pointer-events none) so John can SEE
/// whether the script executed at all, separate from whether
/// a bar attaches — the badge is the deciding field test.
///
/// v0.0.22a: 0.0.21a passed John's Windows field test — the
/// bar attaches in the opened player, fullscreen / seek /
/// speed / CC all work, the picture advances, and our bar's
/// unmute button unmutes and holds. His verdict: "everything
/// else is working", with ONE defect left, in his words:
/// "it need to start unmuted". Diagnosis (suggested, and
/// consistent with every field observation): the saved
/// volume was applied at `loadedmetadata`, then Muse's own
/// player set `video.muted = true` as part of starting
/// playback, after that application; nothing re-muted after
/// John's own unmute click, so the override is an initial-
/// start behaviour, not a continuing one. This run is the
/// single-purpose fix: apply the saved state on
/// `loadedmetadata`, `canplay`, `play`, and `playing`, then
/// defend it for a bounded ~5-second grace window after
/// each `play` — checks every ~250 ms, ending early once
/// the saved state has held for several consecutive checks,
/// and ending at once if John touches our mute button or
/// volume slider. A `volumechange` guard inside the window
/// sends external overrides back to the saved state, while
/// changes stamped as our own application or our bar's
/// controls are left alone (the bar's choices also become
/// the new saved state). Applying an unmuted saved state
/// also removes the `muted` content attribute and clears
/// `defaultMuted`. The saved state is enforced exactly as
/// saved: a saved-muted preference still starts muted.
/// After the window closes, later changes are John's
/// business and are no longer fought.
///
/// The same run carries a second fix, from evidence John
/// brought minutes after the 0.0.21a pass: in Nubisianua his
/// Muse presence-card avatar wore a cropped strip of our
/// bar (play button, gold seek thumb, a "0" time fragment),
/// because the avatar's animation is itself a looping
/// `<video>` and 0.0.21a's enhancer took every video it
/// found; in the browser, with no enhancer, the same avatar
/// is clean. `enhance()` is now gated on the one structure
/// John's inspection proved both real chat videos share —
/// a `[data-hatch-video-wrapper]` ancestor (the preview
/// bubble and the opened "Media preview" player). A video
/// without that ancestor is skipped entirely: no bar, no
/// volume or resume meddling, not even the dataset mark, so
/// the periodic re-walk can still adopt it if it gains a
/// wrapper later. The gate applies wherever the walk finds
/// the video, shadow roots and iframes included; the badge
/// stays as it was — it witnesses the script, not videos.
///
/// v0.0.23a: 0.0.22a's avatar fix field-verified clean,
/// but the start-unmuted fix FAILED in the field — John:
/// "its still muted on start". Reading the shipped code
/// found a self-defeating loop, mechanics verified in the
/// code (field-confirmation still John's to give): the
/// `volumechange` listener still LEARNED preferences
/// from external changes after the grace window, saving
/// whatever state the page last produced — and the page
/// forces a muted start. Preview bubbles toggle muted as
/// the page pleases too. Any such external muted state
/// landing outside a grace window poisoned the stored
/// preference to muted=true, and the next video's grace
/// machinery then dutifully enforced that poisoned
/// "saved" state. This run breaks the loop four ways,
/// volume start state only, everything else untouched:
/// (1) the volumechange listener NEVER writes the
/// preference again — it syncs the bar UI and, inside
/// the grace window, still triggers re-assertion; the
/// preference is written only by the bar's own mute /
/// volume handlers and by the migration in (2).
/// (2) Provenance: bar-written preferences carry
/// `byBar: true`. loadVolPref treats a stored value
/// without that stamp as untrusted — returns the default
/// {volume 1, muted false} and overwrites the key with
/// the stamped default, once, so a poisoned 0.0.22a
/// preference self-heals on first run; a deliberate
/// bar-set muted preference is stamped and honoured.
/// (3) During the grace window a MutationObserver also
/// watches the element's `muted` ATTRIBUTE and re-applies
/// the saved state if it appears while the saved state
/// is unmuted; it disconnects when grace ends or the
/// user takes control.
/// (4) Volume management is dialog-only: inline preview
/// bubbles are silent looping thumbnails by design, so
/// apply / grace / persistence skip any video whose
/// wrapper has no [role="dialog"] ancestor; enforcement
/// runs solely for the opened "Media preview" player.
/// The preview keeps its bar, exactly as in 0.0.22a.
///
/// v0.0.24a: 0.0.23a passed John's fresh-video test —
/// "I like it" — and this run is purely his two cosmetic
/// directives of 2026-11-08 20:55: (1) "change the color
/// from gold to.. something that matches the app", and
/// (2) hide or move the "NUB player active" banner —
/// "it is right over the call button". The badge is
/// REMOVED, not moved: its diagnostic job ended when
/// execution was proven across 0.0.21a-0.0.23a, and the
/// window title still names the running build. In its
/// place a one-line cleanup deletes any badge element a
/// previous build left in a live page. The console logs
/// stay (invisible, still useful). Every gold accent in
/// the in-page bar — button and speed-select borders,
/// the bar's top rule, the seek and volume thumbs
/// (accent-color), and the CC-on colour — becomes the
/// app's own dark-mode neutral: off-white #f2f2f2 for
/// fills / thumbs / active states, hairlines in the
/// same off-white at low alpha, muted grey #9a9a9e
/// staying on secondary elements (time readout, CC-off).
/// Nothing else changes: same combined script, wrapper
/// gate, dialog-only volume, un-poisoned preference.
///
/// Placement: where the video's parent is positioned AND is
/// the video's offsetParent (the 0.0.18a safe-anchor test),
/// the bar overlays the video's bottom edge, sized/placed from
/// its offsets and kept in place by a ResizeObserver; where
/// that test fails, the bar is inserted in flow directly after
/// the video, full width. Either way it is Nubisianua's bar,
/// in place, in the chat.
///
/// Same architecture as EXT_LINK_SCRIPT (v0.0.13a): an
/// initialization script in the embedder's privileged context —
/// NO Tauri APIs, no IPC (the zero-IPC invariant holds);
/// everything persisted lives in the page origin's
/// localStorage, inside the tab's per-profile directory.
/// Scope: remote http(s) pages only. The Rust side does not
/// attach this script to the Screening Room tab at all, and
/// the script itself also refuses non-http(s) pages and any
/// player.html, so app pages keep their own native player.
/// Defensive throughout: a global flag makes double-injection
/// a no-op, and every operation is try/catch wrapped.
/// Diagnostics: console lines prefixed [nubisianua-player]
/// for video enhanced / controls attached / captions toggled /
/// fullscreen clicked, quiet otherwise.
const VIDEO_ENHANCER_SCRIPT: &str = r#"
(function () {
  try {
    if (window.__nubisianuaPlayer) return;
    window.__nubisianuaPlayer = true;

    /* ---- scope: remote pages only, never the app's own ---- */
    try {
      var proto = window.location.protocol;
      if (proto !== 'http:' && proto !== 'https:') return;
      if (window.location.pathname &&
          window.location.pathname.indexOf('player.html') !== -1) return;
    } catch (e) { return; }

    var VOL_KEY = 'nubisianua-player-volume';
    var POS_PREFIX = 'nubisianua-player-pos:';
    var CC_KEY = 'nubisianua-player-cc';
    var RATE_KEY = 'nubisianua-player-rate';
    var RATES = [0.5, 0.75, 1, 1.25, 1.5, 2];
    var BAR_CLASS = 'nubisianua-player-bar';

    var registry = [];   /* every enhanced video, for global prefs */

    function log(msg) {
      try { console.info('[nubisianua-player] ' + msg); } catch (e) { }
    }

    /* ---- v0.0.24a: the diagnostic badge is GONE ----
       The v0.0.21a "NUB player active" badge did its job —
       execution is proven — and John directed its removal
       (it sat right over the page's Call button). Nothing
       creates it now; this cleanup only deletes a badge
       element an older build may have left in a live page. */
    function removeBadge() {
      try {
        var stale = document.querySelectorAll(
          '#nubisianua-player-badge, .nubisianua-player-badge');
        for (var i = 0; i < stale.length; i++) {
          try { stale[i].remove(); } catch (e) { }
        }
      } catch (e) { /* never break the page */ }
    }
    function lsGet(k) {
      try { return localStorage.getItem(k); } catch (e) { return null; }
    }
    function lsSet(k, v) {
      try { localStorage.setItem(k, v); } catch (e) { /* quota / private mode */ }
    }
    function lsDel(k) {
      try { localStorage.removeItem(k); } catch (e) { }
    }
    function fmt(t) {
      if (!isFinite(t) || t < 0) t = 0;
      t = Math.floor(t);
      var h = Math.floor(t / 3600), m = Math.floor((t % 3600) / 60), s = t % 60;
      function p(n) { return (n < 10 ? '0' : '') + n; }
      return h > 0 ? h + ':' + p(m) + ':' + p(s) : m + ':' + p(s);
    }

    /* ---- global volume / mute preference (0.0.18a key) ---- */
    /* v0.0.23a provenance: the preference is trusted ONLY
       when it carries the `byBar: true` stamp, which only
       our own bar's controls (commitVolumeControl) and the
       one-time migration below ever write. 0.0.22a's field
       failure was self-poisoning: the volumechange listener
       learned (saved) external states after the grace
       window, and the page's own forced muted-start is an
       external state — so a muted start overwrote the very
       preference the next video was supposed to start
       with, and enforcement then dutifully enforced muted.
       An unstamped stored value is therefore treated as
       untrusted: fall back to the default and overwrite
       the stored key with that default, stamped, exactly
       once — a possibly-poisoned 0.0.22a preference
       self-heals on first run, while a muted preference
       John deliberately sets on the bar later carries the
       stamp and is honoured. */
    function defaultVolPref() {
      return { volume: 1, muted: false, byBar: true };   /* NOT muted by default */
    }
    function loadVolPref() {
      try {
        var raw = lsGet(VOL_KEY);
        if (raw) {
          var o = JSON.parse(raw);
          if (o && o.byBar === true &&
              typeof o.volume === 'number' && isFinite(o.volume)) {
            return {
              volume: Math.min(1, Math.max(0, o.volume)),
              muted: !!o.muted,
              byBar: true
            };
          }
          /* Untrusted stored value (no byBar stamp):
             migrate once to the stamped default. */
          var d = defaultVolPref();
          lsSet(VOL_KEY, JSON.stringify(d));
          log('volume preference untrusted (no byBar stamp); migrated to default');
          return d;
        }
      } catch (e) { /* fall through to the default */ }
      return defaultVolPref();
    }
    function currentVolumeSource(video) {
      try { return video.currentSrc || video.src || ''; } catch (e) { return ''; }
    }

    /* v0.0.23a: the opened player lives in the Radix
       "Media preview" dialog; the inline preview bubble
       (same wrapper, NOT in a dialog) is a silent looping
       thumbnail by design. Volume state — applying it,
       enforcing it, persisting it — is managed ONLY for
       the dialog video. Checked dynamically at call time,
       so an element that moves contexts is judged by
       where it is when the event fires. */
    function isDialogVideo(video) {
      try {
        return !!(video && video.closest && video.closest('[role="dialog"]'));
      } catch (e) { return false; }
    }

    /* The saved state for THIS source, captured before the
       page's startup code gets a chance to overwrite the
       preference with its own forced mute (the 0.0.22a
       volumechange listener saved every change, including
       the page's — the poisoning 0.0.23a removes).
       Re-captured when the source changes; updated
       directly by our own bar's controls only. External
       changes are never learned (see loadVolPref). */
    function savedVolPref(video, st) {
      try {
        var srcId = currentVolumeSource(video);
        if (!st.volTarget || st.volTargetFor !== srcId) {
          if (st.volTarget) {
            /* A new source gets a fresh enforcement run and a
               freshly-read preference. */
            st.volUserControlled = false;
            st.volGraceCompleted = false;
            st.volReassertions = 0;
            st.volHeldChecks = 0;
            st.volApplied = false;
            st.volAppliedFor = null;
          }
          st.volTarget = loadVolPref();
          st.volTargetFor = srcId;
        }
        return st.volTarget;
      } catch (e) {
        return defaultVolPref();
      }
    }

    function volumeStateMatches(video, p) {
      try {
        if (!p) return false;
        if (Math.abs(video.volume - p.volume) > 0.001) return false;
        if (video.muted !== p.muted) return false;
        if (!p.muted) {
          if (video.hasAttribute && video.hasAttribute('muted')) return false;
          if (video.defaultMuted) return false;
        }
        return true;
      } catch (e) { return false; }
    }

    /* Apply the saved state once. Returns true only when the
       live state actually diverged and had to be corrected.
       Our own write is timestamp-stamped so the volumechange
       listener can tell it from an external override. */
    function applySavedVolume(video, st, countAsReassert) {
      try {
        if (!isDialogVideo(video)) return false;   /* previews: hands off volume */
        if (st.volUserControlled) return false;
        var p = savedVolPref(video, st);
        var diverged = !volumeStateMatches(video, p);
        st.ownVolumeGuardUntil = Date.now() + 1500;
        if (!p.muted) {
          try { if (video.removeAttribute) video.removeAttribute('muted'); } catch (e) { }
          try { video.defaultMuted = false; } catch (e) { }
        }
        if (Math.abs(video.volume - p.volume) > 0.001) video.volume = p.volume;
        if (video.muted !== p.muted) video.muted = p.muted;
        st.ownVolumeGuardUntil = Date.now() + 1500;
        if (diverged && countAsReassert) {
          st.volReassertions = (st.volReassertions || 0) + 1;
          log('saved volume state re-asserted (#' + st.volReassertions + ')');
        }
        if (st.syncVol) st.syncVol();
        return diverged;
      } catch (e) { return false; }
    }

    function applyVolPrefOnce(video, st) {
      try {
        if (!isDialogVideo(video)) return;   /* previews keep their silence */
        var srcId = currentVolumeSource(video);
        if (st.volAppliedFor === srcId && st.volApplied) return;
        st.volApplied = true;
        st.volAppliedFor = srcId;
        savedVolPref(video, st);
        applySavedVolume(video, st, false);
      } catch (e) { /* never break the page */ }
    }

    /* v0.0.23a belt and braces: the page can force a mute
       by setting the `muted` content ATTRIBUTE, which may
       not surface through the property checks on the same
       tick. While the grace window runs, watch the
       attribute; if it appears while the saved state is
       unmuted, re-apply (applying strips the attribute).
       Disconnected when grace ends or the user takes
       control — both go through stopVolumeGrace. */
    function startMutedAttrWatch(video, st) {
      try {
        if (st.volAttrObserver) return;
        if (typeof MutationObserver === 'undefined') return;
        st.volAttrObserver = new MutationObserver(function () {
          try {
            if (!st.volGraceActive || st.volUserControlled) return;
            if (!video.hasAttribute || !video.hasAttribute('muted')) return;
            var p = savedVolPref(video, st);
            if (!p.muted) applySavedVolume(video, st, true);
          } catch (e) { }
        });
        st.volAttrObserver.observe(video, {
          attributes: true, attributeFilter: ['muted']
        });
      } catch (e) { /* never break the page */ }
    }
    function stopMutedAttrWatch(st) {
      try {
        if (st.volAttrObserver) {
          try { st.volAttrObserver.disconnect(); } catch (e) { }
          st.volAttrObserver = null;
        }
      } catch (e) { /* never break the page */ }
    }

    function stopVolumeGrace(video, st, reason, silent) {
      try {
        var wasActive = !!st.volGraceActive;
        stopMutedAttrWatch(st);
        if (st.volGraceTimer) {
          try { clearInterval(st.volGraceTimer); } catch (e) { }
          st.volGraceTimer = null;
        }
        st.volGraceActive = false;
        st.volGraceCompleted = true;
        if (wasActive && !silent && st.volReassertions) {
          log('volume grace closed (' + reason + '): saved state re-asserted ' +
            st.volReassertions + ' time' + (st.volReassertions === 1 ? '' : 's'));
        }
        if (wasActive) st.volReassertions = 0;
      } catch (e) { /* never break the page */ }
    }

    function checkVolumeGrace(video, st) {
      try {
        if (!st.volGraceActive) return;
        if (st.volUserControlled) {
          stopVolumeGrace(video, st, 'user volume control');
          return;
        }
        if (Date.now() >= st.volGraceUntil) {
          stopVolumeGrace(video, st, 'grace window elapsed');
          return;
        }
        var p = savedVolPref(video, st);
        if (volumeStateMatches(video, p)) {
          st.volHeldChecks = (st.volHeldChecks || 0) + 1;
          /* Several consecutive held checks (~1 s of quiet)
             mean the startup fight is over; stop early. */
          if (st.volHeldChecks >= 4) {
            stopVolumeGrace(video, st, 'saved state held');
          }
        } else {
          st.volHeldChecks = 0;
          applySavedVolume(video, st, true);
        }
      } catch (e) { /* never break the page */ }
    }

    function startVolumeGrace(video, st) {
      try {
        if (!isDialogVideo(video)) return;   /* previews: no enforcement */
        if (st.volUserControlled) return;
        if (st.volGraceTimer) {
          try { clearInterval(st.volGraceTimer); } catch (e) { }
          st.volGraceTimer = null;
        }
        startMutedAttrWatch(video, st);
        st.volGraceActive = true;
        st.volGraceCompleted = false;
        st.volGraceUntil = Date.now() + 5000;
        st.volHeldChecks = 0;
        st.volGraceTimer = setInterval(function () {
          checkVolumeGrace(video, st);
        }, 250);
      } catch (e) { /* never break the page */ }
    }

    /* Our bar's volume controls: stamp the change as the
       user's own, end enforcement for this source, and make
       the resulting state the saved preference. */
    function claimVolumeControl(video, st) {
      try {
        st.userVolumeGuardUntil = Date.now() + 1500;
        if (!st.volUserControlled) {
          st.volUserControlled = true;
          stopVolumeGrace(video, st, 'user volume control');
        }
      } catch (e) { /* never break the page */ }
    }
    function commitVolumeControl(video, st) {
      try {
        /* v0.0.23a: the bar's handlers are one of only two
           writers of the preference (the other is the
           load-time migration), and everything they write
           carries the byBar provenance stamp. Persistence
           is managed for the dialog video only — twiddling
           a silent preview's slider writes nothing. */
        if (isDialogVideo(video)) {
          var nowState = {
            volume: Math.min(1, Math.max(0, video.volume)),
            muted: !!video.muted,
            byBar: true
          };
          st.volTarget = nowState;
          st.volTargetFor = currentVolumeSource(video);
          lsSet(VOL_KEY, JSON.stringify(nowState));
        }
        st.userVolumeGuardUntil = Date.now() + 1500;
        if (st.syncVol) st.syncVol();
      } catch (e) { /* never break the page */ }
    }

    /* ---- closed captions preference: default OFF ---- */
    function ccOn() { return lsGet(CC_KEY) === '1'; }
    function applyCaptions(video, st) {
      try {
        var tracks = video.textTracks;
        if (!tracks) return;
        var on = ccOn();
        var shown = false;
        var any = false;
        for (var i = 0; i < tracks.length; i++) {
          var tr = tracks[i];
          if (!tr) continue;
          if (tr.kind !== 'captions' && tr.kind !== 'subtitles') continue;
          any = true;
          var want = (on && !shown) ? 'showing' : 'disabled';
          if (on && !shown) shown = true;
          if (tr.mode !== want) tr.mode = want;   /* write only on change */
        }
        st.hasCC = any;
        if (st.syncCC) st.syncCC();
      } catch (e) { /* never break the page */ }
    }

    /* ---- playback speed preference ---- */
    function loadRate() {
      var r = parseFloat(lsGet(RATE_KEY));
      if (isFinite(r) && RATES.indexOf(r) !== -1) return r;
      return 1;
    }
    function applyRate(video, st) {
      try {
        var r = loadRate();
        if (Math.abs((video.playbackRate || 1) - r) > 0.01) video.playbackRate = r;
        if (st.rateSel && st.rateSel.value !== String(r)) st.rateSel.value = String(r);
      } catch (e) { /* never break the page */ }
    }

    /* ---- per-video position (kept from 0.0.18a) ---- */
    function fnv1a(s) {
      var h = 0x811c9dc5;
      for (var i = 0; i < s.length; i++) {
        h ^= s.charCodeAt(i);
        h = (h * 0x01000193) >>> 0;
      }
      return ('0000000' + h.toString(16)).slice(-8);
    }
    function posKeyFor(video) {
      try {
        var src = video.currentSrc || video.src || '';
        if (!src) return null;
        if (src.indexOf('blob:') === 0 || src.indexOf('data:') === 0) return null;
        var clean = src;
        try {
          var u = new URL(src, location.href);
          clean = u.origin + u.pathname;
        } catch (e) {
          clean = src.split(/[?#]/)[0];
        }
        if (!clean) return null;
        return POS_PREFIX + fnv1a(clean) + '-' + clean.length;
      } catch (e) {
        return null;
      }
    }
    function savePos(video) {
      try {
        var k = posKeyFor(video);
        if (!k) return;
        var d = video.duration, t = video.currentTime;
        if (!isFinite(d) || d <= 0 || !isFinite(t)) return;
        if (video.ended || d - t <= 5) { lsDel(k); return; }
        if (t > 0) lsSet(k, String(t));
      } catch (e) { /* never break the page */ }
    }
    /* Restore AT MOST ONCE per video source. The key is marked
       restored BEFORE any seek is attempted, and no other event
       (durationchange included) ever calls this — see the freeze
       diagnosis in the Rust comment above this script. */
    function restorePosOnce(video, st) {
      try {
        var k = posKeyFor(video);
        if (!k) return;
        if (st.restoredKey === k) return;
        st.restoredKey = k;
        var t = parseFloat(lsGet(k));
        if (!isFinite(t)) return;
        var d = video.duration;
        if (!isFinite(d) || d <= 0) return;
        if (t > 5 && t < d - 5) {
          try { video.currentTime = t; } catch (e) { }
        }
      } catch (e) { /* never break the page */ }
    }

    /* ---- fullscreen: container first, video as fallback ---- */
    function toggleFullscreen(video, st) {
      log('fullscreen clicked');
      try {
        if (document.fullscreenElement) { document.exitFullscreen(); return; }
        var target = null;
        if (st && st.wrapper) target = st.wrapper;
        else if (st && st.overlay && video.parentElement) target = video.parentElement;
        var p = null;
        try { if (target && target.requestFullscreen) p = target.requestFullscreen(); } catch (e) { p = null; }
        if (p && p.catch) {
          p.catch(function () {
            try { if (video.requestFullscreen) video.requestFullscreen(); } catch (e) { }
          });
          return;
        }
        if (!p) {
          if (video.requestFullscreen) video.requestFullscreen();
          else if (video.webkitEnterFullscreen) video.webkitEnterFullscreen();
        }
      } catch (e) { /* never break the page */ }
    }

    /* ---- the control bar ---- */
    var BTN_CSS =
      'flex:none;height:26px;min-width:30px;padding:0 7px;border-radius:6px;' +
      'background:rgba(42,42,43,.9);color:#e6e6e6;cursor:pointer;' +
      'border:1px solid rgba(242,242,242,.28);font-size:12px;line-height:1;';

    function makeBtn(text, title) {
      var b = document.createElement('button');
      b.type = 'button';
      b.textContent = text;
      b.title = title;
      b.setAttribute('aria-label', title);
      b.style.cssText = BTN_CSS;
      return b;
    }

    function buildBar(video, st) {
      var bar = document.createElement('div');
      bar.className = BAR_CLASS;
      bar.style.cssText =
        'display:flex;align-items:center;gap:6px;height:38px;' +
        'box-sizing:border-box;padding:5px 8px;' +
        'background:rgba(24,24,25,.92);color:#e6e6e6;' +
        'font:12px/1.4 system-ui,"Segoe UI",sans-serif;' +
        'border-top:1px solid rgba(242,242,242,.18);user-select:none;';

      var playBtn = makeBtn('▶', 'Play / pause');
      var seek = document.createElement('input');
      seek.type = 'range'; seek.min = '0'; seek.max = '1000'; seek.value = '0';
      seek.title = 'Seek';
      seek.setAttribute('aria-label', 'Seek');
      seek.style.cssText = 'flex:1;min-width:40px;accent-color:#f2f2f2;cursor:pointer;';
      var time = document.createElement('span');
      time.textContent = '0:00 / 0:00';
      time.style.cssText = 'flex:none;color:#9a9a9e;white-space:nowrap;font-variant-numeric:tabular-nums;';
      var ccBtn = makeBtn('CC', 'Closed captions on / off');
      ccBtn.style.fontWeight = '700';
      var rateSel = document.createElement('select');
      rateSel.title = 'Playback speed';
      rateSel.setAttribute('aria-label', 'Playback speed');
      rateSel.style.cssText =
        'flex:none;height:26px;background:rgba(42,42,43,.9);color:#e6e6e6;' +
        'border:1px solid rgba(242,242,242,.28);border-radius:6px;font-size:12px;cursor:pointer;';
      for (var i = 0; i < RATES.length; i++) {
        var opt = document.createElement('option');
        opt.value = String(RATES[i]);
        opt.textContent = RATES[i] + '×';
        rateSel.appendChild(opt);
      }
      rateSel.value = String(loadRate());
      var muteBtn = makeBtn('🔊', 'Mute / unmute');
      var vol = document.createElement('input');
      vol.type = 'range'; vol.min = '0'; vol.max = '100'; vol.value = '100';
      vol.title = 'Volume';
      vol.setAttribute('aria-label', 'Volume');
      vol.style.cssText = 'flex:none;width:52px;accent-color:#f2f2f2;cursor:pointer;';
      var fsBtn = makeBtn('⛶', 'Full screen');

      bar.appendChild(playBtn);
      bar.appendChild(seek);
      bar.appendChild(time);
      bar.appendChild(ccBtn);
      bar.appendChild(rateSel);
      bar.appendChild(muteBtn);
      bar.appendChild(vol);
      bar.appendChild(fsBtn);

      st.bar = bar; st.playBtn = playBtn; st.seek = seek; st.timeEl = time;
      st.ccBtn = ccBtn; st.rateSel = rateSel; st.muteBtn = muteBtn; st.volEl = vol;

      /* sync helpers */
      st.syncPlay = function () {
        try { playBtn.textContent = video.paused || video.ended ? '▶' : '⏸'; } catch (e) { }
      };
      st.syncTime = function () {
        try {
          time.textContent = fmt(video.currentTime) + ' / ' + fmt(video.duration);
          if (!st.scrubbing && isFinite(video.duration) && video.duration > 0) {
            var v = Math.round((video.currentTime / video.duration) * 1000);
            if (String(v) !== seek.value) seek.value = String(v);
          }
        } catch (e) { }
      };
      st.syncCC = function () {
        try {
          var on = ccOn();
          ccBtn.setAttribute('aria-pressed', on ? 'true' : 'false');
          ccBtn.style.color = on ? '#f2f2f2' : '#9a9a9e';
          ccBtn.style.opacity = st.hasCC ? '1' : '.55';
          ccBtn.title = st.hasCC
            ? (on ? 'Closed captions are ON — click to turn off'
                  : 'Closed captions are OFF — click to turn on')
            : 'No captions on this video yet';
        } catch (e) { }
      };
      st.syncVol = function () {
        try {
          muteBtn.textContent = video.muted || video.volume === 0 ? '🔇' : '🔊';
          var v = Math.round((video.muted ? 0 : video.volume) * 100);
          if (String(v) !== vol.value && document.activeElement !== vol) vol.value = String(v);
        } catch (e) { }
      };

      function stop(e) { try { e.preventDefault(); e.stopPropagation(); } catch (err) { } }
      playBtn.addEventListener('click', function (e) {
        stop(e);
        try {
          if (video.paused || video.ended) { var p = video.play(); if (p && p.catch) p.catch(function () { }); }
          else video.pause();
        } catch (err) { }
      });
      seek.addEventListener('pointerdown', function () { st.scrubbing = true; });
      seek.addEventListener('input', function () {
        st.scrubbing = true;
        try {
          if (isFinite(video.duration) && video.duration > 0) {
            time.textContent = fmt((parseFloat(seek.value) / 1000) * video.duration) +
              ' / ' + fmt(video.duration);
          }
        } catch (e) { }
      });
      seek.addEventListener('change', function () {
        /* one deliberate seek per user scrub — never a loop */
        try {
          if (isFinite(video.duration) && video.duration > 0) {
            video.currentTime = (parseFloat(seek.value) / 1000) * video.duration;
          }
        } catch (e) { }
        st.scrubbing = false;
      });
      ccBtn.addEventListener('click', function (e) {
        stop(e);
        var on = !ccOn();
        lsSet(CC_KEY, on ? '1' : '0');
        log('captions toggled ' + (on ? 'on' : 'off'));
        for (var i = 0; i < registry.length; i++) {
          var v = registry[i];
          if (v && v.__nubSt) applyCaptions(v, v.__nubSt);
        }
      });
      rateSel.addEventListener('change', function () {
        var r = parseFloat(rateSel.value) || 1;
        lsSet(RATE_KEY, String(r));
        for (var i = 0; i < registry.length; i++) {
          var v = registry[i];
          if (v && v.__nubSt) applyRate(v, v.__nubSt);
        }
      });
      muteBtn.addEventListener('click', function (e) {
        stop(e);
        claimVolumeControl(video, st);
        try { video.muted = !video.muted; } catch (err) { }
        commitVolumeControl(video, st);
      });
      vol.addEventListener('input', function () {
        claimVolumeControl(video, st);
        try {
          video.volume = parseFloat(vol.value) / 100;
          if (video.volume > 0 && video.muted) video.muted = false;
        } catch (e) { }
        commitVolumeControl(video, st);
      });
      fsBtn.addEventListener('click', function (e) {
        stop(e);
        toggleFullscreen(video, st);
      });
      return bar;
    }

    function positionBar(video, st) {
      try {
        if (st.wrapperBar) return;   /* CSS anchors it to the wrapper */
        if (!st.bar || !st.overlay) return;
        st.bar.style.left = video.offsetLeft + 'px';
        st.bar.style.width = video.offsetWidth + 'px';
        st.bar.style.top = (video.offsetTop + video.offsetHeight - 38) + 'px';
      } catch (e) { /* never break the page */ }
    }

    function attachBar(video, st) {
      try {
        if (st.bar) { positionBar(video, st); return; }
        var parent = video.parentElement;
        if (!parent) return;
        var bar = buildBar(video, st);
        /* v0.0.21a first-class target: a video inside a
           [data-hatch-video-wrapper] container (the proven
           Muse chat structure). That wrapper is relative +
           overflow-hidden, so an in-flow bar would push the
           layout and an offset overlay can be clipped; the
           bar instead goes ON the wrapper itself, absolute
           at its bottom edge, inside the clip, on top. */
        var wrapper = null;
        try {
          wrapper = (video.closest)
            ? video.closest('[data-hatch-video-wrapper]') : null;
        } catch (e) { wrapper = null; }
        if (wrapper) {
          try {
            var wcs = window.getComputedStyle(wrapper);
            if (!wcs || wcs.position === 'static') wrapper.style.position = 'relative';
          } catch (e) { }
          st.wrapper = wrapper;
          st.wrapperBar = true;
          st.overlay = true;
          bar.style.position = 'absolute';
          bar.style.left = '0';
          bar.style.right = '0';
          bar.style.bottom = '0';
          bar.style.width = '100%';
          bar.style.zIndex = '2147483000';
          wrapper.appendChild(bar);
          st.syncPlay(); st.syncTime(); st.syncCC(); st.syncVol();
          log('controls attached (wrapper)');
          return;
        }
        var overlay = false;
        try {
          var cs = window.getComputedStyle(parent);
          if (cs && cs.position !== 'static' && video.offsetParent === parent && video.offsetWidth) {
            overlay = true;
          }
        } catch (e) { overlay = false; }
        st.overlay = overlay;
        if (overlay) {
          bar.style.position = 'absolute';
          bar.style.zIndex = '2147483000';
          parent.appendChild(bar);
          positionBar(video, st);
        } else {
          bar.style.position = 'relative';
          bar.style.width = '100%';
          if (video.nextSibling) parent.insertBefore(bar, video.nextSibling);
          else parent.appendChild(bar);
        }
        st.syncPlay(); st.syncTime(); st.syncCC(); st.syncVol();
        log('controls attached (' + (overlay ? 'overlay' : 'in-flow') + ')');
      } catch (e) { /* never break the page */ }
    }

    /* ---- enhancement ---- */
    function enhance(video, where) {
      try {
        if (!video || (video.dataset && video.dataset.nubisianuaPlayer)) return;
        /* v0.0.22a scoping (the second fix of this run):
           chat attachments ONLY. 0.0.21a enhanced every
           <video> the walk found, and John's field evidence
           showed the cost: the animated avatar on muse.ai's
           presence card is itself a looping <video>, and in
           Nubisianua it grew a cropped strip of our bar —
           play button, gold seek thumb, a "0" time fragment —
           across his picture, while the same avatar in the
           browser (no enhancer) is clean. John's DOM
           inspection proved both real chat videos — the
           preview bubble and the opened "Media preview"
           player — live inside a [data-hatch-video-wrapper]
           ancestor, so that ancestor is now the gate: no
           wrapper, no enhancement at all — no bar, no volume
           or resume meddling, and not even the dataset mark,
           so a video that gains a wrapper ancestor later
           (moved in the DOM) is still picked up by the
           periodic re-walk. A video that loses its wrapper
           after enhancement is left as it is. The check is
           the same wherever the video is found — main
           document, shadow root, or iframe. The diagnostic
           badge is unaffected: it witnesses the script's
           execution, not individual videos. */
        var gateWrapper = null;
        try {
          gateWrapper = (video.closest)
            ? video.closest('[data-hatch-video-wrapper]') : null;
        } catch (e) { gateWrapper = null; }
        if (!gateWrapper) return;
        try { video.dataset.nubisianuaPlayer = '1'; } catch (e) { }
        var st = {
          bar: null, overlay: false, hasCC: false, scrubbing: false,
          restoredKey: null, volApplied: false, volAppliedFor: null,
          volTarget: null, volTargetFor: null,
          volGraceTimer: null, volGraceActive: false, volGraceUntil: 0,
          volGraceCompleted: false, volHeldChecks: 0, volReassertions: 0,
          volAttrObserver: null,
          volUserControlled: false, ownVolumeGuardUntil: 0,
          userVolumeGuardUntil: 0,
          clickTimer: null, wrapper: null, wrapperBar: false
        };
        video.__nubSt = st;
        registry.push(video);
        if (where === 'shadow-root') log('video found in shadow root');
        else if (where === 'iframe') log('video found in iframe');
        log('video found in hatch wrapper');
        log('video enhanced');

        function onMeta() {
          try {
            var srcId = currentVolumeSource(video);
            if (st.volTargetFor && st.volTargetFor !== srcId) {
              stopVolumeGrace(video, st, 'source changed', true);
            }
          } catch (e) { }
          applyVolPrefOnce(video, st);
          applyRate(video, st);
          applyCaptions(video, st);
          restorePosOnce(video, st);
          attachBar(video, st);
          if (st.syncVol) st.syncVol();
          if (st.syncTime) st.syncTime();
        }
        video.addEventListener('loadedmetadata', onMeta);
        video.addEventListener('canplay', function () {
          applySavedVolume(video, st, true);
        });
        video.addEventListener('playing', function () {
          applySavedVolume(video, st, true);
          if (st.syncPlay) st.syncPlay();
        });
        /* durationchange deliberately does NOT restore position
           (freeze safeguard, see header); the bounded volume
           grace above is the 0.0.22a exception, not this event. */
        video.addEventListener('durationchange', function () { if (st.syncTime) st.syncTime(); });
        video.addEventListener('volumechange', function () {
          try {
            var nowMs = Date.now();
            var actual = {
              volume: Math.min(1, Math.max(0, video.volume)),
              muted: !!video.muted
            };
            if (nowMs <= (st.userVolumeGuardUntil || 0)) {
              /* From our bar: the handler already committed it
                 as the saved state and ended enforcement. */
              actual.byBar = true;
              st.volTarget = actual;
              st.volTargetFor = currentVolumeSource(video);
            } else if (nowMs <= (st.ownVolumeGuardUntil || 0)) {
              /* From our own saved-state application: nothing
                 to save and nothing to fight. */
            } else if (st.volGraceActive) {
              /* External override inside the grace window:
                 do NOT let it overwrite the saved preference;
                 send the live state back to the saved state. */
              var target = savedVolPref(video, st);
              if (!volumeStateMatches(video, target)) {
                applySavedVolume(video, st, true);
              }
            } else if (!st.volGraceCompleted) {
              /* Startup has begun but the play-triggered grace
                 window has not closed yet (the page's forced
                 mute can land here, between metadata and
                 play). Keep the captured saved state intact;
                 the play handler / grace window will apply
                 it. Do not persist the override. */
            } else {
              /* After the grace window, external changes are
                 the page's business, but they are NEVER
                 learned: v0.0.23a removes the 0.0.22a branch
                 that saved them into the preference. That
                 learning was the poison — the page's forced
                 muted start landed here and overwrote the
                 saved state, so every next video started
                 muted by our own enforcement. This listener
                 now only syncs the bar UI; VOL_KEY is
                 written solely by the bar's handlers (with
                 the byBar stamp) and by the one-time
                 migration in loadVolPref. */
            }
          } catch (e) { }
          if (st.syncVol) st.syncVol();
        });
        video.addEventListener('ratechange', function () {
          if (st.rateSel && st.rateSel.value !== String(video.playbackRate)) {
            var r = video.playbackRate;
            if (RATES.indexOf(r) !== -1) st.rateSel.value = String(r);
          }
        });
        var lastSave = 0;
        video.addEventListener('timeupdate', function () {
          if (st.syncTime) st.syncTime();
          var now = Date.now();
          if (now - lastSave >= 5000) { lastSave = now; savePos(video); }
        });
        video.addEventListener('pause', function () { savePos(video); if (st.syncPlay) st.syncPlay(); });
        video.addEventListener('play', function () {
          applySavedVolume(video, st, true);
          startVolumeGrace(video, st);
          if (st.syncPlay) st.syncPlay();
        });
        video.addEventListener('ended', function () {
          var k = posKeyFor(video);
          if (k) lsDel(k);
          if (st.syncPlay) st.syncPlay();
        });
        /* NOTE: no 'seeked' listener (0.0.18a freeze safeguard). */

        /* Captions arriving after metadata (the common case) */
        try {
          if (video.textTracks && video.textTracks.addEventListener) {
            video.textTracks.addEventListener('addtrack', function () {
              applyCaptions(video, st);
            });
            /* Re-assert our preference if the page flips a
               track itself. Terminates: applyCaptions only
               writes a mode that actually differs. */
            video.textTracks.addEventListener('change', function () {
              applyCaptions(video, st);
            });
          }
        } catch (e) { }

        /* Click toggles play/pause, debounce-guarded: a second
           click inside the window cancels the pending toggle
           (it was a double-click, handled below), and when the
           timer fires we only toggle if nothing else already
           changed the play state since the click. */
        video.addEventListener('click', function () {
          try {
            if (st.clickTimer) { clearTimeout(st.clickTimer); st.clickTimer = null; return; }
            var wasPaused = video.paused;
            st.clickTimer = setTimeout(function () {
              st.clickTimer = null;
              try {
                if (video.paused !== wasPaused) return;   /* page already toggled */
                if (video.paused) { var p = video.play(); if (p && p.catch) p.catch(function () { }); }
                else video.pause();
              } catch (e) { }
            }, 260);
          } catch (e) { }
        });
        video.addEventListener('dblclick', function (e) {
          try {
            if (st.clickTimer) { clearTimeout(st.clickTimer); st.clickTimer = null; }
            e.preventDefault();
          } catch (err) { }
          toggleFullscreen(video, st);
        });

        try {
          window.addEventListener('resize', function () { positionBar(video, st); });
          if (typeof ResizeObserver !== 'undefined') {
            var ro = new ResizeObserver(function () { positionBar(video, st); });
            ro.observe(video);
          }
        } catch (e) { }
        video.addEventListener('resize', function () { positionBar(video, st); });

        /* A video already far enough along when we met it. */
        if (video.readyState >= 1) onMeta();
        try {
          if (!video.paused && !video.ended && video.readyState >= 2) {
            applySavedVolume(video, st, true);
            startVolumeGrace(video, st);
          }
        } catch (e) { }
      } catch (e) { /* never break the page */ }
    }

    function isOurNode(node) {
      try {
        return !!(node && node.classList && node.classList.contains(BAR_CLASS));
      } catch (e) { return false; }
    }

    /* ---- v0.0.20a: piercing scan ----
       0.0.19a's scan() searched only the main document, so a
       <video> inside a shadow root or a same-origin iframe
       was never seen and the page kept its original player.
       walk() below is the recursive replacement: videos in
       the root it is handed, iframes (same-origin only),
       and every shadow root reachable from there. Depth is
       capped so a self-embedding page cannot recurse forever,
       and enhance() is dataset-guarded, so re-walking the
       same tree is cheap and safe to do on a timer. */
    var observedRoots = null;
    try { observedRoots = new WeakSet(); } catch (e) { observedRoots = null; }

    function whereFor(node) {
      try {
        var r = node.getRootNode ? node.getRootNode() : null;
        if (r && r !== document && r.host) return 'shadow-root';
        if (node.ownerDocument && node.ownerDocument !== document) return 'iframe';
      } catch (e) { }
      return 'document';
    }

    function observeRoot(root) {
      try {
        if (!root) return;
        if (observedRoots) {
          if (observedRoots.has(root)) return;
          observedRoots.add(root);
        }
        var target = root.documentElement || root;
        if (!target) return;
        var obs = new MutationObserver(onMutations);
        obs.observe(target, { childList: true, subtree: true });
      } catch (e) { /* never break the page */ }
    }

    function scanIframe(frame, depth) {
      try {
        var doc = null;
        try { doc = frame.contentDocument; } catch (e) { return; }
        if (!doc) return;   /* cross-origin: skip silently */
        try {
          if (!frame.__nubLoadHooked) {
            frame.__nubLoadHooked = true;
            frame.addEventListener('load', function () {
              try { scanIframe(frame, 0); } catch (e) { }
            });
          }
        } catch (e) { }
        walk(doc, 'iframe', (typeof depth === 'number' ? depth : 0) + 1);
      } catch (e) { /* cross-origin / dead frame: skip silently */ }
    }

    function walk(root, where, depth) {
      try {
        if (!root || isOurNode(root)) return;
        if (typeof depth !== 'number') depth = 0;
        if (depth > 16) return;
        if (root.nodeName === 'VIDEO') { enhance(root, where); return; }
        if (root.nodeName === 'IFRAME') { scanIframe(root, depth); return; }
        /* Watch documents and shadow roots for later changes
           (mutations inside a shadow tree are NOT visible to
           an observer on the host document). */
        try {
          if (root.nodeType === 9 ||
              (typeof ShadowRoot !== 'undefined' && root instanceof ShadowRoot)) {
            observeRoot(root);
          }
        } catch (e) { }
        try { if (root.shadowRoot) walk(root.shadowRoot, 'shadow-root', depth + 1); } catch (e) { }
        if (root.querySelectorAll) {
          var vids = root.querySelectorAll('video');
          for (var i = 0; i < vids.length; i++) enhance(vids[i], where);
          var frames = root.querySelectorAll('iframe');
          for (var f = 0; f < frames.length; f++) scanIframe(frames[f], depth);
          var els = root.querySelectorAll('*');
          for (var j = 0; j < els.length; j++) {
            try {
              if (els[j].shadowRoot) walk(els[j].shadowRoot, 'shadow-root', depth + 1);
            } catch (e) { }
          }
        }
      } catch (e) { /* never break the page */ }
    }

    function scan(node) {
      try { walk(node, whereFor(node), 0); } catch (e) { /* never break the page */ }
    }

    function onMutations(mutations) {
      for (var i = 0; i < mutations.length; i++) {
        var added = mutations[i].addedNodes;
        for (var j = 0; j < added.length; j++) scan(added[j]);
        var removed = mutations[i].removedNodes;
        for (var r = 0; r < removed.length; r++) {
          var rn = removed[r];
          try {
            if (rn && rn.nodeName === 'VIDEO' && rn.__nubSt && rn.__nubSt.bar) {
              rn.__nubSt.bar.remove();
            }
          } catch (e) { }
        }
      }
    }

    /* Hook attachShadow before the page's own scripts run
       (initialization scripts go first), so shadow roots are
       observed from birth — including closed roots, which
       el.shadowRoot never reveals but this return value does. */
    try {
      var origAttachShadow = Element.prototype.attachShadow;
      if (origAttachShadow && !Element.prototype.__nubPlayerPatched) {
        Element.prototype.attachShadow = function () {
          var sr = origAttachShadow.apply(this, arguments);
          try {
            if (sr) {
              observeRoot(sr);
              var captured = sr;
              setTimeout(function () {
                try { walk(captured, 'shadow-root', 0); } catch (e) { }
              }, 0);
            }
          } catch (e) { }
          return sr;
        };
        try { Element.prototype.__nubPlayerPatched = true; } catch (e) { }
      }
    } catch (e) { /* a page that seals its prototype keeps its player */ }

    try {
      observeRoot(document);
    } catch (e) { /* scan-only fallback below still runs */ }

    try {
      if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', function () {
          removeBadge(); scan(document);
        });
      } else {
        removeBadge();
        scan(document);
      }
    } catch (e) { /* never break the page */ }

    /* Shadow content attached long after the host's DOM
       mutation may produce no signal our observers can see
       (e.g. a root created before this script ran); a slow
       periodic re-walk covers the leftovers. */
    try {
      setInterval(function () {
        try { removeBadge(); walk(document, 'document', 0); } catch (e) { }
      }, 3000);
    } catch (e) { /* never break the page */ }

    try { removeBadge(); } catch (e) { }
    log('in-chat player active (v0.0.25a)');
  } catch (e) { /* never break the page */ }
})();
"#;


/// The one and only child-webview creation path. Callers must be async
/// (see the module docs — Windows deadlock rule).
fn create_webview(
    app: &AppHandle,
    entry: &TabEntry,
    active: bool,
    side: DockSide,
    shelf_width: f64,
) -> Result<Webview, String> {
    let window = host_window(app)?;
    // v0.0.18a: the Screening Room sentinel parses fine as a URL
    // (scheme `app`, host `player`) and serves as the tracker's
    // starting value, but the webview itself must NOT be built
    // External from it — it loads the bundled local page instead
    // (see the builder below).
    let is_player = entry.url == PLAYER_URL;
    let url: tauri::Url = entry
        .url
        .parse()
        .map_err(|e| format!("bad tab URL {:?}: {e}", entry.url))?;
    let profile = config::profile_dir(app, &entry.id).map_err(|e| e.to_string())?;
    let (pos, size) = content_bounds(&window, side, shelf_width);
    // v0.0.11a — links. Until this run a `target=_blank` / `window.open`
    // link inside a tab vanished: no handler answered the new-window
    // request, so the platform dropped it. Routing, decided here on the
    // Rust side (tab webviews still have zero IPC — these are builder
    // hooks, not capabilities):
    // - ordinary same-window navigation is never intercepted; it just
    //   flows, and `on_navigation` only *notices* it to keep
    //   `current_url` truthful for the origin test below;
    // - a new-window request whose URL is on the SAME origin as the
    //   page the tab is currently showing is allowed to open as a real
    //   popup — that is where providers keep their own flows (sign-in
    //   and OAuth popups live on the provider's origin);
    // - a host-less request (about:blank and friends) is also allowed:
    //   popup flows commonly start blank and navigate after;
    // - anything else — the link-in-a-chat case — is handed to the OS
    //   default browser via the opener plugin's Rust API and the
    //   in-app window is denied.
    // The handler creates no webview, so the Windows deadlock rule
    // (async creation paths only) is untouched.
    //
    // v0.0.12a — a diagnostic build. The 0.0.11a routing above FAILED
    // the field test on Linux/WebKitGTK: clicking a link in the Muse
    // tab did nothing at all, and the app printed nothing when run
    // from a terminal — while the OS side was proven innocent (the
    // same URL opens via xdg-open; default browser and https scheme
    // handler both point at Firefox). The pinned sources say the
    // wiring is complete end to end (tauri-runtime-wry's
    // create_webview installs the handler for child webviews too, and
    // wry's GTK backend connects WebKit's "create" signal for every
    // webview that carries one), so this round changes NO routing —
    // it makes the path testify instead:
    // - every new-window request logs its target, the tab's tracked
    //   current URL, and the routing decision ([nubisianua-link]);
    // - the opener call's Result is no longer discarded with `let _`
    //   (the one place a silent failure could hide);
    // - on_navigation additionally logs origin CHANGES, so same-tab
    //   cross-origin travel is visible too (still never intercepted).
    // Run from a terminal and click a link: ENTRY lines with no
    // opener success mean the handler fires but cannot hand off;
    // silence means WebKitGTK never raised a new-window request for
    // that click at all.
    //
    // v0.0.12a also carries drag-drop attempt #2. 0.0.11a's fix set
    // `dragDropEnabled: false` in tauri.conf.json and the field test
    // still swallowed drops: that window-config flag only reaches the
    // host webview the config itself creates. Child webviews take
    // their attributes from the builder below, whose default is
    // handler-ENABLED — and the handler is exactly what eats drops on
    // GTK: tauri-runtime-wry attaches wry's drag-drop handler per
    // webview, and wry's GTK implementation connects widget-level
    // drag signals (drag-data-received / drag-drop, finishing the
    // drop itself) that pre-empt WebKit's own page drop handling.
    // `.disable_drag_drop_handler()` clears that same per-webview
    // flag (supported on every platform; only the doc's emphasis is
    // Windows). If drops still die after this, the remaining suspect
    // is WebKitGTK itself, not Tauri interception.
    //
    // v0.0.13a — the 0.0.12a instrumented run returned its verdict:
    // startup logs proved the hooks install and on_navigation
    // logging works, but clicking a cross-origin link in the Muse
    // tab produced TOTAL silence — on_new_window never fired and
    // the click generated no navigation event either. The request
    // dies inside WebKitGTK before any native hook can see it, so
    // this round stops waiting for the native new-window path on
    // Linux and intercepts inside the page instead:
    // - `EXT_LINK_SCRIPT` (above) rides every tab webview as an
    //   initialization script and re-aims external / new-window
    //   clicks and cross-origin window.open calls at the SAME frame,
    //   with a `#nubisianua-ext` fragment marker appended — an
    //   ordinary navigation, which on_navigation DOES see;
    // - `on_navigation` is upgraded from observe-only to a gate
    //   whose ONLY blocking criterion is that marker: it strips the
    //   marker, hands the clean URL to the opener (same Rust-side
    //   call as the new-window path), and returns false, so the tab
    //   stays on its own page. Every unmarked navigation — including
    //   legitimate cross-origin sign-in redirect chains — passes
    //   exactly as before (origin-change logging + tracker update;
    //   a blocked navigation never touches the tracker, since the
    //   tab never actually went there).
    // on_new_window stays exactly as 0.0.12a left it: unproven on
    // Linux, but it is the native path and may serve on Windows.
    //
    // v0.0.14a — the drop-to-clipboard bridge, a reversible trial
    // John blessed by hand. His control test settled the drag
    // question the instrumentation couldn't: a drop into the
    // ChatGPT tab DID reach the page, but WebKitGTK delivered it
    // as text/uri-list only — no File objects — so ChatGPT's
    // composer pasted the path in as text, while Muse's (which
    // reads Files) silently did nothing. Tauri's interceptor
    // stands exonerated; the gap is the delivery FORM. The bridge
    // reuses this module's link architecture end for end: the
    // initialization script gains a drop arm (see EXT_LINK_SCRIPT)
    // that — only for uri-list drops carrying no Files — sends
    // the file:// entries up through a navigation to the reserved
    // sentinel host, and the gate below answers it by loading the
    // first image onto the system clipboard (Linux; the act is
    // cfg-gated, the block is not). John drops a screenshot on a
    // tab, then pastes it into the composer with Ctrl+V. The
    // remaining open question is the paste itself: whether
    // WebKitGTK hands a clipboard image to the page as a File on
    // Ctrl+V is exactly what the field test of THIS build exists
    // to answer — the bridge is what makes that test possible.
    //
    // v0.0.15a — the 0.0.14a answer came back: ZERO bridge log
    // lines on drop. The cursor showed the GTK widget's green +
    // (the drop was accepted at widget level) while the Muse
    // page never even raised its "drag files here" overlay — the
    // in-page arm was starved by the engine, because page JS
    // cannot read an external drop's payload on WebKitGTK. So
    // this round moves the interception to the native layer,
    // where wry reads the uri-list at widget level:
    // - both historical disables are REMOVED (the window config's
    //   `dragDropEnabled: false` and this builder's
    //   `.disable_drag_drop_handler()` from 0.0.12a attempt #2), so
    //   Tauri's own drag-drop handler is attached again, per
    //   webview, exactly as the runtime intends;
    // - each tab webview gets an `on_webview_event` adapter (tab
    //   webviews are WindowChild kind — their drag-drop events
    //   surface as `WebviewEvent::DragDrop`, NOT window events),
    //   and `restore` installs the matching host adapter for the
    //   shelf surface (`WindowEvent::DragDrop`); both normalize
    //   positions to window-relative physical pixels and feed
    //   `handle_drag_drop`, which answers content-area drops with
    //   0.0.14a's clipboard code and ignores shelf drops;
    // - the script's drop arm and the navigation gate's sentinel
    //   branch are REMOVED as dead: the channel they spoke over
    //   provably carries nothing on WebKitGTK. The link arm and
    //   its marker gate are untouched.
    //
    // v0.0.16a — 0.0.15a's pipeline testified perfectly (events,
    // paths, Pixbuf) but its last step vanished: the image set
    // through GTK3's Clipboard never appeared in John's Klipper
    // history and Ctrl+V came up empty. GTK3's clipboard is a
    // weak bridge on Wayland, so only the courier changes: the
    // clipboard act inside `handle_drag_drop` now pipes the PNG
    // bytes to `wl-copy`, whose daemon holds the offer the way
    // KWin respects. Adapters, funnel, guard, logging —
    // untouched.
    //
    // v0.0.17a — the chain worked in the field (drop → wl-copy →
    // John's Ctrl+V attached the image), and John asked for
    // the last manual step gone: "now that we have it landing
    // can we have the software do the pasting?" So a loaded
    // image is now followed by an auto-paste on the dropped-on
    // tab — a focus-aim eval, then WebKit's native Paste
    // editing command through `with_webview` — making the flow
    // drop-only. See `auto_paste_after_drop`. The clipboard
    // still loads first and stands if the paste misses, so
    // Ctrl+V remains the fallback. Linux-only; other platforms
    // keep the 0.0.16a behavior exactly.
    let current_url = Arc::new(Mutex::new(url.clone()));
    let nav_tracker = current_url.clone();
    let opener_app = app.clone();
    let nav_opener_app = app.clone();
    let nav_log_id = entry.id.clone();
    let nw_log_id = entry.id.clone();
    // v0.0.18a — Screening Room branch: a local App page instead of
    // an External URL. Serving from WebviewUrl::App resolves against
    // frontendDist (ui/), so ui/player.html ships in the bundle.
    // EXT_LINK_SCRIPT still rides along — the player page has no
    // external anchors, and its blob: media loads are not http(s),
    // so the interceptor is inert there. Navigations it does make
    // (the tauri:// / tauri.localhost app URL itself) carry no
    // marker and pass the gate below untouched.
    //
    // v0.0.19a rode the enhancer as a SECOND initialization
    // script on remote tabs; v0.0.20a kept that arrangement
    // while its scan pierced shadow roots and same-origin
    // iframes. John's inspector screenshots then proved the
    // chat videos live in the MAIN document all along (see
    // the v0.0.21a note above VIDEO_ENHANCER_SCRIPT), so the
    // shadow/iframe hypothesis is closed — and the unproven
    // assumption underneath the arrangement is exposed: the
    // LINK script provably executes in the Windows WebView2
    // tab (0.0.17a field test) as the FIRST
    // `initialization_script` call, but the enhancer rode a
    // SECOND call, on a "multiple calls accumulate" assumption
    // that was never field-verified.
    //
    // v0.0.21a: exactly ONE `initialization_script` call per
    // remote tab, whose body is the link interceptor IIFE
    // followed by the enhancer IIFE (both keep their own
    // guards, flags and try/catch armour, so concatenation
    // changes nothing about either behaviour) — if the link
    // arm runs, the enhancer arm necessarily runs too. The
    // Screening Room tab keeps the link-only script exactly
    // as before: it gets NO enhancer — player.html is an app
    // page with its own player, and the enhancer's own scope
    // check would refuse it anyway.
    let webview_url = if is_player {
        WebviewUrl::App(PLAYER_PAGE.into())
    } else {
        WebviewUrl::External(url)
    };
    let builder = tauri::webview::WebviewBuilder::new(webview_label(&entry.id), webview_url)
    .data_directory(profile);
    let builder = if is_player {
        builder.initialization_script(EXT_LINK_SCRIPT)
    } else {
        builder.initialization_script(format!(
            "{EXT_LINK_SCRIPT}\n{VIDEO_ENHANCER_SCRIPT}"
        ))
    };
    let builder = builder
    .on_navigation(move |navigated: &tauri::Url| {
        // v0.0.13a gate: a navigation carrying the marker is the
        // in-page script handing us an external link. Open the
        // clean URL in the OS browser and refuse the navigation.
        if let Some(clean) = strip_ext_marker(navigated) {
            eprintln!(
                "[nubisianua-link] tab {nav_log_id}: intercepted external link -> opener: {clean}"
            );
            match nav_opener_app
                .opener()
                .open_url(clean.to_string(), None::<String>)
            {
                Ok(()) => eprintln!(
                    "[nubisianua-link] tab {nav_log_id}: opener Ok — handed to OS default browser: {clean}"
                ),
                Err(e) => eprintln!(
                    "[nubisianua-link] tab {nav_log_id}: opener FAILED: {e} (target was {clean})"
                ),
            }
            return false;
        }
        if let Ok(mut current) = nav_tracker.lock() {
            if !same_origin(&current, navigated) {
                eprintln!(
                    "[nubisianua-link] tab {nav_log_id}: navigation crossed origin: {} -> {} (full: {navigated})",
                    origin_label(&current),
                    origin_label(navigated),
                );
            }
            *current = navigated.clone();
        }
        true
    })
    .on_new_window(move |target, _features| {
        let current = current_url
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_else(|_| target.clone());
        let hostless = target.host_str().is_none();
        let same = same_origin(&target, &current);
        eprintln!(
            "[nubisianua-link] tab {nw_log_id}: new-window request: target={target} current={current} hostless={hostless} same_origin={same}"
        );
        if hostless || same {
            eprintln!(
                "[nubisianua-link] tab {nw_log_id}: decision=Allow (in-app popup)"
            );
            NewWindowResponse::Allow
        } else {
            match opener_app
                .opener()
                .open_url(target.to_string(), None::<String>)
            {
                Ok(()) => eprintln!(
                    "[nubisianua-link] tab {nw_log_id}: decision=Deny, opener Ok — handed to OS default browser: {target}"
                ),
                Err(e) => eprintln!(
                    "[nubisianua-link] tab {nw_log_id}: decision=Deny, opener FAILED: {e} (target was {target})"
                ),
            }
            NewWindowResponse::Deny
        }
    });
    // v0.0.18a: the player webview — and only it — disables Tauri's
    // native drag-drop handler. Every other tab NEEDS that handler
    // (it feeds the clipboard bridge in handle_drag_drop), but the
    // Screening Room handles drops itself, in-page: its page JS
    // reads DataTransfer.files from a local App page, which works
    // — unlike remote pages on WebKitGTK (the 0.0.14a finding).
    // With the native handler attached, wry would finish the drop
    // at widget level and the page would never see it. The
    // per-webview on_webview_event adapter below stays registered
    // but simply never fires for this webview.
    let builder = if is_player {
        builder.disable_drag_drop_handler()
    } else {
        builder
    };
    let webview = window
        .add_child(builder, pos, size)
        .map_err(|e| e.to_string())?;
    // v0.0.15a tab adapter: this webview's drag-drop events arrive
    // as WebviewEvent::DragDrop (WindowChild kind — see the
    // v0.0.15a block above). Its positions are relative to the
    // webview itself, which fills the content area, so the
    // window-relative position is the content origin (shelf width
    // on the dock side, scaled) plus the event position. State is
    // read fresh at event time — shelf width and dock side may
    // have changed since creation.
    let dd_app = app.clone();
    let dd_tag = format!("tab {}", entry.id);
    webview.on_webview_event(move |event| {
        let tauri::WebviewEvent::DragDrop(dd) = event else {
            return;
        };
        let position = match dd {
            tauri::DragDropEvent::Enter { position, .. }
            | tauri::DragDropEvent::Over { position }
            | tauri::DragDropEvent::Drop { position, .. } => *position,
            _ => tauri::PhysicalPosition::new(0.0, 0.0),
        };
        let state = dd_app.state::<TabManager>();
        let (side, shelf_width) = {
            let inner = state.inner.lock().unwrap();
            (inner.config.dock_side, inner.config.shelf_width)
        };
        let Ok(window) = host_window(&dd_app) else {
            return;
        };
        let scale = window.scale_factor().unwrap_or(1.0);
        let origin_x = match side {
            DockSide::Left => shelf_width * scale,
            DockSide::Right => 0.0,
        };
        let window_pos = tauri::PhysicalPosition::new(position.x + origin_x, position.y);
        handle_drag_drop(&dd_app, &state, &dd_tag, dd, window_pos);
    });
    eprintln!(
        "[nubisianua-link] tab {}: webview created — link hooks + in-page ext-link interceptor installed (start: {})",
        entry.id, entry.url
    );
    eprintln!(
        "[nubisianua-dragdrop] tab {}: native drag-drop handling ENABLED — wry handler attached, WebviewEvent::DragDrop feeds the clipboard bridge + auto-paste (v0.0.17a)",
        entry.id
    );
    if active {
        let _ = webview.show();
    } else {
        let _ = webview.hide();
    }
    Ok(webview)
}

/// v0.0.25a — lazy loading: make sure the tab's webview exists,
/// creating it on first use. Only the tab the user is looking at
/// ever loads; every other tab in the shelf stays a label until
/// the user clicks it ("called on"), at which point this builds
/// its webview and loads its URL. Once loaded, a tab stays loaded
/// until it is closed — so two tabs the user has opened both stay
/// live, they just never paid the load cost before being opened.
///
/// Returns true when a webview was actually created. The caller
/// needs that: on Linux a freshly packed child lands AFTER the
/// host widget in the window's GtkBox, which shoves the shelf to
/// the wrong edge (field-proven 0.0.25a: shelf jumped right ->
/// left the first time a lazy tab was clicked) — so a creation
/// must be followed by `linux_layout_pass`, exactly as
/// `create_tab` already does after its own creation.
///
/// MUST be called with the TabManager lock NOT held: `create_webview`
/// installs hooks that read state (the drag-drop adapter), and
/// creation itself must stay out of sync handlers (Windows
/// deadlock rule). The fast path takes the lock only to check.
fn ensure_webview(
    app: &AppHandle,
    state: &State<'_, TabManager>,
    tab_id: &str,
) -> Result<bool, String> {
    {
        let inner = state.inner.lock().unwrap();
        if inner.webviews.contains_key(tab_id) {
            return Ok(false);
        }
    }
    let (entry, side, shelf_width) = {
        let inner = state.inner.lock().unwrap();
        let Some(entry) = inner.config.tabs.iter().find(|t| t.id == tab_id).cloned() else {
            return Err(format!("unknown tab {tab_id}"));
        };
        (entry, inner.config.dock_side, inner.config.shelf_width)
    };
    // Created visible: the caller is about to make this the active tab.
    let webview = create_webview(app, &entry, true, side, shelf_width)?;
    let mut inner = state.inner.lock().unwrap();
    if inner.webviews.contains_key(tab_id) {
        // A concurrent path built it while we worked — keep the
        // first, close the spare instead of leaking a webview.
        drop(inner);
        let _ = webview.close();
        return Ok(false);
    }
    inner.webviews.insert(tab_id.to_string(), webview);
    eprintln!("[nubisianua-lazy] tab {tab_id}: webview created on demand — {}", entry.url);
    Ok(true)
}

/// Create a webview ONLY for the active tab in the loaded config
/// (v0.0.25a lazy loading — the rest stay labels until clicked).
/// Runs from the async task spawned in setup — not from setup
/// itself (deadlock rule).
pub async fn restore(app: &AppHandle, state: &State<'_, TabManager>) {
    // v0.0.15a: the host (shelf) surface's drag-drop adapter —
    // installed once, before any webview exists.
    install_host_drag_drop_adapter(app);
    let (entries, side, shelf_width) = {
        let inner = state.inner.lock().unwrap();
        let active = inner.config.active_tab.clone();
        (
            inner
                .config
                .tabs
                .iter()
                .map(|t| (t.clone(), Some(&t.id) == active.as_ref()))
                .collect::<Vec<(TabEntry, bool)>>(),
            inner.config.dock_side,
            inner.config.shelf_width,
        )
    };
    // Linux: put the window's GtkBox into shelf layout BEFORE the first
    // child is packed into it (queued ahead of creation on the main
    // thread), and again after, to settle the dock order (v0.0.8a).
    linux_layout_pass(app, side, shelf_width);
    // v0.0.25a lazy loading: only the active tab's webview is
    // created at launch. Every other tab stays a shelf label until
    // the user clicks it (ensure_webview, via activate_tab).
    for (entry, is_active) in entries {
        if !is_active {
            continue;
        }
        match create_webview(app, &entry, is_active, side, shelf_width) {
            Ok(webview) => {
                let mut inner = state.inner.lock().unwrap();
                inner.webviews.insert(entry.id.clone(), webview);
            }
            Err(e) => eprintln!("nubisianua: failed to restore tab {}: {e}", entry.id),
        }
    }
    // v0.0.26a: no lock held across apply_visibility (see its docs) —
    // it locks internally in the safe order.
    apply_visibility(app, state);
    let inner = state.inner.lock().unwrap();
    linux_layout_pass(app, inner.config.dock_side, inner.config.shelf_width);
    emit_snapshot(app, &inner);
    // If every webview failed to create, the shelf still shows the tab list
    // and the + button works — the app never opens windowless (spec §7.6).
}

/// Re-fit the active webview after the host window resizes. Called from the
/// window event handler in main.rs — resizes only, never creates.
///
/// v0.0.26a: this runs ON the main thread, so it must never block on
/// `state.inner` — the async side never holds the lock across a
/// main-thread round-trip anymore (see `apply_visibility`), so the
/// short lock below only ever waits out a mutation, never a deadlock.
pub fn layout(app: &AppHandle, state: &State<'_, TabManager>) {
    if state.inner.lock().unwrap().webviews.is_empty() {
        return;
    }
    apply_visibility(app, state);
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
        .map(|(id, name, url)| Preset {
            id: id.to_string(),
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
    // v0.0.18a: the Screening Room preset's sentinel is the one
    // non-http(s) URL allowed through — exact match only, checked
    // before the https:// prepend below could mangle it. It is not
    // a network address; create_webview branches on it to the
    // bundled local player page. This also covers restore: tabs.json
    // stores the sentinel and create paths re-validate nothing on
    // restore, but picker-created player tabs come through here.
    if trimmed == PLAYER_URL {
        return Ok(PLAYER_URL.to_string());
    }
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
    let (entry, side, shelf_width) = {
        let inner = state.inner.lock().unwrap();
        (
            TabEntry {
                id: inner.config.next_id(),
                name,
                url,
            },
            inner.config.dock_side,
            inner.config.shelf_width,
        )
    };
    // Create BEFORE mutating state, with no lock held: if creation fails,
    // the config was never touched.
    let webview = create_webview(&app, &entry, true, side, shelf_width)?;
    let mut inner = state.inner.lock().unwrap();
    inner.webviews.insert(entry.id.clone(), webview);
    inner.config.tabs.push(entry.clone());
    inner.config.active_tab = Some(entry.id.clone());
    config::save(&app, &inner.config);
    drop(inner);
    // v0.0.26a: no lock held across apply_visibility (see its docs).
    apply_visibility(&app, &state);
    let inner = state.inner.lock().unwrap();
    // Linux: the new child was just packed into the window's GtkBox —
    // settle the dock order and shelf width around it (v0.0.8a).
    linux_layout_pass(&app, inner.config.dock_side, inner.config.shelf_width);
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
    {
        let inner = state.inner.lock().unwrap();
        if !inner.config.tabs.iter().any(|t| t.id == id) {
            return Err(format!("unknown tab {id}"));
        }
    }
    // v0.0.25a lazy loading: a tab that has never been opened gets
    // its webview built (and its URL loaded) here, on first click.
    // No lock held across creation (see ensure_webview).
    let created = ensure_webview(&app, &state, &id)?;
    let mut inner = state.inner.lock().unwrap();
    inner.config.active_tab = Some(id.clone());
    config::save(&app, &inner.config);
    drop(inner);
    // v0.0.26a: no lock held across apply_visibility (see its docs).
    apply_visibility(&app, &state);
    let inner = state.inner.lock().unwrap();
    // A fresh child was packed after the host widget in the
    // window's GtkBox — re-settle the dock order and shelf width
    // around it (v0.0.8a), as create_tab does after creation.
    if created {
        linux_layout_pass(&app, inner.config.dock_side, inner.config.shelf_width);
    }
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

    let new_active = {
        let mut inner = state.inner.lock().unwrap();
        let Some(pos) = inner.config.tabs.iter().position(|t| t.id == id) else {
            return Err(format!("unknown tab {id}"));
        };
        let was_active = inner.config.active_tab.as_deref() == Some(id.as_str());
        inner.config.tabs.remove(pos);
        if let Some(webview) = inner.webviews.remove(&id) {
            let _ = webview.close();
        }
        let next = if was_active {
            // Neighbor rule: the tab that slid into the closed tab's slot, or
            // the new last tab if the closed one was last in the list.
            let next = inner
                .config
                .tabs
                .get(pos)
                .or_else(|| inner.config.tabs.last())
                .map(|t| t.id.clone());
            inner.config.active_tab = next.clone();
            next
        } else {
            None
        };
        config::save(&app, &inner.config);
        next
    };
    // v0.0.25a lazy loading: the neighbor that just became active may
    // never have been opened — build its webview here. No lock held
    // across creation. (If it was already loaded, this is a no-op.)
    let created = match new_active {
        Some(next_id) => ensure_webview(&app, &state, &next_id)?,
        None => false,
    };
    // v0.0.26a: no lock held across apply_visibility (see its docs).
    apply_visibility(&app, &state);
    let inner = state.inner.lock().unwrap();
    if created {
        linux_layout_pass(&app, inner.config.dock_side, inner.config.shelf_width);
    }
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
    let next_id = {
        let inner = state.inner.lock().unwrap();
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
        inner.config.tabs[next].id.clone()
    };
    // v0.0.25a lazy loading: cycling onto a never-opened tab builds
    // its webview here. No lock held across creation.
    let created = ensure_webview(&app, &state, &next_id)?;
    let mut inner = state.inner.lock().unwrap();
    inner.config.active_tab = Some(next_id);
    config::save(&app, &inner.config);
    drop(inner);
    // v0.0.26a: no lock held across apply_visibility (see its docs).
    apply_visibility(&app, &state);
    let inner = state.inner.lock().unwrap();
    if created {
        linux_layout_pass(&app, inner.config.dock_side, inner.config.shelf_width);
    }
    emit_snapshot(&app, &inner);
    Ok(snapshot(&inner))
}

/// Set the shelf width (v0.0.5a). Driven by the drag handle on the shelf's
/// inner edge: during the drag the frontend calls this with
/// `persist = false` on every settled frame, so the content webview
/// re-fits live; on release it calls once with `persist = true` to save
/// and broadcast. Resizes/repositions existing webviews only — safe from
/// any handler (Windows deadlock rule covers creation, not layout).
#[tauri::command]
pub async fn set_shelf_width(
    app: AppHandle,
    state: State<'_, TabManager>,
    width: f64,
    persist: bool,
) -> Result<TabSnapshot, String> {
    let mut inner = state.inner.lock().unwrap();
    inner.config.shelf_width = width.clamp(SHELF_MIN_WIDTH, SHELF_MAX_WIDTH);
    if persist {
        config::save(&app, &inner.config);
    }
    drop(inner);
    // v0.0.26a: no lock held across apply_visibility (see its docs).
    apply_visibility(&app, &state);
    let inner = state.inner.lock().unwrap();
    // Linux: the shelf's strip IS the host webview's size request in the
    // window's GtkBox — this is what actually resizes it there (v0.0.8a).
    linux_layout_pass(&app, inner.config.dock_side, inner.config.shelf_width);
    if persist {
        emit_snapshot(&app, &inner);
    }
    Ok(snapshot(&inner))
}

/// Set which window edge the shelf docks to (v0.0.5a). Chosen from the
/// shelf's settings (gear) menu — "Shelf side: Left / Right" — mirroring
/// the Firefox Settings pattern John asked for; never by dragging the
/// shelf. Persisted immediately; the content webview re-fits to the
/// remaining area (x = 0 when docked right, x = shelf width when left).
#[tauri::command]
pub async fn set_dock_side(
    app: AppHandle,
    state: State<'_, TabManager>,
    side: String,
) -> Result<TabSnapshot, String> {
    let side = DockSide::parse(&side).ok_or_else(|| format!("unknown dock side {side:?}"))?;
    let mut inner = state.inner.lock().unwrap();
    inner.config.dock_side = side;
    config::save(&app, &inner.config);
    drop(inner);
    // v0.0.26a: no lock held across apply_visibility (see its docs).
    apply_visibility(&app, &state);
    let inner = state.inner.lock().unwrap();
    // Linux: dock side is the host widget's position in the GtkBox
    // (first = left, last = right) — move it (v0.0.8a).
    linux_layout_pass(&app, inner.config.dock_side, inner.config.shelf_width);
    emit_snapshot(&app, &inner);
    Ok(snapshot(&inner))
}
