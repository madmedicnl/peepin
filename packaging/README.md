# Packaging

`PKGBUILD` builds **peepin-git** from this repository: the sources come in with
git, the crates with `cargo fetch`, and the package holds the binary, the menu
entry, the icon and the licence. `.SRCINFO` is generated from it and has to
travel with it — the AUR reads that file, not the PKGBUILD.

## AppImage

`appimage.sh` builds the release AppImage — one file, no install — into
`dist/`:

```sh
./packaging/appimage.sh              # build, then dist/peepin-<version>-x86_64.AppImage
./packaging/appimage.sh --no-build   # reuse the binary already in target/
```

The version comes from `Cargo.toml`, and the script proves the file runs
(`--appimage-extract-and-run`, then `--version`) before it says it is built.
Nothing is bundled beside the binary: X11, Wayland, the GL stack and GLIBC are
the host's, and mpv and ImageMagick stay optional tools peepin looks for.

## Build and install the package here

```sh
mkdir -p ~/aur/peepin-git
cp PKGBUILD .SRCINFO ~/aur/peepin-git/
cd ~/aur/peepin-git
makepkg -si
```

`makepkg -si` asks for your sudo password once, for `pacman -U`. The version is
filled in by `pkgver()` (`r<commits>.<hash>` of the checkout it just made), so
the one in the file is only a starting value.

## Publishing to the AUR

The account needs an SSH key registered on <https://aur.archlinux.org> (My
Account → SSH Public Key); the AUR answers `Permission denied (publickey)` until
it is there. Registration is closed at times, in which case this waits.

```sh
cd ~/aur/peepin-git
git init -b master                # only the first time
git add PKGBUILD .SRCINFO
git commit -m "Initial import: peepin-git"
git remote add origin ssh://aur@aur.archlinux.org/peepin-git.git
git push -u origin master
```

The first push creates the package; after that:

```sh
makepkg --printsrcinfo > .SRCINFO
git commit -am "Update"
git push
```

`peepin-git` follows the branch, so most updates need no push at all — only a
change to the PKGBUILD itself (a new dependency, say) does. A stable `peepin`
package would need a tag and a tarball instead of the git source.
