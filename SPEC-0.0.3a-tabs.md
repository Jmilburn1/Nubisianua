# Nubisianua — SPEC v0.0.3a: Tabs (Multiple Agents, One Window)

**Status:** Design spec, approved to write 2026-11-06. Build approved
2026-11-06 as v0.0.3a (the tabs release slid one number: v0.0.2a shipped
the quarter-screen resize fix first).
**Base:** v0.0.2a — one Tauri 2 window, no frontend, loads `https://muse.ai`,
min size 480×360.
**Goal of 0.0.3a:** One Nubisianua window holding several agents in tabs, each tab
signed in separately, restored on launch.

---

## 1. What the user sees

- A slim **tab strip** across the top of the window (dark, matching the app).
  Below it, the active tab's agent fills the rest of the window.
- **+ button** at the right end of the strip opens a **pick-an-agent menu**:
  the preset list (§2) plus "Custom URL…". Choosing one opens a new tab on it.
- Each tab shows the agent's name (editable label, see below) and a **× close
  button**. Closing a tab closes that webview; its saved session stays on disk
  unless the user removes the profile (see §4 — out of scope for 0.0.3a UI,
  profiles can be deleted by hand).
- **Rename:** double-click a tab label to rename it (e.g. two Muse tabs named
  "Albert" and "Work"). Stored in the tab config; the page title never
  overwrites a user-set name.
- Switching tabs **shows/hides** webviews — it does not reload them. A chat in
  progress in a hidden tab stays exactly where it was.
- **Restore on launch:** Nubisianua reopens the same set of tabs, in the same
  order, with the same active tab, signed in as before. First run ever: one
  Muse tab (§2).
- Keyboard: `Ctrl+Tab` / `Ctrl+Shift+Tab` cycle tabs, `Ctrl+T` new tab,
  `Ctrl+W` closes the current tab (with the usual care: never the last tab —
  closing the last tab closes the app, same as closing the window).
- The window title follows the active tab: `Nubisianua — <tab name>`.

## 2. Agents

**Default tab (first run):** Muse — `https://muse.ai`. This is the known-good
tab: Nubisianua was built for it and v0.0.1a already proves it signs in and
works inside the webview.

**Preset list** (name + start URL):

| Name              | URL                              |
|-------------------|----------------------------------|
| Muse              | https://muse.ai                  |
| ChatGPT           | https://chatgpt.com              |
| Claude            | https://claude.ai                |
| Gemini            | https://gemini.google.com        |
| Microsoft Copilot | https://copilot.microsoft.com    |
| Grok              | https://grok.com                 |
| Perplexity        | https://www.perplexity.ai        |
| Meta AI           | https://www.meta.ai              |
| DeepSeek          | https://chat.deepseek.com       |
| Qwen              | https://chat.qwen.ai             |
| Kimi              | https://www.kimi.com             |
| Le Chat (Mistral) | https://chat.mistral.ai          |
| Doubao            | https://www.doubao.com           |
| Custom URL…       | user-entered (validated: `https://`, or `http://` to loopback only) |

Preset list expanded 2026-11-06 (John): the picker carries the most popular
AI chat assistants **worldwide**, not a US list plus tokens. All URLs
verified 2026-11-06. Field notes from verification:

- **Le Chat** was renamed **Mistral Vibe** (May 2026) — same address, same
  account; the preset keeps the Le Chat name John knows, the site it opens
  calls itself Vibe. **Qwen Chat** likewise now brands itself *Qwen Studio*
  at the same address, `chat.qwen.ai`.
- **Doubao** is ByteDance's assistant, huge in China. `www.doubao.com`
  redirects to its `/chat` page, but it is China-first: from outside
  mainland China it shows a regional-restriction notice, and sign-in needs
  a mainland (+86) phone number, a Douyin account, or an Apple ID.
  ByteDance's international sibling is a separate product (Dola). The preset
  stays — the web chat exists at that address — with this caveat attached.
- **Microsoft Copilot**: personal Microsoft accounts sign in as expected;
  work (Entra ID) accounts are redirected by Microsoft to the Microsoft 365
  Copilot Chat experience.
- **Kimi**: `www.kimi.com` is Moonshot's global site (the old
  `kimi.moonshot.cn` redirects there).
- **Custom URL** is also the door for the open-weight siblings — Muse
  Glimmer, Muse Spark Open, or anything else a user runs on their own
  machine: once it's installed and serving locally, they add it here by
  its local address. Validation therefore accepts plain `http://` **only**
  for loopback hosts (`localhost`, `127.0.0.1`, `::1`); remote addresses
  still require `https://`. The picker words the option accordingly:
  "Custom URL — any agent, including a local model you run yourself
  (Glimmer, Spark Open, …)". No presets for these — every local install
  lives at a different address/port.

Notes:

- Presets are just start URLs + labels in the config — no special-casing per
  provider in code. Adding a preset later is a data edit, not a feature.
