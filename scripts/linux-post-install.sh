# Nubisianua — Linux package post-install script
# (RPM %post / DEB postinst; wired in tauri.linux.conf.json)
#
# Why this exists (v0.0.9a): Tauri's bundler names the desktop entry after
# the product name — /usr/share/applications/Nubisianua.desktop — while
# the running window's Wayland app_id is the binary name, "nubisianua"
# (Tauri only sets a GTK application id from the bundle identifier when
# app.enableGTKAppId is on; it is off, so GTK falls back to the program
# name). KDE matches a window to its desktop entry by that app_id, case
# included, so the capital-N file was never matched: no taskbar icon, no
# window icon. The entry's own Icon=nubisianua and the hicolor icon files
# were always correct.
#
# Fix: leave a lowercase twin of the entry, marked NoDisplay so menus
# still show exactly one Nubisianua (the original), while window
# matching finds nubisianua.desktop. Then refresh the icon/desktop
# caches so a just-installed package shows its icon in the same session.
# Every step is best-effort: nothing here may ever fail an install.

DESKTOP_DIR=/usr/share/applications
SRC="$DESKTOP_DIR/Nubisianua.desktop"
TWIN="$DESKTOP_DIR/nubisianua.desktop"

if [ -f "$SRC" ]; then
    sed 's/^Type=Application[[:space:]]*$/Type=Application\nNoDisplay=true/' "$SRC" > "$TWIN" 2>/dev/null || true
    chmod 644 "$TWIN" 2>/dev/null || true
fi

# v0.0.10a: the bundler preserves the build tree's file modes when it
# stages the icons, and a 660 source lands root-owned and unreadable by
# the user's session — right name, right place, blank icon. Whatever the
# build tree said, the installed icons end up world-readable.
chmod 644 /usr/share/icons/hicolor/*/apps/nubisianua.png 2>/dev/null || true

gtk-update-icon-cache -f -t /usr/share/icons/hicolor >/dev/null 2>&1 || true
update-desktop-database "$DESKTOP_DIR" >/dev/null 2>&1 || true

exit 0
