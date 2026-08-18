#!/bin/sh
# Per-user installer for INXM Local. Installs into $HOME/.local — no root
# required. Run from inside the unpacked release tarball:
#
#   ./install.sh              install (or update) for the current user
#   ./install.sh --autostart  also start INXM Local hidden at login
#   ./install.sh --uninstall  remove a previous per-user install
#
# PREFIX overrides the install root (default: $HOME/.local).
set -eu

PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"
ICON_PATH="$PREFIX/share/icons/hicolor/512x512/apps/ai.inxm.local.png"
DESKTOP_PATH="$PREFIX/share/applications/ai.inxm.local.desktop"
DOC_DIR="$PREFIX/share/doc/inxm-local"
AUTOSTART_PATH="${XDG_CONFIG_HOME:-$HOME/.config}/autostart/ai.inxm.local.desktop"

AUTOSTART=0
UNINSTALL=0
for arg in "$@"; do
    case "$arg" in
        --autostart) AUTOSTART=1 ;;
        --uninstall) UNINSTALL=1 ;;
        *) echo "Unknown option: $arg" >&2; exit 2 ;;
    esac
done

if [ "$UNINSTALL" -eq 1 ]; then
    rm -f "$BIN_DIR/inxm-local" "$ICON_PATH" "$DESKTOP_PATH" "$AUTOSTART_PATH"
    rm -rf "$DOC_DIR"
    echo "INXM Local removed from $PREFIX (user data in ~/.local/share is kept)."
    exit 0
fi

HERE="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
if [ ! -f "$HERE/inxm-local" ]; then
    echo "inxm-local binary not found next to install.sh — run this from the unpacked release tarball." >&2
    exit 1
fi

install -Dm755 "$HERE/inxm-local" "$BIN_DIR/inxm-local"
install -Dm644 "$HERE/ai.inxm.local.png" "$ICON_PATH"
for doc in LICENSE NOTICE THIRD_PARTY_LICENSES.md; do
    [ -f "$HERE/$doc" ] && install -Dm644 "$HERE/$doc" "$DOC_DIR/$doc"
done

# Absolute Exec path: ~/.local/bin is not on PATH in every desktop session.
mkdir -p "$(dirname "$DESKTOP_PATH")"
cat > "$DESKTOP_PATH" <<EOF
[Desktop Entry]
Type=Application
Name=INXM Local
Comment=Local-first compiled-AI workflows
Exec=$BIN_DIR/inxm-local
Icon=ai.inxm.local
Terminal=false
Categories=Development;Utility;
StartupWMClass=ai.inxm.local
EOF

if [ "$AUTOSTART" -eq 1 ]; then
    mkdir -p "$(dirname "$AUTOSTART_PATH")"
    cat > "$AUTOSTART_PATH" <<EOF
[Desktop Entry]
Type=Application
Name=INXM Local
Comment=Local-first compiled-AI workflows
Exec=$BIN_DIR/inxm-local --start-hidden
Icon=ai.inxm.local
Terminal=false
X-GNOME-Autostart-enabled=true
EOF
    echo "Autostart enabled: INXM Local will start hidden in the tray at login."
else
    echo "Tip: rerun with --autostart to start INXM Local at login."
fi

command -v update-desktop-database >/dev/null 2>&1 && \
    update-desktop-database "$PREFIX/share/applications" >/dev/null 2>&1 || true

echo "Installed INXM Local to $BIN_DIR/inxm-local."
