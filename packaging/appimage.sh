#!/bin/sh
# Build peepin-x86_64.AppImage from a release build.
#
#   ./packaging/appimage.sh            build dist/peepin-<version>-x86_64.AppImage
#   ./packaging/appimage.sh --no-build reuse the binary already in target/
#
# Nothing is bundled beside the binary: X11, Wayland, the GL stack and GLIBC
# have to be the host's, and AppImage's own exclude list keeps them out. mpv
# and ImageMagick are optional tools peepin looks for at run time, so they are
# not in here either — the AppImage is the program, not its tools.
set -eu

cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
[ -n "$version" ] || { echo "no version in Cargo.toml" >&2; exit 1; }
arch=x86_64
out="dist/peepin-$version-$arch.AppImage"

if [ "${1:-}" != "--no-build" ]; then
    cargo build --release --locked
fi
[ -x target/release/peepin ] || { echo "no release binary; build it first" >&2; exit 1; }

rm -rf dist/AppDir
mkdir -p dist/AppDir/usr/bin \
         dist/AppDir/usr/share/applications \
         dist/AppDir/usr/share/icons/hicolor/512x512/apps \
         dist/AppDir/usr/share/doc/peepin
install -m755 target/release/peepin dist/AppDir/usr/bin/peepin
install -m644 packaging/peepin.desktop dist/AppDir/usr/share/applications/peepin.desktop
install -m644 assets/peep.png dist/AppDir/usr/share/icons/hicolor/512x512/apps/peepin.png
install -m644 LICENSE dist/AppDir/usr/share/doc/peepin/LICENSE
# appimagetool wants the desktop entry and the icon at the AppDir root as well,
# and `.DirIcon` is what a file manager shows.
install -m644 packaging/peepin.desktop dist/AppDir/peepin.desktop
install -m644 assets/peep.png dist/AppDir/peepin.png
install -m644 assets/peep.png dist/AppDir/.DirIcon
# AppRun rather than a symlink to the binary: `exec` keeps the process id, so a
# desktop that started the AppImage still owns the window, and "$@" passes the
# command line through — `--screenshot`, `--list` and the rest work as they do
# on the binary.
cat > dist/AppDir/AppRun <<'EOF'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/peepin" "$@"
EOF
chmod +x dist/AppDir/AppRun

# appimagetool is itself an AppImage; `--appimage-extract-and-run` needs no
# FUSE. It is kept between builds so a release is one command.
tool="dist/appimagetool-$arch.AppImage"
if [ ! -x "$tool" ]; then
    curl -sSfL "https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-$arch.AppImage" -o "$tool"
    chmod +x "$tool"
fi
ARCH=$arch VERSION=$version APPIMAGE_EXTRACT_AND_RUN=1 "$tool" dist/AppDir "$out"

# A file that cannot be run is not a release asset, so prove it: the same
# extract-and-run path a user without FUSE takes.
APPIMAGE_EXTRACT_AND_RUN=1 "$out" --version

echo
echo "built $out"
sha256sum "$out"
