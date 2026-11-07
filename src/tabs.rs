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
pub const PRESETS: &[(&str, &str, &str)] = &[
    ("muse", "Muse", "https://muse.ai"),
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
fn apply_visibility(app: &AppHandle, inner: &Inner) {
    let Ok(window) = host_window(app) else {
        return;
    };
    let (pos, size) = content_bounds(&window, inner.config.dock_side, inner.config.shelf_width);
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
    let builder = tauri::webview::WebviewBuilder::new(
        webview_label(&entry.id),
        WebviewUrl::External(url),
    )
    .data_directory(profile)
    .initialization_script(EXT_LINK_SCRIPT)
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

/// Create webviews for every tab in the loaded config. Runs from the async
/// task spawned in setup — not from setup itself (deadlock rule).
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
    for (entry, is_active) in entries {
        match create_webview(app, &entry, is_active, side, shelf_width) {
            Ok(webview) => {
                let mut inner = state.inner.lock().unwrap();
                inner.webviews.insert(entry.id.clone(), webview);
            }
            Err(e) => eprintln!("nubisianua: failed to restore tab {}: {e}", entry.id),
        }
    }
    let inner = state.inner.lock().unwrap();
    apply_visibility(app, &inner);
    linux_layout_pass(app, inner.config.dock_side, inner.config.shelf_width);
    emit_snapshot(app, &inner);
    // If every webview failed to create, the shelf still shows the tab list
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
    apply_visibility(&app, &inner);
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
    apply_visibility(&app, &inner);
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
    apply_visibility(&app, &inner);
    // Linux: dock side is the host widget's position in the GtkBox
    // (first = left, last = right) — move it (v0.0.8a).
    linux_layout_pass(&app, inner.config.dock_side, inner.config.shelf_width);
    emit_snapshot(&app, &inner);
    Ok(snapshot(&inner))
}
