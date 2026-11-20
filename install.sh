#!/bin/sh
# Build peepin and put it in ~/.local, with its icon and menu entry.
#
#   ./install.sh              build and install
#   ./install.sh --uninstall  remove what this script put there
#
# PREFIX=~/somewhere ./install.sh installs there instead of ~/.local
set -eu

PREFIX="${PREFIX:-$HOME/.local}"
BIN="$PREFIX/bin"
ICON="$PREFIX/share/icons/hicolor/512x512/apps"
APPS="$PREFIX/share/applications"

say() { printf '%s\n' "$*"; }

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$BIN/peepin" "$ICON/peepin.png" "$APPS/peepin.desktop"
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$APPS" >/dev/null 2>&1 || true
    fi
    say "peepin removed from $PREFIX"
    exit 0
fi

command -v cargo >/dev/null 2>&1 || {
    say "cargo not found. Install the Rust toolchain first: https://rustup.rs"
    exit 1
}

say "building peepin (a minute, the first time longer)…"
if ! cargo build --release --locked; then
    say "the lockfile was out of date; building without --locked"
    cargo build --release
fi

install -Dm755 target/release/peepin "$BIN/peepin"
install -Dm644 assets/peep.png "$ICON/peepin.png"
install -Dm644 packaging/peepin.desktop "$APPS/peepin.desktop"

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$APPS" >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t "$PREFIX/share/icons/hicolor" >/dev/null 2>&1 || true
fi

say "installed $BIN/peepin"

# What it will find on this machine, and what it will miss.
say ""
say "optional tools:"
for tool in mpv magick gm heif-convert djxl vips; do
    if command -v "$tool" >/dev/null 2>&1; then
        say "  found   $tool"
    fi
done
if ! command -v mpv >/dev/null 2>&1; then
    say "  missing mpv — pictures work, video will not play"
fi
if ! command -v magick >/dev/null 2>&1 && ! command -v gm >/dev/null 2>&1; then
    say "  missing ImageMagick (magick) — heic, jxl, psd, svg and camera raw will not open"
fi

case ":$PATH:" in
    *":$BIN:"*) ;;
    *) say ""; say "note: $BIN is not in your PATH" ;;
esac