- The same preset can be opened **more than once** (two Muse tabs with
  different accounts is a legitimate use — that's what per-tab isolation in
  §4 is for).
- Sign-in compatibility differs per provider — see Risks (§6). Muse is the
  guaranteed one; the others are "load and try" until tested.

## 3. Architecture options

### Option A — True multi-webview in one window (`Window::add_child`)

One host window; each tab is a child `Webview` attached with
`Window::add_child(WebviewBuilder::new(label, WebviewUrl::External(url)))`.
A minimal frontend in the host window renders the tab strip; Rust shows/hides
and resizes child webviews as tabs switch and the window resizes.

- **Pro:** Real tabs — one window, one taskbar entry, instant switching, each
  webview fully independent (own profile via `data_directory`, §4).
- **Con:** `add_child` / `WebviewBuilder` multi-webview APIs are gated behind
  Tauri's Cargo feature **`unstable`** — Tauri explicitly warns breaking
  changes can land in minor releases. Adopting it means **pinning the Tauri
  version** in `Cargo.toml`/`Cargo.lock` and bumping deliberately.
- **Con:** This is the project's **first frontend** (see §5). Today's app has
  none — the window is created in Rust pointing straight at an external URL.
  The host window needs a local page for the tab strip, and Rust/frontend
  messaging (`invoke` + events) to drive tab operations.
- **Rule (Windows):** child webviews must be created from **async commands or
  a separate thread** — creating one inside a synchronous command/event
  handler can deadlock WebView2.

### Option B — Multiple `WebviewWindow`s (one window per agent)

Each "tab" is really its own top-level window created with the stable
`WebviewWindowBuilder` (the exact API v0.0.1a already uses); a small launcher
window or tray menu switches between them by focus/show/hide.

- **Pro:** 100% stable APIs, no `unstable` feature, per-window `data_directory`
  isolation works the same way.
- **Con:** They are not tabs. Separate taskbar entries, separate window
  frames, alt-tab noise — "tabs" becomes cosmetic window management, and it
  gets worse with every added agent. Fails the actual request: *more than one
  agent in it* (one window).

### Option C — One webview, tabs just switch its URL — **REJECTED**

A tab strip over a single webview; switching tabs navigates the one webview
to the other agent's URL.

- **Rejected because:** one webview = one profile = **one shared session**.
  All tabs share cookies/storage, so separate sign-ins per agent (the core
  point) are impossible, and every switch reloads and loses the other tab's
  place. It looks like tabs and behaves like bookmarks.

### Recommendation: **Option A**

It's the only option that delivers the thing asked for — real tabs, one
window, separate sign-ins. The `unstable` cost is manageable and honest:
the feature gates APIs that already exist and are used by other Tauri apps,
and the exposure is limited to one module (tab/webview management). Mitigate
by (1) pinning `tauri` to the exact version in `Cargo.lock` and only bumping
it in dedicated, tested steps, and (2) keeping all multi-webview calls behind
one Rust module so an API change has exactly one place to fix. Option B stays
documented as the fallback if `unstable` ever breaks badly enough to matter.

## 4. Per-tab isolation and persistence

**Isolation — `data_directory` (stable API, no `unstable` needed):**

- Every tab gets its **own profile directory**:
  `<app data dir>/profiles/<tab-id>/`, passed via
  `WebviewBuilder::data_directory(path)` at creation.
- On Windows (WebView2) that directory is the webview's User Data Folder —
  its own cookies, localStorage, IndexedDB and cache. On Linux (WebKitGTK)
  it likewise separates the webview's website data. Two tabs sharing a
  directory would share a session; Nubisianua never shares.
- Consequence: two tabs on the *same* agent can hold **two different
  accounts**, and no agent can see another agent's cookies. Sign-ins persist
  across restarts because the directories persist.

**Tab list persistence — small JSON config:**

- `<app config dir>/tabs.json`, written on every tab change (add/close/
  rename/reorder/activate), read once at startup. Illustrative shape:

```json
{
  "version": 1,
  "activeTab": "t2",
  "tabs": [
    { "id": "t1", "name": "Muse",   "url": "https://muse.ai" },
    { "id": "t2", "name": "Albert", "url": "https://muse.ai" }
  ]
}
```

- `id` is a stable random id; it names the profile directory, so a tab keeps
  its session across renames and reorders. The config stores **URLs and
  labels only — never credentials**; sign-in state lives in the webview
  profiles themselves.
- Corrupt/missing config → fall back to the single default Muse tab rather
  than failing to start.

## 5. What changes in the codebase

File-level, against the v0.0.1a layout (crate root = app directory):

