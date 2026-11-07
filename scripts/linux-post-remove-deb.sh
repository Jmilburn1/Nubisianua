# Nubisianua — DEB post-removal script (postrm).
# $1 is the dpkg action: on "remove"/"purge" the package's own files are
# gone, so remove the lowercase desktop-entry twin left by the
# post-install script as well. On "upgrade" the new package stays, and
# its postinst has already refreshed the twin — leave it alone.

case "$1" in
    remove|purge)
        rm -f /usr/share/applications/nubisianua.desktop || true
        gtk-update-icon-cache -f -t /usr/share/icons/hicolor >/dev/null 2>&1 || true
        update-desktop-database /usr/share/applications >/dev/null 2>&1 || true
        ;;
esac

exit 0
