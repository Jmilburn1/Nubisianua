# Nubisianua 0.0.9a — Linux package icons: diagnostic groundwork

Prepared 2026-11-06 (proactive research). Scope: icons only, per the 0.0.9a
single-item rule.

## Symptom (John, Dell 2, after RPM install of 0.0.8+a)
- No icon in the app menu, no icon on the taskbar, wrong icon in the app window.
- Desktop entry reads `Icon=nubisianua` (all lowercase).

## What the current project config actually does (verified locally)
- `tauri.conf.json`: version `0.0.8-a`, productName `Nubisianua`,
  identifier `dev.farmerjay.nubisianua`, bundle targets `["nsis"]`,
  bundle icon list: 32x32, 128x128, 128x128@2x, icon.icns, icon.ico, icon.png.
- All PNG icons present on disk and valid sizes: 32x32, 128x128,
  128x128@2x (256x256 physical), icon.png (512x512).
- `Cargo.toml` package name is `nubisianua` (lowercase) — this is the
  "main binary name" the Linux bundler uses.
- `tauri.linux.conf.json` only overrides the version (`0.0.8+a`); nothing
  icon-related. Linux bundles are produced by CLI override
  (`--bundles appimage,rpm`), not by the targets list.

## What the Tauri bundler does with that (verified from source)
Read in the local cargo registry copy of tauri-bundler 2.10.4,
`src/bundle/linux/freedesktop/mod.rs` and `src/bundle/linux/rpm.rs`:
- For RPM (and deb), each bundle PNG icon is installed to
  `/usr/share/icons/hicolor/<W>x<H>/apps/<main_binary_name>.png`
  (the @2x file goes to a `256x256@2` style high-density path).
- The generated `.desktop` file is named `<productName>.desktop`
  (i.e. `Nubisianua.desktop`) and its `Icon=` field is set to the main
  binary name — `nubisianua`.
- So on paper the wiring is CONSISTENT: `Icon=nubisianua` should resolve
  to `/usr/share/icons/hicolor/*/apps/nubisianua.png`. The config as it
  stands does not contain an obvious naming mismatch.

## What that rules in / out
- NOT a bare-binary effect: the fault appears only via the installed
  package (already established).
- Two live suspects remain:
  1. The icons are in the RPM but the installed system never refreshed
     its icon/desktop caches (Tauri-generated RPMs run no icon-cache
     post-install script), so the menu shows nothing until a cache
     rebuild / re-login. Window/taskbar icon on Wayland/KDE additionally
     depends on the running window's app-id matching a desktop file —
     the desktop file is `Nubisianua.desktop` while the app identifier
     is `dev.farmerjay.nubisianua`; a mismatch there produces exactly a
     generic taskbar/window icon even when the menu icon works.
  2. The icons never made it into the RPM John built on Dell 2 (e.g. a
     different `cargo tauri` CLI version there than the 2.10.4 bundler
     read here).

## The one check that separates them (not yet supplied)
On Dell 2:
```
rpm -ql Nubisianua | grep -Ei 'icon|desktop|png'
```
- Icons listed under /usr/share/icons/hicolor/.../apps/nubisianua.png
  → suspect 1: fix is cache refresh / desktop-file app-id alignment,
  may not need any icon-file change at all.
- No icon files in the list → suspect 2: the Dell 2 bundler dropped
  them; check `cargo tauri --version` on Dell 2 and rebuild there.

## Not verified
- A local test RPM could NOT be built in the agent VM: no `cargo tauri`
  CLI and no webkit2gtk dev libraries installed there. The bundler
  behavior above is from reading the bundler's source, not from
  inspecting a built RPM. The `rpm -ql` output from Dell 2 is still the
  decisive artifact.