| File | Change |
|---|---|
| `Cargo.toml` | Enable `features = ["unstable"]` on `tauri`; pin the resolved version (lock it in `Cargo.lock`, bump only deliberately). No new crates expected. |
| `tauri.conf.json` | Windows array now defines **one host window** (no external URL) sized as today (1200×850, min 480×360), loading the local frontend (`index.html`). Bundle targets unchanged (NSIS first). |
| `index.html` + `src-ui/` (new) | **First frontend:** the tab strip page — strip, + button, agent menu, rename field — plus a small JS file that calls Rust commands (`invoke`) and listens for tab-state events. No framework, no build step; plain HTML/CSS/JS kept deliberately tiny and dark-themed. |
| `src/main.rs` | Slimmed down: builds the host window only; window/tab logic moves out to the modules below. |
| `src/tabs.rs` (new) | All multi-webview code lives here and nowhere else: create tab (async command — Windows deadlock rule), show/hide on switch, resize children on window resize, close tab. |
| `src/config.rs` (new) | Load/save `tabs.json` (§4), profile-dir resolution (`profiles/<tab-id>`), first-run default (one Muse tab), corrupt-config fallback. |
| `capabilities/default.json` | Host window keeps `core:default`; the local frontend needs permission to call the app's own tab commands (custom commands are app-defined, listed in the invoke handler — no extra plugin permissions). Child webviews loading **external** URLs get **no** Tauri IPC capability: remote pages must not reach the app's commands. |
| `README.md` | Document tabs, the preset list, the `unstable` decision + version pin, and the per-provider sign-in caveat (§6). |

Unchanged: icons, LICENSE, bundle identifier (`dev.farmerjay.nubisianua`),
dark background color on the host window (avoids white flash while the strip
loads).

## 6. Risks and unknowns

1. **`unstable` feature churn.** Tauri may change the multi-webview API in a
   minor release. Mitigation: version pinned; upgrades are their own tested
   step, never a side effect. Fallback if it ever becomes untenable: Option B.
2. **Windows webview-creation deadlock.** Creating child webviews inside a
   synchronous handler can hang WebView2. Rule is known and cheap: all tab
   creation goes through async commands (already the design in §5).
3. **Linux WebKitGTK multi-webview behavior is untested.** Multiple WebKitGTK
   webviews in one window, show/hide behavior, resize handling, and
   `data_directory` separation under WebKitGTK all need a real pass on
   **Dell 2** (the fleet canary, Rawhide) before Linux is claimed working.
   Windows-first stands: 0.0.3a ships when Windows passes §7; Linux results
   get recorded either way.
4. **Provider sign-in restrictions (open risk, per provider).** Some sign-in
   flows — Google OAuth in particular — have historically refused or broken
   inside embedded webviews. **There is no cheap workaround:** opening the
   sign-in in the system browser does *not* carry the session back into the
   webview, so "just log in via Firefox" is not a fix. Treat every non-Muse
   preset as **unproven until signed in successfully inside the app**; if a
   provider blocks its webview sign-in, that provider's preset gets flagged
   in the README (not silently dropped) and revisited if the provider changes
   policy. Muse/`muse.ai` is the known-good default.
5. **RAM honesty.** Each tab is a real webview with a real session — tabs cost
   roughly a browser tab each. The win vs. Firefox is the missing browser
   around them, not magic; §7 measures it instead of assuming it.
6. **Profile growth.** One profile directory per tab accumulates cache over
   time. Acceptable for 0.0.3a; a "clear tab data" control is future work.

## 7. Acceptance tests (John's hands-on pass, MAIN)

Build: `cargo tauri build` on MAIN, install the NSIS `.exe`, then:

1. **First run:** app opens with exactly one tab, Muse, signed in (or
   sign-in completes inside the app) — same as 0.0.1a, plus the tab strip.
2. **Two agents at once:** add a second preset tab; sign into it *while the
   Muse tab stays signed in*. Switch back and forth — neither tab reloads,
   neither sign-in leaks into the other.
3. **Two accounts, one agent:** add a second Muse tab; sign it into a
   different state/account than the first (or at minimum confirm the two
   tabs' sessions are independent — activity in one does not appear in the
   other). Rename both tabs; names stick.
4. **Restart restore:** close the app with 3 tabs open, relaunch — same tabs,
   same order, same active tab, **still signed in everywhere**, no reload of
   the setup.
5. **Custom URL:** add a custom-URL tab; it loads, persists across restart,
   and closes cleanly.
6. **Tab hygiene:** `Ctrl+T` / `Ctrl+W` / `Ctrl+Tab` behave; closing a tab
   does not disturb the others; the app never ends up windowless with tabs
   still "open."
7. **RAM comparison:** with the same agents open in Nubisianua tabs and in
   Firefox, compare Task Manager totals (the 0.0.1a motivation: Firefox was
   holding ~2.5 GB). Record both numbers, whatever they say.
8. **Known-risk check (informational):** for each non-Muse preset tried,
   note whether sign-in completed inside the webview or was blocked — this
   fills in the §6.4 table with facts instead of guesses.

Pass bar: 1–6 clean on Windows. 7–8 are measurements, recorded either way.
Linux (Dell 2) repeats 1–4 when the Linux build exists — separate result,
does not block the Windows call.
