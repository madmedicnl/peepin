# peepin

A tiny image and video viewer with a terminal feel: no window background of its
own, monospace green text, and the picture scaled into whatever space the window
has. Built for flipping through Fallout's `.dds` textures as much as for photos.

## Install

```sh
cargo build --release
install -m755 target/release/peepin ~/.local/bin/peepin
```

## Open

```sh
peepin                      # empty, Ctrl+O to pick
peepin ~/Pictures/photo.jpg  # opens that file, ← → walk the folder
peepin ~/Pictures            # opens the first image in the folder
```

## Keys

| key | what |
|---|---|
| `←` `→` | previous / next file in the folder |
| mouse left / right button | previous / next (swappable, see below) |
| mouse wheel, `↑` `↓` | zoom in / out |
| middle-drag | pan a zoomed picture |
| `0` | fit to window again |
| `Ctrl`+`←` `→` `↑` `↓` | resize the window itself (works while floating) |
| `Ctrl+O` | file picker |
| `Ctrl+S` | save as: folder, name and format (png, jpg, webp, bmp, tiff) |
| `F1` | the key list |
| `F2` | settings panel (background, mouse buttons, colours) |
| `F3` | contact sheet of the folder, as polaroids |
| `F11` | batch rename the images in the folder |
| `b` | background: clear → blur → solid |
| `Ctrl+Q` | quit |

Videos (mp4, mkv, webm, avi, mov, …) are handed to **mpv** in its own window and
loop; images go through `image` (png, jpg, gif, bmp, tga, tiff, webp, qoi, hdr,
exr, ico, pnm) with `.dds` decoded through `image_dds` (BC1–BC7, Fallout's
textures included).

## Mouse buttons

`~/.config/peepin/peepin.conf`:

```ini
# prev or next — default is left = previous, right = next
mouse.left = prev
mouse.right = next
```

## Notes

- The window is transparent: the picture sits on whatever is behind it. Under a
  compositor that ignores alpha it will look black instead.
- Zoom is relative to "fit", so `zoom 100%` means the picture exactly fills the
  window; it re-fits when you resize.

## Background

Three modes, `b` cycles them, `~/.config/peepin/peepin.conf` sets the one it
starts in:

```ini
background = transparent   # the desktop shows through (default look)
background = blur          # a blurred, dimmed copy of the picture fills the window
background = solid         # the terminal's background colour
```

Blur is the picture itself, downscaled to 48 px wide, blurred and stretched over
the window — no compositor support needed, and it works the same under X11 and
Wayland.

## Colours

The menu, status line and picker take their colours from your terminal: kitty's
`~/.config/kitty/kitty.conf` is read, `include` lines followed (this machine uses
`themes/noctalia.conf`). Override any of them in `~/.config/peepin/peepin.conf`:

```ini
color.fg     = #e4dad1   # text
color.bg     = #1e2424   # picker and status line background
color.dim    = #6b5f5c   # borders, secondary text
color.accent = #7fb3ad   # selection
```

`peepin --palette` prints what it resolved.

## Settings (F2)

The F2 panel edits the same file: background mode, which way each mouse button
walks, and the four colours. `Ctrl+S` (or *save*) writes `peepin.conf` and applies
it at once; `Esc` closes, `F2` toggles.

## Saving (Ctrl+S)

Writes the picture at its own size — not as zoomed on screen — to the folder and
name in the panel, in png, jpg (quality 92, alpha composited on white), webp
(lossless), bmp or tiff. With a contact sheet open, Ctrl+S saves the sheet.

## Contact sheet (F3)

Every image in the folder as a polaroid: a white card, the picture kept in its own
proportions and centred, with the classic deeper bottom edge, laid out on a cream
board (at most 96 images). Ctrl+S saves it; F3 or a new file closes it.

## Batch rename (F11)

`{name}` is the old stem, `{n}` the number (zero-padded to the folder's count), and
`start` is where numbering begins; the extension is kept. The panel previews the
first twelve names, and *rename* renames the images in two passes, so a batch can
never collide with itself.

## Picker (Ctrl+O)

Two columns, like elio: folders on the left, images and videos on the right, the
path along the bottom edge in inverse video. It is drawn inside peepin's own
window (`--picker` opens it at start, which is how it was tested: `egui::Window`
does not appear under eframe's ui-based entry point).

| key | what |
|---|---|
| `↑` `↓` | move inside the focused column |
| `Tab`, `←`, `→` | switch column |
| `Enter`, `→` on a folder | open the folder or file under the cursor |
| `←` | one folder up |
| `Backspace` | one folder up |
| `Esc` | close |
