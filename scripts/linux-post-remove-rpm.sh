# Nubisianua — RPM post-uninstall script (%postun).
# $1 is the number of packages of this name that remain: 0 means final
# erase (remove the lowercase desktop-entry twin left by the post-install
# script), 1 means an upgrade is in progress (leave everything alone —
# the new package's %post has already refreshed the twin).

if [ "$1" -eq 0 ] 2>/dev/null; then
    rm -f /usr/share/applications/nubisianua.desktop || true
    gtk-update-icon-cache -f -t /usr/share/icons/hicolor >/dev/null 2>&1 || true
    update-desktop-database /usr/share/applications >/dev/null 2>&1 || true
fi

exit 0
