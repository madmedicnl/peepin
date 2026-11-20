//! peepin — a tiny image and video viewer with a terminal feel.
//!
//! The window has no background of its own: the picture sits on whatever is behind
//! it, and the menu, status line and picker take their colours from the terminal
//! (`~/.config/kitty/kitty.conf`, with `peepin.conf` able to override them).
//! Left/right (or the mouse's left/right button) walk the folder, the wheel or
//! up/down zoom, Ctrl+O opens the picker and Ctrl+Q quits. Videos are handed to
//! mpv, which draws inside this window; everything else goes through `image`,
//! with `.dds` (Fallout's textures) through `image_dds`.
//!
//! peepin is free software: you can redistribute it and/or modify it under the
//! terms of the GNU General Public License as published by the Free Software
//! Foundation, either version 3 of the License, or (at your option) any later
//! version. It comes without any warranty; see the LICENSE file, or
//! <https://www.gnu.org/licenses/>.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{
    Align2, Color32, FontFamily, FontId, Pos2, Rect, Stroke, StrokeKind, Vec2, ViewportCommand,
};

/// Everything `image` can open out of the box.
const IMAGE_EXTS: &[&str] = &[
    // what `image` reads itself
    "png", "jpg", "jpeg", "gif", "bmp", "tga", "tif", "tiff", "webp", "qoi", "hdr", "exr",
    "dds", "ico", "pnm", "ff", "farbfeld", "avif",
    // and what the fallback decoders (below) cover
    "heic", "heif", "jxl", "jp2", "j2k", "psd", "pcx", "xcf", "wmf", "emf", "svg",
    "cr2", "cr3", "nef", "arw", "dng", "orf", "raf", "rw2", "pef", "srw", "3fr", "mrw",
    "jxr", "wdp", "hdp",
];

/// Handed to mpv.
const VIDEO_EXTS: &[&str] = &[
    "mp4", "mkv", "webm", "avi", "mov", "m4v", "wmv", "flv", "mpg", "mpeg", "ts", "ogv",
];

/// Subtitle files peepin will offer for the video on screen.
const SUBTITLE_EXTS: &[&str] = &["srt", "vtt", "ass", "ssa", "sub", "idx", "sup"];

/// Height of the control bar under a video, in points.
const VIDEO_BAR: f32 = 34.0;

/// The most mpv will play at; the bar shows the same range.
const MAX_VOLUME: f64 = 130.0;

fn has_ext(path: &Path, set: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| set.iter().any(|s| s.eq_ignore_ascii_case(e)))
        .unwrap_or(false)
}

fn is_image(path: &Path) -> bool {
    has_ext(path, IMAGE_EXTS)
}

fn is_video(path: &Path) -> bool {
    has_ext(path, VIDEO_EXTS)
}

/// What sits behind the picture: nothing (the desktop shows through), a blurred
/// copy of the picture itself, or the terminal's background colour.
#[derive(Clone, Copy, PartialEq)]
enum Background {
    Transparent,
    Blur,
    Solid,
}

/// Which way a mouse button walks the folder.
#[derive(Clone, Copy, PartialEq)]
enum Step {
    Previous,
    Next,
}

/// The terminal's colours.
#[derive(Clone, Copy)]
struct Palette {
    /// Normal text: the terminal's foreground.
    fg: Color32,
    /// Panels and the status line: the terminal's background, translucent.
    bg: Color32,
    /// Borders and secondary text: colour8 (bright black).
    dim: Color32,
    /// Selection and highlights: colour4 (blue) or colour6 (cyan).
    accent: Color32,
}

impl Palette {
    /// Read the terminal's palette: kitty first (that is what this machine runs),
    /// then `peepin.conf` on top, then the classic green-on-black if neither says
    /// anything.
    fn load() -> Self {
        let mut palette = Palette {
            fg: Color32::from_rgb(0x46, 0xff, 0x7a),
            bg: Color32::from_rgb(0x00, 0x00, 0x00),
            dim: Color32::from_rgb(0x2a, 0x99, 0x4a),
            accent: Color32::from_rgb(0x46, 0xff, 0x7a),
        };

        let home = std::env::var("HOME").unwrap_or_default();
        let kitty = PathBuf::from(&home).join(".config/kitty/kitty.conf");
        let mut color4: Option<Color32> = None;
        let mut color6: Option<Color32> = None;
        if let Ok(text) = read_with_includes(&kitty, 0) {
            for line in text.lines() {
                let line = line.trim();
                if line.starts_with('#') {
                    continue;
                }
                let Some((key, value)) = line.split_once(char::is_whitespace) else {
                    continue;
                };
                let value = value.trim();
                match key {
                    "foreground" => palette.fg = parse_color(value).unwrap_or(palette.fg),
                    "background" => palette.bg = parse_color(value).unwrap_or(palette.bg),
                    "color8" => palette.dim = parse_color(value).unwrap_or(palette.dim),
                    "color4" => color4 = parse_color(value),
                    "color6" => color6 = parse_color(value),
                    _ => {}
                }
            }
        }
        palette.accent = color6.or(color4).unwrap_or(palette.fg);

        // peepin.conf wins: colour.fg / bg / dim / accent, as #rrggbb.
        if let Ok(text) = std::fs::read_to_string(config_path()) {
            for line in text.lines() {
                let line = line.trim();
                if line.starts_with('#') || line.is_empty() {
                    continue;
                }
                let Some((key, value)) = line.split_once('=') else {
                    continue;
                };
                let Some(colour) = parse_color(value.trim()) else {
                    continue;
                };
                match key.trim() {
                    "color.fg" => palette.fg = colour,
                    "color.bg" => palette.bg = colour,
                    "color.dim" => palette.dim = colour,
                    "color.accent" => palette.accent = colour,
                    _ => {}
                }
            }
        }
        palette
    }
}

/// A kitty config plus everything it pulls in with `include`.
fn read_with_includes(path: &Path, depth: usize) -> Result<String, std::io::Error> {
    let mut text = std::fs::read_to_string(path)?;
    if depth > 4 {
        return Ok(text);
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut extra = String::new();
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("include ") else {
            continue;
        };
        let target = rest.trim().trim_matches('"');
        let target = if let Some(home) = target.strip_prefix("~/") {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(home)
        } else if target.starts_with('/') {
            PathBuf::from(target)
        } else {
            dir.join(target)
        };
        if let Ok(more) = read_with_includes(&target, depth + 1) {
            extra.push_str(&more);
            extra.push('\n');
        }
    }
    text.push('\n');
    text.push_str(&extra);
    Ok(text)
}

fn parse_color(text: &str) -> Option<Color32> {
    let hex = text.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color32::from_rgb(r, g, b))
}

/// One pane of the picker: what it lists and where the cursor sits.
struct Pane {
    entries: Vec<(String, PathBuf)>,
    selected: usize,
}

struct Pick {
    path: PathBuf,
    folders: Pane,
    files: Pane,
    /// False = files have the focus, true = folders.
    on_folders: bool,
    error: String,
    /// `v` in the picker: show a thumbnail beside every name in the right column.
    preview: bool,
    /// Thumbnails already made, so scrolling does not decode the same picture twice.
    thumbs: std::collections::HashMap<PathBuf, Option<egui::TextureHandle>>,
    /// Set when the arrows moved the selection, so the list can bring it back on
    /// screen in the next frame.
    scroll: bool,
}

/// The F1 panel: the handful of settings that are worth changing without an
/// editor, written back to `peepin.conf`.
struct Settings {
    background: Background,
    left: Step,
    right: Step,
    fg: String,
    bg: String,
    dim: String,
    accent: String,
    note: String,
}

impl Settings {
    fn from(app: &Peepin) -> Self {
        let hex = |c: Color32| format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b());
        Self {
            background: app.background,
            left: app.left_button,
            right: app.right_button,
            fg: hex(app.palette.fg),
            bg: hex(app.palette.bg),
            dim: hex(app.palette.dim),
            accent: hex(app.palette.accent),
            note: String::new(),
        }
    }
}

/// Formats the save dialog can write.
#[derive(Clone, Copy, PartialEq)]
enum Format {
    Png,
    Jpeg,
    Webp,
    Bmp,
    Tiff,
}

impl Format {
    fn ext(self) -> &'static str {
        match self {
            Format::Png => "png",
            Format::Jpeg => "jpg",
            Format::Webp => "webp",
            Format::Bmp => "bmp",
            Format::Tiff => "tiff",
        }
    }
}

/// Ctrl+S: where to write the picture, and in what format.
struct SaveDialog {
    dir: String,
    name: String,
    format: Format,
    note: String,
}

/// F3: the contact sheet, already rendered as a picture so Ctrl+S can save it.
struct Sheet {
    texture: egui::TextureHandle,
    image: egui::ColorImage,
    count: usize,
}

/// F11: batch rename of the images in the folder.
struct RenameDialog {
    pattern: String,
    /// Free text on purpose: it is typed in, and parsed when the rename runs.
    start: String,
    note: String,
}

/// The panels that sit over the picture.
enum Overlay {
    Help,
    Settings(Settings),
    Save(SaveDialog),
    Rename(RenameDialog),
}

/// One subtitle track of the file: an embedded one, or one peepin loaded.
#[derive(Clone, Default)]
struct SubTrack {
    id: i64,
    lang: String,
    title: String,
}

/// What mpv last told us about the video on screen.
#[derive(Default, Clone)]
struct VideoState {
    pos: f64,
    duration: f64,
    volume: f64,
    paused: bool,
    muted: bool,
    /// The selected subtitle track, when one is chosen.
    sid: Option<i64>,
    /// Whether mpv would show it.
    subs: bool,
    /// The subtitle tracks the file has, read once when playback starts.
    tracks: Vec<SubTrack>,
    /// Set when a track was added, so the list is read again.
    tracks_stale: bool,
}

/// A child window of our own that mpv is told to paint into with `--wid`.
///
/// `--wid` on our top-level window would let mpv cover the whole client area, so
/// the control bar under the video could neither be seen nor clicked. Handing mpv
/// a window that stops above the bar leaves the bar to peepin.
struct VideoWindow {
    xlib: x11_dl::xlib::Xlib,
    display: *mut x11_dl::xlib::Display,
    window: u64,
    geometry: (i32, i32, u32, u32),
    mapped: bool,
}

impl VideoWindow {
    fn new(parent: u64, geometry: (i32, i32, u32, u32)) -> Option<Self> {
        let xlib = x11_dl::xlib::Xlib::open().ok()?;
        let display = unsafe { (xlib.XOpenDisplay)(std::ptr::null()) };
        if display.is_null() {
            return None;
        }
        let (x, y, width, height) = geometry;
        let window = unsafe {
            (xlib.XCreateSimpleWindow)(
                display,
                parent as x11_dl::xlib::Window,
                x,
                y,
                width,
                height,
                0,
                0,
                0,
            )
        };
        if window == 0 {
            unsafe { (xlib.XCloseDisplay)(display) };
            return None;
        }
        unsafe {
            (xlib.XMapWindow)(display, window);
            (xlib.XFlush)(display);
        }
        Some(Self { xlib, display, window, geometry, mapped: true })
    }

    /// Hide the video while a panel or the picker wants the window.
    fn show(&mut self, show: bool) {
        if self.mapped == show {
            return;
        }
        unsafe {
            if show {
                (self.xlib.XMapWindow)(self.display, self.window);
            } else {
                (self.xlib.XUnmapWindow)(self.display, self.window);
            }
            (self.xlib.XFlush)(self.display);
        }
        self.mapped = show;
    }

    fn resize(&mut self, geometry: (i32, i32, u32, u32)) {
        if self.geometry == geometry {
            return;
        }
        let (x, y, width, height) = geometry;
        unsafe {
            (self.xlib.XMoveResizeWindow)(self.display, self.window, x, y, width, height);
            (self.xlib.XFlush)(self.display);
        }
        self.geometry = geometry;
    }
}

impl Drop for VideoWindow {
    fn drop(&mut self) {
        unsafe {
            (self.xlib.XDestroyWindow)(self.display, self.window);
            (self.xlib.XFlush)(self.display);
            (self.xlib.XCloseDisplay)(self.display);
        }
    }
}

/// Where the video window goes: the client area, less the control bar.
fn video_geometry(ctx: &egui::Context) -> Option<(i32, i32, u32, u32)> {
    let rect = ctx.content_rect();
    let ppp = ctx.pixels_per_point();
    let width = (rect.width() * ppp).round().max(1.0) as u32;
    let height = ((rect.height() - VIDEO_BAR) * ppp).round().max(1.0) as u32;
    Some((0, 0, width, height))
}

/// `folder/name`, or `folder/stem-2.png` and on, so a second screenshot in the
/// same second does not overwrite the first.
fn free_name(folder: &Path, name: &str) -> PathBuf {
    let path = folder.join(name);
    if !path.exists() {
        return path;
    }
    let stem = Path::new(name).file_stem().map(|s| s.to_owned()).unwrap_or_default();
    let extension = Path::new(name).extension().map(|e| e.to_owned());
    for n in 2..1000 {
        let candidate = match &extension {
            Some(ext) => folder.join(format!("{}-{n}.{}", stem.to_string_lossy(), ext.to_string_lossy())),
            None => folder.join(format!("{}-{n}", stem.to_string_lossy())),
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    path
}

/// `20261120-143512`, in local time: the name a screenshot gets.
fn stamp() -> String {
    // Local time, because that is what a person looks for in a folder listing.
    let mut now: libc::time_t = 0;
    let mut parts: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::time(&mut now);
        libc::localtime_r(&now, &mut parts);
    }
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        parts.tm_year + 1900,
        parts.tm_mon + 1,
        parts.tm_mday,
        parts.tm_hour,
        parts.tm_min,
        parts.tm_sec
    )
}

/// The window as it is on screen, as RGBA. `XGetImage` on our own window brings
/// its children with it, so a video mpv draws inside is in the picture too.
fn capture_window(window: u64) -> Option<image::RgbaImage> {
    let xlib = x11_dl::xlib::Xlib::open().ok()?;
    unsafe {
        let display = (xlib.XOpenDisplay)(std::ptr::null());
        if display.is_null() {
            return None;
        }
        let taken = (|| {
            (xlib.XSync)(display, 0);
            let mut attributes: x11_dl::xlib::XWindowAttributes = std::mem::zeroed();
            if (xlib.XGetWindowAttributes)(display, window, &mut attributes) == 0 {
                return None;
            }
            let width = attributes.width.max(1) as u32;
            let height = attributes.height.max(1) as u32;
            let image = (xlib.XGetImage)(
                display,
                window,
                0,
                0,
                width,
                height,
                !0,
                x11_dl::xlib::ZPixmap,
            );
            if image.is_null() {
                return None;
            }
            let pixels = image_pixels(&*image, attributes.depth, width, height);
            (xlib.XDestroyImage)(image);
            pixels
        })();
        (xlib.XCloseDisplay)(display);
        taken
    }
}

/// mpv's IPC socket for this run.
fn mpv_socket_path() -> PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    base.join(format!("peepin-mpv-{}.sock", std::process::id()))
}

/// Sockets left behind by a peepin that was killed rather than closed: their
/// process is gone, so the file can go too.
fn clean_old_mpv_sockets() {
    let base = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    let Ok(entries) = std::fs::read_dir(&base) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix("peepin-mpv-") else {
            continue;
        };
        let Some(pid) = rest.strip_suffix(".sock") else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        if Path::new(&format!("/proc/{pid}")).exists() {
            continue;
        }
        let _ = std::fs::remove_file(entry.path());
    }
}

/// One command to mpv, as a JSON array, and its reply.
fn mpv_send(socket: &Path, command: &[&str]) -> Option<String> {
    let mut stream = UnixStream::connect(socket).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(400)));
    let mut line = String::from("{\"command\":[");
    for (i, arg) in command.iter().enumerate() {
        if i > 0 {
            line.push(',');
        }
        line.push('"');
        for c in arg.chars() {
            match c {
                '"' | '\\' => {
                    line.push('\\');
                    line.push(c);
                }
                '\n' => line.push_str("\\n"),
                _ => line.push(c),
            }
        }
        line.push('"');
    }
    line.push_str("]}\n");
    stream.write_all(line.as_bytes()).ok()?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).ok()?;
    Some(reply)
}

/// `{"data":42,"error":"success"}` -> `42`; `null` and arrays give nothing.
fn data_field(reply: &str) -> Option<String> {
    let at = reply.find("\"data\":")?;
    let rest = reply[at + 7..].trim_start();
    if rest.starts_with('[') || rest.starts_with('{') {
        return None;
    }
    if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"')?;
        return Some(stripped[..end].to_string());
    }
    let end = rest.find(['}', ',']).unwrap_or(rest.len());
    let value = rest[..end].trim();
    if value.is_empty() || value == "null" {
        None
    } else {
        Some(value.to_string())
    }
}

/// The pixels of an `XImage` as RGBA. X hands 32-bit pixels as BGRX (or BGRA
/// when the window has an alpha channel), four bytes each, with a stride that
/// need not be the width.
fn image_pixels(
    photo: &x11_dl::xlib::XImage,
    depth: i32,
    width: u32,
    height: u32,
) -> Option<image::RgbaImage> {
    let stride = photo.bytes_per_line as usize;
        let len = stride.checked_mul(height as usize)?;
        if photo.data.is_null() || len == 0 {
            return None;
        }
        let data = unsafe { std::slice::from_raw_parts(photo.data as *const u8, len) };
        let mut out = image::RgbaImage::new(width, height);
        for y in 0..height as usize {
            for x in 0..width as usize {
                let at = y * stride + x * 4;
                if at + 3 >= data.len() {
                    continue;
                }
                let alpha = if depth > 24 { data[at + 3] } else { 255 };
                out.put_pixel(
                    x as u32,
                    y as u32,
                    image::Rgba([data[at + 2], data[at + 1], data[at], alpha]),
                );
            }
        }
    Some(out)
}

fn mpv_get(socket: &Path, property: &str) -> Option<String> {
    let reply = mpv_send(socket, &["get_property", property])?;
    data_field(&reply)
}

fn mpv_number(socket: &Path, property: &str) -> Option<f64> {
    mpv_get(socket, property).and_then(|v| v.parse().ok())
}

impl VideoState {
    /// Ask mpv where it is and how loud; six quick questions on a local socket.
    fn update(&mut self, socket: &Path) {
        if let Some(v) = mpv_number(socket, "time-pos") {
            self.pos = v;
        }
        if let Some(v) = mpv_number(socket, "duration") {
            self.duration = v;
        }
        if let Some(v) = mpv_number(socket, "volume") {
            self.volume = v;
        }
        if let Some(v) = mpv_get(socket, "pause") {
            self.paused = v == "true";
        }
        if let Some(v) = mpv_get(socket, "mute") {
            self.muted = v == "true";
        }
        if let Some(v) = mpv_get(socket, "sub-visibility") {
            self.subs = v == "true";
        }
        // `sid` is a number when a subtitle is selected, and `false` or `no`
        // when there is none.
        if let Some(v) = mpv_get(socket, "sid") {
            self.sid = v.parse::<f64>().ok().map(|n| n as i64);
        }
        if self.tracks.is_empty() || self.tracks_stale {
            self.read_tracks(socket);
        }
    }

    /// The subtitle tracks of the file, as mpv lists them.
    fn read_tracks(&mut self, socket: &Path) {
        let Some(count) = mpv_number(socket, "track-list/count") else {
            return;
        };
        let mut tracks = Vec::new();
        for i in 0..count as u32 {
            let Some(kind) = mpv_get(socket, &format!("track-list/{i}/type")) else {
                continue;
            };
            if kind != "sub" {
                continue;
            }
            let Some(id) = mpv_number(socket, &format!("track-list/{i}/id")) else {
                continue;
            };
            tracks.push(SubTrack {
                id: id as i64,
                lang: mpv_get(socket, &format!("track-list/{i}/lang")).unwrap_or_default(),
                title: mpv_get(socket, &format!("track-list/{i}/title")).unwrap_or_default(),
            });
        }
        self.tracks = tracks;
        self.tracks_stale = false;
    }

    /// The track that is showing, if any.
    fn selected(&self) -> Option<&SubTrack> {
        let sid = self.sid?;
        self.tracks.iter().find(|t| t.id == sid)
    }

    /// What the subtitle chip says: the language of the track, or `sub`.
    fn label(&self) -> String {
        match self.selected() {
            Some(track) if !track.lang.is_empty() => track.lang.to_uppercase(),
            Some(track) if !track.title.is_empty() => shorten(&track.title, 4).to_uppercase(),
            Some(track) => format!("s{}", track.id),
            None => "sub".to_string(),
        }
    }
}

/// `83.4` -> `1:23`, and hours when the video has them.
fn clock(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "--:--".to_string();
    }
    let total = seconds.round() as u64;
    let (hours, minutes, secs) = (total / 3600, (total % 3600) / 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes}:{secs:02}")
    }
}

struct Peepin {
    files: Vec<PathBuf>,
    index: usize,
    texture: Option<egui::TextureHandle>,
    image_size: Vec2,
    zoom: f32,
    pan: Vec2,
    video: Option<Child>,
    status: String,
    picker: Option<Pick>,
    /// Whether the picker shows thumbnails; remembered for the next time it opens.
    previews: bool,
    left_button: Step,
    right_button: Step,
    drag: Option<(Pos2, Vec2)>,
    /// Left-button drag: start, pan when it began, and whether it moved far
    /// enough to count as a pan rather than a click.
    left_drag: Option<(Pos2, Vec2, bool)>,
    /// X11 window id, when the window could be created through XWayland. mpv
    /// draws its video into that window with `--wid`.
    window_id: Option<u64>,
    /// True while mpv draws into our window (through the child window below).
    embedded: bool,
    /// Our own child window that mpv paints into, so the bar underneath it stays
    /// visible and clickable.
    video_window: Option<VideoWindow>,
    /// mpv's IPC socket: the way peepin asks for volume, subtitles and seeking.
    mpv_socket: Option<PathBuf>,
    /// What mpv last told us about the video.
    mpv: VideoState,
    /// When mpv was last asked, so it is polled a few times a second.
    mpv_polled: Option<Instant>,
    /// When a seek was last sent, so dragging the bar does not flood mpv.
    seek_sent: Option<Instant>,
    /// `--seek=<seconds>`: jump there once the video is running.
    seek_at_start: Option<f64>,
    /// `--sub-load`: load the subtitle beside the video once it is running.
    sub_load_at_start: bool,
    /// `--sub-cycle=N`: walk through the subtitle tracks N times, as the chip
    /// would, with a moment between each.
    sub_cycle_at_start: usize,
    /// When the last of those steps was taken.
    sub_cycle_at: Option<Instant>,
    /// `--sub-pick=N`: choose subtitle track N once, as the number keys do.
    sub_pick_at_start: Option<usize>,
    /// `--sub-menu`: open the track list at start, the way a right click does.
    sub_menu_at_start: bool,
    /// The list of subtitle tracks, open above the bar: right-click on the chip.
    sub_menu: bool,
    /// A line shown instead of the file name for a moment, e.g. which track was
    /// just picked.
    video_note: Option<(String, Instant)>,
    /// A line shown in the status bar for a moment, e.g. where a screenshot went.
    note: Option<(String, Instant)>,
    /// `--screenshot=<file>`: take one once the first frame is on screen.
    screenshot_at: Option<(Instant, PathBuf)>,
    /// A video waiting for the window to be mapped.
    pending_video: Option<PathBuf>,
    palette: Palette,
    background: Background,
    /// A small, blurred copy of the current picture, stretched over the window in
    /// `Background::Blur`.
    blur: Option<egui::TextureHandle>,
    /// The picture as decoded, so Ctrl+S writes it at its own size.
    source: Option<egui::ColorImage>,
    /// The panel that is open: help, settings, save or rename.
    overlay: Option<Overlay>,
    /// The contact sheet built by F3, until the next file is shown.
    sheet: Option<Sheet>,
    /// A contact sheet being built in the background.
    sheet_job: Option<std::sync::mpsc::Receiver<(egui::ColorImage, usize)>>,
}


impl Peepin {
    /// Print the palette and exit; used by `cargo run --example palette`.
    pub fn palette_for_debug() {
        let p = Palette::load();
        println!(
            "fg={:02x}{:02x}{:02x} bg={:02x}{:02x}{:02x} dim={:02x}{:02x}{:02x} accent={:02x}{:02x}{:02x}",
            p.fg.r(), p.fg.g(), p.fg.b(),
            p.bg.r(), p.bg.g(), p.bg.b(),
            p.dim.r(), p.dim.g(), p.dim.b(),
            p.accent.r(), p.accent.g(), p.accent.b()
        );
    }

    fn new(
        cc: &eframe::CreationContext<'_>,
        arg: Option<PathBuf>,
        open_picker_now: bool,
        open_settings_now: bool,
        open_sheet_now: bool,
        previews_at_start: bool,
    ) -> Self {
        let palette = Palette::load();

        cc.egui_ctx.all_styles_mut(|style| {
            for (_, font) in style.text_styles.iter_mut() {
                font.family = FontFamily::Monospace;
            }
            style.spacing.item_spacing = Vec2::new(4.0, 1.0);
            style.visuals.override_text_color = Some(palette.fg);
            style.visuals.window_fill = palette.bg;
            style.visuals.panel_fill = Color32::TRANSPARENT;
            style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, palette.dim);
        });

        let (files, index) = match arg {
            Some(path) => scan(&path),
            None => (Vec::new(), 0),
        };

        let mut app = Self {
            files,
            index,
            texture: None,
            image_size: Vec2::ZERO,
            zoom: 1.0,
            pan: Vec2::ZERO,
            video: None,
            status: String::new(),
            picker: None,
            previews: previews_at_start,
            left_button: Step::Previous,
            right_button: Step::Next,
            drag: None,
            left_drag: None,
            window_id: x11_window_id(cc),
            embedded: false,
            video_window: None,
            mpv_socket: None,
            mpv: VideoState::default(),
            mpv_polled: None,
            seek_sent: None,
            seek_at_start: None,
            sub_load_at_start: false,
            sub_cycle_at_start: 0,
            sub_cycle_at: None,
            sub_pick_at_start: None,
            sub_menu_at_start: false,
            sub_menu: false,
            video_note: None,
            note: None,
            screenshot_at: None,
            pending_video: None,
            palette,
            background: Background::Transparent,
            blur: None,
            source: None,
            overlay: None,
            sheet: None,
            sheet_job: None,
        };
        if let (Some(window), Some(icon)) = (app.window_id, window_icon()) {
            set_window_icon(window, &icon);
        }
        app.read_config();
        app.load(&cc.egui_ctx);
        if open_picker_now {
            app.open_picker();
            app.previews = previews_at_start;
        }
        if open_settings_now {
            app.open_settings();
        }
        if open_sheet_now {
            app.toggle_sheet(&cc.egui_ctx);
        }
        app
    }

    fn apply_palette(&mut self, ctx: &egui::Context) {
        let palette = self.palette;
        ctx.all_styles_mut(|style| {
            style.visuals.override_text_color = Some(palette.fg);
            style.visuals.window_fill = palette.bg;
            style.visuals.panel_fill = Color32::TRANSPARENT;
            style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, palette.dim);
        });
    }

    fn open_settings(&mut self) {
        self.overlay = Some(Overlay::Settings(Settings::from(self)));
    }

    fn open_save(&mut self) {
        let dir = self
            .files
            .get(self.index)
            .and_then(|p| p.parent())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default().to_string_lossy().into_owned());
        let stem = if self.sheet.is_some() {
            "contactsheet".to_string()
        } else {
            self.files
                .get(self.index)
                .and_then(|p| p.file_stem())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "peepin".to_string())
        };
        self.overlay = Some(Overlay::Save(SaveDialog {
            dir,
            name: stem,
            format: Format::Png,
            note: String::new(),
        }));
    }

    fn toggle_sheet(&mut self, ctx: &egui::Context) {
        if self.sheet.is_some() {
            self.sheet = None;
            return;
        }
        if self.sheet_job.is_some() {
            return;
        }
        self.stop_video();
        let files = self.files.clone();
        if files.is_empty() {
            return;
        }
        // Decoding a folder of pictures takes a moment, so it happens on its own
        // thread and the window says what it is doing.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Some((image, count)) = build_sheet(&files) {
                let _ = tx.send((image, count));
            }
        });
        self.sheet_job = Some(rx);
        ctx.request_repaint();
    }

    /// Re-read the folder after its files were renamed, keeping the picture the
    /// user was looking at when its new name is known.
    fn reload_dir(&mut self, ctx: &egui::Context, preferred: Option<String>) {
        let dir = self
            .files
            .get(self.index)
            .and_then(|p| p.parent())
            .or_else(|| self.files.first().and_then(|p| p.parent()))
            .map(|p| p.to_path_buf());
        let Some(dir) = dir else {
            return;
        };
        let files = media_in(&dir);
        if files.is_empty() {
            self.files.clear();
            self.index = 0;
            self.texture = None;
            self.source = None;
            return;
        }
        let index = preferred
            .and_then(|name| {
                files.iter().position(|p| {
                    p.file_name()
                        .map(|n| n.to_string_lossy() == name)
                        .unwrap_or(false)
                })
            })
            .unwrap_or_else(|| self.index.min(files.len() - 1));
        self.files = files;
        self.index = index;
        self.load(ctx);
    }

    fn open_rename(&mut self) {
        let count = self.files.len();
        self.overlay = Some(Overlay::Rename(RenameDialog {
            pattern: "{name}-{n}".to_string(),
            start: "1".to_string(),
            note: if count == 0 {
                "no images or videos in this folder".to_string()
            } else {
                format!("{count} file(s) in this folder")
            },
        }));
    }

    fn save_settings(&mut self, ctx: &egui::Context) {
        let Some(Overlay::Settings(settings)) = self.overlay.as_ref() else {
            return;
        };
        let text = format!(
            "# peepin — written by the settings panel (F2)\n\
             background = {}\n\
             mouse.left = {}\n\
             mouse.right = {}\n\
             color.fg = {}\n\
             color.bg = {}\n\
             color.dim = {}\n\
             color.accent = {}\n",
            match settings.background {
                Background::Transparent => "transparent",
                Background::Blur => "blur",
                Background::Solid => "solid",
            },
            match settings.left {
                Step::Previous => "prev",
                Step::Next => "next",
            },
            match settings.right {
                Step::Previous => "prev",
                Step::Next => "next",
            },
            settings.fg,
            settings.bg,
            settings.dim,
            settings.accent
        );
        let path = config_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let note = match std::fs::write(&path, text) {
            Ok(()) => {
                self.background = settings.background;
                self.left_button = settings.left;
                self.right_button = settings.right;
                self.palette = Palette::load();
                self.apply_palette(ctx);
                "saved".to_string()
            }
            Err(err) => format!("could not write {}: {err}", path.display()),
        };
        if let Some(Overlay::Settings(settings)) = self.overlay.as_mut() {
            settings.note = note;
        }
    }

    /// Keys that belong to a panel rather than to the picture.
    fn overlay_keys(&mut self, ctx: &egui::Context) {
        let (escape, f1, f2, f11, ctrl_s) = ctx.input(|i| {
            let ctrl = i.modifiers.command || i.modifiers.ctrl;
            (
                i.key_pressed(egui::Key::Escape),
                i.key_pressed(egui::Key::F1),
                i.key_pressed(egui::Key::F2),
                i.key_pressed(egui::Key::F11),
                ctrl && i.key_pressed(egui::Key::S),
            )
        });
        if escape {
            self.overlay = None;
            return;
        }
        if f1 {
            self.overlay = match self.overlay {
                Some(Overlay::Help) => None,
                _ => Some(Overlay::Help),
            };
            return;
        }
        if f2 {
            self.overlay = match self.overlay {
                Some(Overlay::Settings(_)) => None,
                _ => Some(Overlay::Settings(Settings::from(self))),
            };
            return;
        }
        if f11 {
            self.overlay = match self.overlay {
                Some(Overlay::Rename(_)) => None,
                _ => Some(Overlay::Rename(RenameDialog {
                    pattern: "{name}-{n}".to_string(),
                    start: "1".to_string(),
                    note: String::new(),
                })),
            };
            return;
        }
        if ctrl_s {
            match self.overlay {
                Some(Overlay::Settings(_)) => self.save_settings(ctx),
                Some(Overlay::Save(_)) => self.save_picture(ctx),
                _ => {}
            }
        }
    }

    /// Write the picture (or the contact sheet) where the save panel says.
    fn save_picture(&mut self, ctx: &egui::Context) {
        let Some(Overlay::Save(dialog)) = self.overlay.as_ref() else {
            return;
        };
        let dir = PathBuf::from(dialog.dir.trim());
        let name = dialog.name.trim().to_string();
        let format = dialog.format;
        if name.is_empty() {
            self.set_save_note("give the file a name");
            return;
        }
        let source = match self.sheet.as_ref() {
            Some(sheet) => sheet.image.clone(),
            None => match self.source.clone() {
                Some(image) => image,
                None => {
                    self.set_save_note("nothing loaded to save");
                    return;
                }
            },
        };
        if let Err(err) = std::fs::create_dir_all(&dir) {
            self.set_save_note(&format!("could not create {}: {err}", dir.display()));
            return;
        }
        let path = dir.join(format!("{name}.{}", format.ext()));
        match write_image(&source, &path, format) {
            Ok(()) => {
                self.set_save_note(&format!("saved {}", path.display()));
                ctx.request_repaint();
            }
            Err(err) => {
                self.set_save_note(&err);
                ctx.request_repaint();
            }
        }
    }

    fn set_save_note(&mut self, note: &str) {
        if let Some(Overlay::Save(dialog)) = self.overlay.as_mut() {
            dialog.note = note.to_string();
        }
    }

    fn draw_overlay(&mut self, ui: &mut egui::Ui) {
        let palette = self.palette;
        let screen = ui.max_rect();
        let size = Vec2::new(
            screen.width().min(680.0),
            screen.height().min(520.0),
        );
        let rect = Rect::from_min_size(Pos2::new(screen.left() + 12.0, screen.top() + 12.0), size);
        let mut panel = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );
        panel
            .painter()
            .rect_filled(rect, 2.0, palette.bg.gamma_multiply(0.96));
        panel
            .painter()
            .rect_stroke(rect, 2.0, Stroke::new(1.0, palette.dim), StrokeKind::Inside);

        let mut close = false;
        let mut save_now = false;

        egui::Frame::new().inner_margin(8.0).show(&mut panel, |ui| match self.overlay.as_mut() {
            Some(Overlay::Help) => {
                ui.label(egui::RichText::new("keys").color(palette.accent));
                ui.separator();
                for (key, what) in KEYS {
                    ui.horizontal(|ui| {
                        ui.add_sized(
                            [190.0, 16.0],
                            egui::Label::new(
                                egui::RichText::new(*key).color(palette.accent).monospace(),
                            ),
                        );
                        ui.label(*what);
                    });
                }
                ui.separator();
                ui.label(egui::RichText::new("F1 closes this  ·  F2 settings").color(palette.dim));
            }
            Some(Overlay::Settings(settings)) => {
                ui.label(egui::RichText::new("settings").color(palette.accent));
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("background");
                    for (label, mode) in [
                        ("clear", Background::Transparent),
                        ("blur", Background::Blur),
                        ("solid", Background::Solid),
                    ] {
                        let picked = settings.background == mode;
                        if button(ui, label, picked, palette) {
                            settings.background = mode;
                        }
                    }
                });
                for (label, slot) in [("mouse left", 0), ("mouse right", 1)] {
                    ui.horizontal(|ui| {
                        ui.label(label);
                        for (text, step) in [("prev", Step::Previous), ("next", Step::Next)] {
                            let current = if slot == 0 { settings.left } else { settings.right };
                            if button(ui, text, current == step, palette) {
                                if slot == 0 {
                                    settings.left = step;
                                } else {
                                    settings.right = step;
                                }
                            }
                        }
                    });
                }
                ui.separator();
                for (label, value) in [
                    ("text", &mut settings.fg),
                    ("panel", &mut settings.bg),
                    ("dim", &mut settings.dim),
                    ("accent", &mut settings.accent),
                ] {
                    ui.horizontal(|ui| {
                        ui.label(label);
                        ui.add(
                            egui::TextEdit::singleline(value)
                                .font(FontId::monospace(13.0))
                                .desired_width(120.0),
                        );
                    });
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("save  (ctrl+s)").clicked() {
                        save_now = true;
                    }
                    if ui.button("close  (esc)").clicked() {
                        close = true;
                    }
                    if !settings.note.is_empty() {
                        ui.label(egui::RichText::new(&settings.note).color(palette.dim));
                    }
                });
                ui.label(
                    egui::RichText::new("colours as #rrggbb; the terminal's own are read at start")
                        .color(palette.dim),
                );
            }
            Some(Overlay::Save(dialog)) => {
                ui.label(egui::RichText::new("save as").color(palette.accent));
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("folder");
                    ui.add(
                        egui::TextEdit::singleline(&mut dialog.dir)
                            .font(FontId::monospace(13.0))
                            .desired_width(f32::INFINITY),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("name  ");
                    ui.add(
                        egui::TextEdit::singleline(&mut dialog.name)
                            .font(FontId::monospace(13.0))
                            .desired_width(220.0),
                    );
                    ui.label(egui::RichText::new(format!(".{}", dialog.format.ext())).color(palette.dim));
                });
                ui.horizontal(|ui| {
                    ui.label("format");
                    for (label, format) in [
                        ("png", Format::Png),
                        ("jpg", Format::Jpeg),
                        ("webp", Format::Webp),
                        ("bmp", Format::Bmp),
                        ("tiff", Format::Tiff),
                    ] {
                        if button(ui, label, dialog.format == format, palette) {
                            dialog.format = format;
                        }
                        ui.add_space(2.0);
                    }
                });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("save  (ctrl+s)").clicked() {
                        save_now = true;
                    }
                    if ui.button("close  (esc)").clicked() {
                        close = true;
                    }
                });
                if !dialog.note.is_empty() {
                    ui.label(egui::RichText::new(&dialog.note).color(palette.dim));
                }
                ui.label(
                    egui::RichText::new(if self.sheet.is_some() {
                        "the contact sheet is what gets saved"
                    } else {
                        "the picture is saved at its own size, not as zoomed on screen"
                    })
                    .color(palette.dim),
                );
            }
            Some(Overlay::Rename(rename)) => {
                ui.label(egui::RichText::new("batch rename").color(palette.accent));
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("pattern");
                    ui.add(
                        egui::TextEdit::singleline(&mut rename.pattern)
                            .font(FontId::monospace(13.0))
                            .desired_width(240.0),
                    );
                    ui.label("start");
                    ui.add(
                        egui::TextEdit::singleline(&mut rename.start)
                            .font(FontId::monospace(13.0))
                            .desired_width(60.0),
                    );
                });
                ui.label(
                    egui::RichText::new("{name} = the old name, {n} = the number; only images are renamed")
                        .color(palette.dim),
                );
                ui.separator();
                let images: Vec<PathBuf> = self
                    .files
                    .iter()
                    .filter(|p| is_image(p))
                    .cloned()
                    .collect();
                let width = images.len().max(1).to_string().len();
                for (i, path) in images.iter().take(12).enumerate() {
                    let old = path.file_name().unwrap_or_default().to_string_lossy();
                    let start: usize = rename.start.trim().parse().unwrap_or(1);
                    let new = rename_name(&rename.pattern, start + i, width, path);
                    ui.label(
                        egui::RichText::new(format!("{old}  →  {new}")).color(palette.fg),
                    );
                }
                if images.len() > 12 {
                    ui.label(egui::RichText::new(format!("… {} more", images.len() - 12)).color(palette.dim));
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("rename").clicked() {
                        save_now = true;
                    }
                    if ui.button("close  (esc)").clicked() {
                        close = true;
                    }
                });
                if !rename.note.is_empty() {
                    ui.label(egui::RichText::new(&rename.note).color(palette.dim));
                }
            }
            None => {}
        });

        if save_now {
            match self.overlay.as_ref() {
                Some(Overlay::Settings(_)) => self.save_settings(ui.ctx()),
                Some(Overlay::Save(_)) => self.save_picture(ui.ctx()),
                Some(Overlay::Rename(rename)) => {
                    let pattern = rename.pattern.clone();
                    let start: usize = rename.start.trim().parse().unwrap_or(1);
                    // Where the picture on screen will end up, so it stays selected
                    // after the folder is re-read.
                    let image_index = self
                        .files
                        .iter()
                        .take(self.index + 1)
                        .filter(|p| is_image(p))
                        .count()
                        .saturating_sub(1);
                    let width = self
                        .files
                        .iter()
                        .filter(|p| is_image(p))
                        .count()
                        .max(1)
                        .to_string()
                        .len();
                    let target = self.files.get(self.index).map(|path| {
                        rename_name(&pattern, start + image_index, width, path)
                    });
                    let result = rename_images(&self.files, &pattern, start);
                    if let Some(Overlay::Rename(rename)) = self.overlay.as_mut() {
                        rename.note = result;
                    }
                    self.reload_dir(ui.ctx(), target);
                }
                _ => {}
            }
        }
        if close {
            self.overlay = None;
        }
    }

    fn read_config(&mut self) {
        let Ok(text) = std::fs::read_to_string(config_path()) else {
            return;
        };
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if key.trim() == "background" {
                self.background = match value.trim().to_ascii_lowercase().as_str() {
                    "blur" => Background::Blur,
                    "solid" => Background::Solid,
                    _ => Background::Transparent,
                };
                continue;
            }
            let step = match value.trim().to_ascii_lowercase().as_str() {
                "prev" | "previous" | "back" => Step::Previous,
                "next" | "forward" => Step::Next,
                _ => continue,
            };
            match key.trim() {
                "mouse.left" => self.left_button = step,
                "mouse.right" => self.right_button = step,
                _ => {}
            }
        }
    }

    fn start_pending_video(&mut self, ctx: &egui::Context) {
        let Some(path) = self.pending_video.take() else {
            return;
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // A window of our own, stopping above the control bar: mpv paints into it
        // and the bar stays peepin's to draw and click.
        self.video_window = match (self.window_id, video_geometry(ctx)) {
            (Some(parent), Some(geometry)) => VideoWindow::new(parent, geometry),
            _ => None,
        };
        let socket = mpv_socket_path();
        let _ = std::fs::remove_file(&socket);

        let mut cmd = Command::new("mpv");
        // X11 explicitly: with WAYLAND_DISPLAY set, mpv would pick Wayland,
        // where `--wid` does not exist and it opens a window of its own.
        cmd.env_remove("WAYLAND_DISPLAY")
            .arg("--gpu-context=x11egl")
            .arg("--really-quiet")
            .arg("--no-terminal")
            .arg("--loop-file=inf")
            .arg("--no-osc")
            // Do not take the keyboard or the mouse: the window belongs to
            // peepin, and peepin's own keys (next, quit, open) must keep working
            // while the video plays.
            .arg("--input-vo-keyboard=no")
            .arg("--input-cursor=no")
            .arg("--no-input-default-bindings")
            .arg("--no-osd-bar")
            // Volume, subtitles and seeking go through here, from the bar.
            .arg(format!("--input-ipc-server={}", socket.display()));
        let embedded = match self.video_window.as_ref() {
            Some(window) => {
                cmd.arg(format!("--wid={}", window.window));
                true
            }
            None => {
                cmd.arg("--force-window=yes");
                false
            }
        };
        cmd.arg(&path);
        match cmd.spawn() {
            Ok(child) => {
                self.video = Some(child);
                self.embedded = embedded;
                self.mpv_socket = Some(socket);
                self.mpv = VideoState { volume: 100.0, ..VideoState::default() };
                self.mpv_polled = None;
                self.status = if embedded {
                    format!("▶ {name}  (mpv, embedded)")
                } else {
                    format!("▶ {name}  (mpv)")
                };
            }
            Err(err) => {
                self.video_window = None;
                self.status = format!("mpv: {err}");
            }
        }
    }

    /// A command for mpv, over its socket.
    fn mpv_command(&self, command: &[&str]) -> Option<String> {
        self.mpv_socket.as_ref().and_then(|s| mpv_send(s, command))
    }

    fn seek_to(&mut self, target: f64) {
        if self.mpv.duration <= 0.0 {
            return;
        }
        let target = target.clamp(0.0, self.mpv.duration);
        self.mpv.pos = target;
        let now = Instant::now();
        if self
            .seek_sent
            .map(|t| now.duration_since(t) < Duration::from_millis(120))
            .unwrap_or(false)
        {
            return;
        }
        self.seek_sent = Some(now);
        let value = format!("{target:.3}");
        self.mpv_command(&["seek", &value, "absolute+exact"]);
    }

    fn set_volume(&mut self, volume: f64) {
        let volume = volume.clamp(0.0, MAX_VOLUME);
        self.mpv.volume = volume;
        let value = format!("{volume:.0}");
        self.mpv_command(&["set_property", "volume", &value]);
    }

    /// Show track `id`, or none at all.
    fn select_sub(&mut self, id: Option<i64>) {
        let note = match id {
            Some(id) => {
                self.mpv_command(&["set_property", "sid", &id.to_string()]);
                self.mpv_command(&["set_property", "sub-visibility", "yes"]);
                self.mpv.sid = Some(id);
                self.mpv.subs = true;
                let place = self.mpv.tracks.iter().position(|t| t.id == id);
                match place {
                    Some(i) => format!("sub {}/{}: {}", i + 1, self.mpv.tracks.len(), track_name(&self.mpv.tracks[i])),
                    None => "subtitle".to_string(),
                }
            }
            None => {
                self.mpv_command(&["set_property", "sid", "no"]);
                self.mpv.sid = None;
                "subtitles off".to_string()
            }
        };
        self.video_note = Some((note, Instant::now()));
    }

    /// The next subtitle track, and off after the last one. With no track in the
    /// file at all, it is the visibility that toggles.
    fn cycle_subs(&mut self) {
        if self.mpv.tracks.is_empty() {
            self.toggle_subs();
            return;
        }
        let ids: Vec<i64> = self.mpv.tracks.iter().map(|t| t.id).collect();
        let next = match self.mpv.sid {
            None => ids.first().copied(),
            Some(current) => match ids.iter().position(|id| *id == current) {
                Some(i) if i + 1 < ids.len() => ids.get(i + 1).copied(),
                _ => None,
            },
        };
        self.select_sub(next);
    }

    /// Track by its place in the file: 0 is off, 1 is the first.
    fn pick_sub(&mut self, number: usize) {
        if number == 0 {
            self.select_sub(None);
            return;
        }
        let id = self.mpv.tracks.get(number - 1).map(|t| t.id);
        self.select_sub(id);
    }

    fn toggle_subs(&mut self) {
        let on = !self.mpv.subs;
        self.mpv.subs = on;
        self.mpv_command(&["set_property", "sub-visibility", if on { "yes" } else { "no" }]);
    }

    /// Load a subtitle file: the one named after the video if there is one, else
    /// the first subtitle in the folder.
    fn load_subtitle(&mut self) {
        let Some(video) = self.files.get(self.index).cloned() else {
            return;
        };
        let Some(dir) = video.parent() else {
            return;
        };
        let stem = video.file_stem().map(|s| s.to_owned()).unwrap_or_default();
        let mut names: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|read| {
                read.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_file() && has_ext(p, SUBTITLE_EXTS))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        let same = names
            .iter()
            .find(|p| p.file_stem() == Some(stem.as_os_str()))
            .cloned();
        match same.or_else(|| names.first().cloned()) {
            Some(path) => {
                let text = path.to_string_lossy().into_owned();
                self.mpv_command(&["sub-add", &text, "select"]);
                self.mpv_command(&["set_property", "sub-visibility", "yes"]);
                self.mpv.subs = true;
                self.mpv.tracks_stale = true;
                self.status = format!(
                    "subs: {}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                );
            }
            None => self.status = "no subtitle file in this folder".to_string(),
        }
    }

    /// Save the window as a PNG. `target` names the file; without one the name
    /// is `peepin-<date>-<time>.png` in the folder of the file on screen.
    fn screenshot(&mut self, target: Option<PathBuf>) -> Option<PathBuf> {
        let Some(window) = self.window_id else {
            self.say("screenshot: no X11 window to take it from");
            return None;
        };
        let Some(pixels) = capture_window(window) else {
            self.say("screenshot: the window could not be read");
            return None;
        };
        let path = match target {
            Some(path) => path,
            None => {
                let folder = self
                    .files
                    .get(self.index)
                    .and_then(|p| p.parent())
                    .map(|p| p.to_path_buf())
                    .or_else(|| std::env::current_dir().ok())
                    .unwrap_or_else(|| PathBuf::from("."));
                free_name(&folder, &format!("peepin-{}.png", stamp()))
            }
        };
        match pixels.save(&path) {
            Ok(()) => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.say(format!("shot → {name}"));
                Some(path)
            }
            Err(err) => {
                self.say(format!("shot: {err}"));
                None
            }
        }
    }

    /// A line in the status bar, and in the video bar, for a few seconds.
    fn say(&mut self, note: impl Into<String>) {
        let note = note.into();
        self.video_note = Some((note.clone(), Instant::now()));
        self.note = Some((note, Instant::now()));
    }

    /// Keep the child window over the video area, follow a resize, and ask mpv
    /// where it is a few times a second.
    fn poll_video(&mut self, ctx: &egui::Context) {
        if self.video.is_none() {
            return;
        }
        if let Some(child) = self.video.as_mut() {
            if let Ok(Some(_)) = child.try_wait() {
                self.stop_video();
                self.status = "video stopped".to_string();
                return;
            }
        }
        // A panel or the picker wants the whole window for a moment.
        let covered = self.overlay.is_some() || self.picker.is_some() || self.sub_menu;
        if let Some(window) = self.video_window.as_mut() {
            window.show(!covered);
            if let Some(geometry) = video_geometry(ctx) {
                window.resize(geometry);
            }
        }
        let due = self
            .mpv_polled
            .map(|t| t.elapsed() > Duration::from_millis(200))
            .unwrap_or(true);
        if due {
            if let Some(socket) = self.mpv_socket.clone() {
                self.mpv.update(&socket);
            }
            self.mpv_polled = Some(Instant::now());
        }
        // Test hooks: a start-up seek and a subtitle load, once mpv is running.
        if let Some(seconds) = self.seek_at_start {
            if self.mpv.duration > 0.0 {
                self.seek_at_start = None;
                self.seek_to(seconds);
            }
        }
        if self.sub_load_at_start && self.mpv.duration > 0.0 {
            self.sub_load_at_start = false;
            self.load_subtitle();
        }
        if self.sub_menu_at_start && !self.mpv.tracks.is_empty() && self.mpv.duration > 0.0 {
            self.sub_menu_at_start = false;
            self.sub_menu = true;
        }
        if let Some(number) = self.sub_pick_at_start {
            if !self.mpv.tracks.is_empty() && self.mpv.duration > 0.0 {
                self.sub_pick_at_start = None;
                self.pick_sub(number);
            }
        }
        if self.sub_cycle_at_start > 0 && !self.mpv.tracks.is_empty() && self.mpv.duration > 0.0 {
            let due = self
                .sub_cycle_at
                .map(|t| t.elapsed() > Duration::from_millis(900))
                .unwrap_or(true);
            if due {
                self.sub_cycle_at = Some(Instant::now());
                self.sub_cycle_at_start -= 1;
                self.cycle_subs();
            }
        }
    }

    /// The keys that belong to a playing video.
    fn video_keys(&mut self, ctx: &egui::Context) {
        let (space, up, down, mute, subs, load) = ctx.input(|i| {
            let plain = !(i.modifiers.command || i.modifiers.ctrl);
            (
                plain && i.key_pressed(egui::Key::Space),
                plain && i.key_pressed(egui::Key::ArrowUp),
                plain && i.key_pressed(egui::Key::ArrowDown),
                plain && i.key_pressed(egui::Key::M),
                plain && i.key_pressed(egui::Key::S),
                plain && i.key_pressed(egui::Key::L),
            )
        });
        if space {
            self.mpv_command(&["cycle", "pause"]);
            self.mpv.paused = !self.mpv.paused;
        }
        if up {
            self.set_volume(self.mpv.volume + 5.0);
        }
        if down {
            self.set_volume(self.mpv.volume - 5.0);
        }
        if mute {
            self.mpv_command(&["cycle", "mute"]);
            self.mpv.muted = !self.mpv.muted;
        }
        if subs {
            self.cycle_subs();
        }
        if load {
            self.load_subtitle();
        }
        // 1-9 pick a subtitle track straight away, 0 turns them off.
        let digit = ctx.input(|i| {
            if i.modifiers.command || i.modifiers.ctrl {
                return None;
            }
            [
                egui::Key::Num0,
                egui::Key::Num1,
                egui::Key::Num2,
                egui::Key::Num3,
                egui::Key::Num4,
                egui::Key::Num5,
                egui::Key::Num6,
                egui::Key::Num7,
                egui::Key::Num8,
                egui::Key::Num9,
            ]
            .iter()
            .position(|key| i.key_pressed(*key))
        });
        if let Some(number) = digit {
            if !self.mpv.tracks.is_empty() {
                self.pick_sub(number);
            }
        }
    }

    /// The list of subtitle tracks, above the chip. The video is out of the way
    /// while it is open, since it would otherwise be drawn over it.
    fn draw_sub_menu(&mut self, ui: &mut egui::Ui, bar: Rect) {
        let palette = self.palette;
        let ctx = ui.ctx().clone();
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.sub_menu = false;
            return;
        }

        let rows = self.mpv.tracks.len() + 1;
        let row_h = 22.0;
        let width = 360.0_f32.min((bar.width() - 16.0).max(200.0));
        let height = rows as f32 * row_h + 10.0;
        let rect = Rect::from_min_size(
            Pos2::new(bar.right() - 8.0 - width, bar.top() - height - 4.0),
            Vec2::new(width, height),
        );
        ui.painter().rect_filled(rect, 2.0, palette.bg);
        ui.painter()
            .rect_stroke(rect, 2.0, Stroke::new(1.0, palette.dim), StrokeKind::Inside);

        let hover = ctx.input(|i| i.pointer.hover_pos());
        let mut pick: Option<usize> = None;
        for i in 0..rows {
            let row = Rect::from_min_size(
                Pos2::new(rect.left() + 5.0, rect.top() + 5.0 + i as f32 * row_h),
                Vec2::new(rect.width() - 10.0, row_h),
            );
            let (selected, label) = if i == 0 {
                (self.mpv.sid.is_none(), "off".to_string())
            } else {
                let track = &self.mpv.tracks[i - 1];
                (
                    self.mpv.sid == Some(track.id),
                    format!("{}  {}", i, track_name(track)),
                )
            };
            if hover.map(|p| row.contains(p)).unwrap_or(false) {
                ui.painter()
                    .rect_filled(row, 1.0, palette.dim.gamma_multiply(0.35));
            }
            ui.painter().text(
                Pos2::new(row.left() + 6.0, row.center().y),
                Align2::LEFT_CENTER,
                shorten(&format!("{} {label}", if selected { "●" } else { " " }), 46),
                FontId::monospace(12.0),
                if selected { palette.accent } else { palette.fg },
            );
            let hit = ui.interact(row, egui::Id::new(("sub-row", i)), egui::Sense::click());
            if hit.clicked() {
                pick = Some(i);
            }
        }

        match pick {
            Some(0) => {
                self.select_sub(None);
                self.sub_menu = false;
            }
            Some(i) => {
                let id = self.mpv.tracks[i - 1].id;
                self.select_sub(Some(id));
                self.sub_menu = false;
            }
            None => {
                // A left click anywhere else closes it.
                let outside = ctx
                    .input(|i| i.pointer.button_clicked(egui::PointerButton::Primary))
                    && !hover.map(|p| rect.contains(p)).unwrap_or(false);
                if outside {
                    self.sub_menu = false;
                }
            }
        }
    }

    /// The bar under a video: name, seek, clock, pause, mute, volume, subtitles.
    fn video_bar(&mut self, ui: &mut egui::Ui) {
        let palette = self.palette;
        let area = ui.max_rect();
        let bar = Rect::from_min_max(
            Pos2::new(area.left(), area.bottom() - VIDEO_BAR),
            area.max,
        );
        ui.painter().rect_filled(bar, 0.0, palette.bg);
        ui.painter().line_segment(
            [Pos2::new(bar.left(), bar.top()), Pos2::new(bar.right(), bar.top())],
            Stroke::new(1.0, palette.dim),
        );

        let centre = bar.center().y;
        let pad = 8.0;
        let chip_h = 20.0;
        let chip_at = |right: f32, width: f32| {
            Rect::from_min_size(
                Pos2::new(right - width, centre - chip_h / 2.0),
                Vec2::new(width, chip_h),
            )
        };
        let load = chip_at(bar.right() - pad, 30.0);
        let subs = chip_at(load.left() - 5.0, 30.0);
        let mute = chip_at(subs.left() - 5.0, 24.0);
        let pause = chip_at(mute.left() - 5.0, 24.0);
        let vol_number = Rect::from_min_size(
            Pos2::new(pause.left() - 42.0, centre - 8.0),
            Vec2::new(40.0, 16.0),
        );
        let vol = Rect::from_min_size(
            Pos2::new(vol_number.left() - 82.0, centre - 5.0),
            Vec2::new(78.0, 10.0),
        );
        let clock_rect = Rect::from_min_size(
            Pos2::new(vol.left() - 104.0, centre - 8.0),
            Vec2::new(100.0, 16.0),
        );
        let name_room = (clock_rect.left() - bar.left() - 2.0 * pad).max(60.0);
        let name_rect = Rect::from_min_size(
            Pos2::new(bar.left() + pad, centre - 8.0),
            Vec2::new(name_room * 0.34, 16.0),
        );
        let seek = Rect::from_min_max(
            Pos2::new(name_rect.right() + 8.0, centre - 5.0),
            Pos2::new(clock_rect.left() - 8.0, centre + 5.0),
        );

        let name = self
            .files
            .get(self.index)
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Just after a track was picked the bar says which one, in words.
        let note = self
            .video_note
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_millis(2500));
        let (text, colour) = match note {
            Some((note, _)) => (
                fit_text(ui.painter(), note, name_rect.width(), FontId::monospace(12.0), palette.accent),
                palette.accent,
            ),
            None => (
                fit_text(ui.painter(), &name, name_rect.width(), FontId::monospace(12.0), palette.fg),
                palette.fg,
            ),
        };
        ui.painter().text(
            name_rect.left_top(),
            Align2::LEFT_TOP,
            text,
            FontId::monospace(12.0),
            colour,
        );
        ui.painter().text(
            clock_rect.right_top(),
            Align2::RIGHT_TOP,
            format!("{} / {}", clock(self.mpv.pos), clock(self.mpv.duration)),
            FontId::monospace(12.0),
            palette.dim,
        );

        // The two bars: position and volume.
        let position = if self.mpv.duration > 0.0 {
            (self.mpv.pos / self.mpv.duration).clamp(0.0, 1.0)
        } else {
            0.0
        };
        paint_bar(ui, seek, position as f32, palette);
        paint_bar(ui, vol, (self.mpv.volume / MAX_VOLUME) as f32, palette);
        ui.painter().text(
            vol_number.right_top(),
            Align2::RIGHT_TOP,
            format!("{:.0}", self.mpv.volume),
            FontId::monospace(12.0),
            palette.dim,
        );

        // Chips: pause, mute, subtitles, load subtitles.
        paint_chip(ui, pause, if self.mpv.paused { "▶" } else { "||" }, false, palette);
        paint_chip(ui, mute, "m", self.mpv.muted, palette);
        let subs_label = self.mpv.label();
        paint_chip(ui, subs, &subs_label, self.mpv.subs && self.mpv.sid.is_some(), palette);
        paint_chip(ui, load, "srt", false, palette);

        let pause_hit = ui.interact(pause, egui::Id::new("video-pause"), egui::Sense::click());
        if pause_hit.clicked() {
            self.mpv_command(&["cycle", "pause"]);
            self.mpv.paused = !self.mpv.paused;
        }
        let mute_hit = ui.interact(mute, egui::Id::new("video-mute"), egui::Sense::click());
        if mute_hit.clicked() {
            self.mpv_command(&["cycle", "mute"]);
            self.mpv.muted = !self.mpv.muted;
        }
        let subs_hit = ui
            .interact(subs, egui::Id::new("video-subs"), egui::Sense::click())
            .on_hover_text(subtitle_hint(&self.mpv));
        if subs_hit.clicked() {
            self.cycle_subs();
        }
        if subs_hit.secondary_clicked() {
            self.sub_menu = !self.sub_menu;
        }
        let load_hit = ui.interact(load, egui::Id::new("video-load"), egui::Sense::click());
        if load_hit.clicked() {
            self.load_subtitle();
        }

        let seek_hit = ui.interact(seek, egui::Id::new("video-seek"), egui::Sense::click_and_drag());
        if seek_hit.dragged() || seek_hit.clicked() {
            if let Some(point) = seek_hit.interact_pointer_pos() {
                let fraction = ((point.x - seek.left()) / seek.width()).clamp(0.0, 1.0);
                self.seek_to(fraction as f64 * self.mpv.duration);
            }
        }
        let vol_hit = ui.interact(vol, egui::Id::new("video-volume"), egui::Sense::click_and_drag());
        if vol_hit.dragged() || vol_hit.clicked() {
            if let Some(point) = vol_hit.interact_pointer_pos() {
                let fraction = ((point.x - vol.left()) / vol.width()).clamp(0.0, 1.0);
                self.set_volume(fraction as f64 * MAX_VOLUME);
            }
        }

        if self.sub_menu {
            self.draw_sub_menu(ui, bar);
        }

        // The wheel over the bar is volume, the way a terminal player does it.
        let (hover, wheel) = ui.ctx().input(|i| (i.pointer.hover_pos(), i.smooth_scroll_delta.y));
        if let Some(point) = hover {
            if bar.contains(point) && wheel.abs() > 0.5 && !vol_hit.dragged() {
                let step = if wheel > 0.0 { 3.0 } else { -3.0 };
                self.set_volume(self.mpv.volume + step);
            }
        }
    }

    fn stop_video(&mut self) {
        self.pending_video = None;
        self.embedded = false;
        if let Some(mut child) = self.video.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // The window goes after mpv: its own window lives inside ours.
        self.video_window = None;
        if let Some(socket) = self.mpv_socket.take() {
            let _ = std::fs::remove_file(socket);
        }
        self.mpv = VideoState::default();
        self.mpv_polled = None;
        self.seek_sent = None;
        self.seek_at_start = None;
        self.sub_load_at_start = false;
    }

    fn step(&mut self, ctx: &egui::Context, forward: bool) {
        if self.files.is_empty() {
            return;
        }
        let len = self.files.len();
        self.index = if forward {
            (self.index + 1) % len
        } else {
            (self.index + len - 1) % len
        };
        self.load(ctx);
    }

    fn load(&mut self, ctx: &egui::Context) {
        self.stop_video();
        self.sheet = None;
        self.texture = None;
        self.zoom = 1.0;
        self.pan = Vec2::ZERO;

        let Some(path) = self.files.get(self.index).cloned() else {
            self.status = "no files — Ctrl+O to pick one".to_string();
            return;
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        if is_video(&path) {
            self.pending_video = Some(path.clone());
            self.status = format!("▶ {name}");
            return;
        }

        match load_image(&path) {
            Ok(image) => {
                self.source = Some(image.clone());
                let size = Vec2::new(image.width() as f32, image.height() as f32);
                self.image_size = size;
                self.blur = Some(ctx.load_texture(
                    "peepin-blur",
                    blur_version(&image),
                    egui::TextureOptions::LINEAR,
                ));
                self.texture =
                    Some(ctx.load_texture("peepin", image, egui::TextureOptions::LINEAR));
                self.status = format!("{name}  {}x{}", size.x as u32, size.y as u32);
            }
            Err(err) => {
                self.blur = None;
                self.status = format!("{name}: {err}")
            }
        }
    }

    fn open_picker(&mut self) {
        let start = self
            .files
            .get(self.index)
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("/"));
        self.picker = Some(Pick {
            path: start,
            folders: Pane { entries: Vec::new(), selected: 0 },
            files: Pane { entries: Vec::new(), selected: 0 },
            on_folders: true,
            error: String::new(),
            preview: self.previews,
            thumbs: std::collections::HashMap::new(),
            scroll: false,
        });
        self.refresh_picker();
    }

    fn refresh_picker(&mut self) {
        let Some(pick) = self.picker.as_mut() else {
            return;
        };
        pick.folders.entries.clear();
        pick.files.entries.clear();
        pick.folders.selected = 0;
        pick.files.selected = 0;
        pick.error.clear();

        if let Some(parent) = pick.path.parent() {
            pick.folders
                .entries
                .push(("../".to_string(), parent.to_path_buf()));
        }
        match std::fs::read_dir(&pick.path) {
            Ok(read) => {
                let mut dirs = Vec::new();
                let mut files = Vec::new();
                for entry in read.flatten() {
                    let path = entry.path();
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with('.') {
                        continue;
                    }
                    if path.is_dir() {
                        dirs.push((format!("{name}/"), path));
                    } else if is_image(&path) || is_video(&path) {
                        files.push((name, path));
                    }
                }
                dirs.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
                files.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
                pick.folders.entries.extend(dirs);
                pick.files.entries.extend(files);
            }
            Err(err) => pick.error = err.to_string(),
        }
    }

    fn open_path(&mut self, ctx: &egui::Context, path: PathBuf) {
        if path.is_dir() {
            let (files, index) = scan(&path);
            self.files = files;
            self.index = index;
            self.load(ctx);
        } else if is_image(&path) || is_video(&path) {
            let dir = path.parent().map(|p| p.to_path_buf());
            let files = match &dir {
                Some(dir) => media_in(dir),
                None => vec![path.clone()],
            };
            self.files = files;
            self.index = self.files.iter().position(|p| p == &path).unwrap_or(0);
            self.load(ctx);
        }
    }

    fn zoom_by(&mut self, factor: f32) {
        self.zoom = (self.zoom * factor).clamp(0.05, 64.0);
    }

    fn picker_keys(&mut self, ctx: &egui::Context) {
        let Some(pick) = self.picker.as_mut() else {
            return;
        };
        let (up, down, enter, back, tab, left, right, escape) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::Backspace),
                i.key_pressed(egui::Key::Tab),
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
                i.key_pressed(egui::Key::Escape),
            )
        });

        if escape {
            self.picker = None;
            return;
        }
        // Right on a folder enters it, Left goes one folder up — the same as a
        // file manager's arrows. Tab is what switches columns.
        if right && pick.on_folders {
            if let Some((_, path)) = pick.folders.entries.get(pick.folders.selected).cloned() {
                pick.path = path;
                self.refresh_picker();
            }
            return;
        }
        if left {
            if let Some(parent) = pick.path.parent() {
                pick.path = parent.to_path_buf();
                self.refresh_picker();
            }
            return;
        }
        if tab {
            pick.on_folders = !pick.on_folders;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::V)) {
            pick.preview = !pick.preview;
            self.previews = pick.preview;
        }
        if up || down {
            pick.scroll = true;
            let pane = if pick.on_folders { &mut pick.folders } else { &mut pick.files };
            if !pane.entries.is_empty() {
                pane.selected = if up {
                    (pane.selected + pane.entries.len() - 1) % pane.entries.len()
                } else {
                    (pane.selected + 1) % pane.entries.len()
                };
            }
        }
        if back {
            if let Some(parent) = pick.path.parent() {
                pick.path = parent.to_path_buf();
                self.refresh_picker();
            }
            return;
        }
        if enter {
            let target = if pick.on_folders {
                pick.folders.entries.get(pick.folders.selected).cloned()
            } else {
                pick.files.entries.get(pick.files.selected).cloned()
            };
            if let Some((_, path)) = target {
                if path.is_dir() {
                    pick.path = path;
                    self.refresh_picker();
                } else {
                    self.picker = None;
                    self.open_path(ctx, path);
                }
            }
        }
    }

    fn draw_picker(&mut self, ui: &mut egui::Ui) {
        if self.picker.is_none() {
            return;
        }
        let palette = self.palette;
        let mut folder_click: Option<PathBuf> = None;
        let mut file_click: Option<PathBuf> = None;
        let pick = self.picker.as_mut().unwrap();

        // Drawn straight into our own layer: an `egui::Window` does not show up
        // under eframe's ui-based app entry point.
        let screen = ui.max_rect();
        let size = Vec2::new(screen.width().min(760.0), screen.height().min(460.0));
        let rect = Rect::from_min_size(
            Pos2::new(screen.left() + 12.0, screen.top() + 12.0),
            size,
        );
        let mut panel = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );
        panel.painter().rect_filled(rect, 2.0, palette.bg.gamma_multiply(0.94));
        panel.painter().rect_stroke(rect, 2.0, Stroke::new(1.0, palette.dim), StrokeKind::Inside);

        egui::Frame::new()
            .inner_margin(6.0)
            .show(&mut panel, |ui| {
                let height = (ui.available_height() - 22.0).max(80.0);
                ui.horizontal_top(|ui| {
                    let column = ui.available_width() * 0.4;
                    ui.allocate_ui_with_layout(
                        Vec2::new(column, height),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.label(egui::RichText::new("folders").color(palette.dim));
                            egui::ScrollArea::vertical()
                                .id_salt("folders")
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    for (i, (name, path)) in pick.folders.entries.iter().enumerate() {
                                        let picked = pick.on_folders && i == pick.folders.selected;
                                        if pane_row(ui, name, picked, palette) {
                                            folder_click = Some(path.clone());
                                        }
                                    }
                                });
                        },
                    );
                    ui.separator();
                    ui.allocate_ui_with_layout(
                        Vec2::new(ui.available_width(), height),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            let header = if pick.preview {
                                "files   (v: names only)"
                            } else {
                                "files   (v: previews)"
                            };
                            ui.label(egui::RichText::new(header).color(palette.dim));
                            let files = pick.files.entries.clone();
                            if pick.preview {
                                // One row is a thumbnail and its name; only the rows
                                // on screen are decoded, so a big folder stays quick.
                                let row = 76.0;
                                let mut budget = 2;
                                egui::ScrollArea::vertical()
                                    .id_salt("files-preview")
                                    .auto_shrink([false, false])
                                    .show_rows(ui, row, files.len(), |ui, range| {
                                        for i in range {
                                            let (name, path) = files[i].clone();
                                            let picked =
                                                !pick.on_folders && i == pick.files.selected;
                                            let mut hit = false;
                                            let row_rect = ui
                                                .horizontal(|ui| {
                                                let mut later = false;
                                                let thumb = thumb_for(
                                                    ui.ctx(),
                                                    &mut pick.thumbs,
                                                    &path,
                                                    &mut budget,
                                                    &mut later,
                                                );
                                                if later {
                                                    ui.ctx().request_repaint();
                                                }
                                                let (rect, response) = ui.allocate_exact_size(
                                                    Vec2::new(96.0, 72.0),
                                                    egui::Sense::click(),
                                                );
                                                match thumb {
                                                    Some(tex) => {
                                                        ui.painter().image(
                                                            tex.id(),
                                                            rect,
                                                            Rect::from_min_max(
                                                                Pos2::ZERO,
                                                                Pos2::new(1.0, 1.0),
                                                            ),
                                                            Color32::WHITE,
                                                        );
                                                    }
                                                    None => {
                                                        ui.painter().rect_stroke(
                                                            rect,
                                                            1.0,
                                                            Stroke::new(1.0, palette.dim),
                                                            StrokeKind::Inside,
                                                        );
                                                        ui.painter().text(
                                                            rect.center(),
                                                            Align2::CENTER_CENTER,
                                                            if is_video(&path) { "▶" } else { "?" },
                                                            FontId::monospace(14.0),
                                                            palette.dim,
                                                        );
                                                    }
                                                }
                                                hit |= response.clicked();
                                                if pane_row(ui, &name, picked, palette) {
                                                    hit = true;
                                                }
                                            })
                                                .response
                                                .rect;
                                            if picked && pick.scroll {
                                                ui.scroll_to_rect(
                                                    row_rect,
                                                    Some(egui::Align::Center),
                                                );
                                                pick.scroll = false;
                                            }
                                            if hit {
                                                file_click = Some(path);
                                            }
                                        }
                                    });
                                if files.is_empty() {
                                    ui.label(
                                        egui::RichText::new("no images or videos here")
                                            .color(palette.dim),
                                    );
                                }
                            } else {
                                egui::ScrollArea::vertical()
                                    .id_salt("files")
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        for (i, (name, path)) in files.iter().enumerate() {
                                            let picked = !pick.on_folders && i == pick.files.selected;
                                            if picked && pick.scroll {
                                                ui.scroll_to_rect(
                                                    Rect::from_min_size(
                                                        ui.cursor().min,
                                                        Vec2::new(ui.available_width(), 18.0),
                                                    ),
                                                    Some(egui::Align::Center),
                                                );
                                                pick.scroll = false;
                                            }
                                            if pane_row(ui, name, picked, palette) {
                                                file_click = Some(path.clone());
                                            }
                                        }
                                        if files.is_empty() {
                                            ui.label(
                                                egui::RichText::new("no images or videos here")
                                                    .color(palette.dim),
                                            );
                                        }
                                    });
                            }
                        },
                    );
                });
                pick.scroll = false;

                if !pick.error.is_empty() {
                    ui.label(egui::RichText::new(&pick.error).color(Color32::LIGHT_RED));
                }
                // The path, inverse video along the bottom, like a TUI status line.
                let text = pick.path.to_string_lossy().into_owned();
                let bar = Rect::from_min_size(
                    Pos2::new(ui.min_rect().left(), ui.max_rect().bottom() - 18.0),
                    Vec2::new(ui.max_rect().width(), 18.0),
                );
                ui.painter().rect_filled(bar, 0.0, palette.dim);
                ui.painter().text(
                    Pos2::new(bar.left() + 4.0, bar.top() + 1.0),
                    Align2::LEFT_TOP,
                    shorten(&text, 110),
                    FontId::monospace(12.0),
                    palette.bg,
                );
            });

        if let Some(path) = folder_click {
            if let Some(pick) = self.picker.as_mut() {
                pick.path = path;
                pick.on_folders = true;
            }
            self.refresh_picker();
        }
        if let Some(path) = file_click {
            self.picker = None;
            self.open_path(ui.ctx(), path);
        }
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        let palette = self.palette;
        let rect = ui.max_rect();
        let bar = Rect::from_min_max(
            Pos2::new(rect.left() + 6.0, rect.bottom() - 20.0),
            Pos2::new(rect.right() - 6.0, rect.bottom() - 2.0),
        );
        let font = FontId::monospace(12.0);
        let painter = ui.painter();
        let name = self
            .files
            .get(self.index)
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "peepin".to_string());
        let count = if self.files.is_empty() {
            String::new()
        } else {
            format!("{}/{}", self.index + 1, self.files.len())
        };
        let zoom = format!("zoom {:.0}%", self.zoom * 100.0);
        let background = match self.background {
            Background::Transparent => "clear",
            Background::Blur => "blur",
            Background::Solid => "solid",
        };
        // From the full line down to the bare name: a long file name takes the
        // room the hints would have had, and keeps its own tail in sight.
        let tails = [
            format!(
                "   {count}   {zoom}   [←/→] next  [↑/↓ wheel] zoom  [b] {background}  [p] shot  [ctrl+o] open  [ctrl+q] quit"
            ),
            format!("   {count}   {zoom}   [b] {background}  [p] shot"),
            format!("   {count}   {zoom}"),
            format!("   {count}"),
            String::new(),
        ];
        let room = bar.width() - 4.0;
        let fresh = self
            .note
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(4));
        let tails = match fresh {
            Some((note, _)) => vec![format!("   {count}   {note}")],
            None => tails.to_vec(),
        };
        let mut text = String::new();
        for tail in tails {
            let used = painter
                .layout_no_wrap(tail.clone(), font.clone(), palette.fg)
                .size()
                .x;
            let for_name = room - used;
            if for_name >= 40.0 || tail.is_empty() {
                text = format!(
                    "{}{tail}",
                    fit_text(painter, &name, for_name.max(10.0), font.clone(), palette.fg)
                );
                break;
            }
        }
        painter.rect_filled(bar.expand2(Vec2::new(2.0, 1.0)), 2.0, palette.bg.gamma_multiply(0.7));
        painter.text(
            Pos2::new(bar.left(), bar.top()),
            Align2::LEFT_TOP,
            fit_text(painter, &text, room, font, palette.fg),
            FontId::monospace(12.0),
            palette.fg,
        );
    }

    fn draw_background(&self, ui: &mut egui::Ui) {
        let palette = self.palette;
        let area = ui.max_rect();
        let painter = ui.painter();
        match self.background {
            Background::Transparent => {}
            Background::Solid => {
                painter.rect_filled(area, 0.0, palette.bg);
            }
            Background::Blur => {
                // Opaque first: a picture with its own transparency would otherwise
                // let the desktop show through the backdrop. With nothing loaded
                // there is nothing to blur, so the panel stays see-through.
                let blur_alpha = if self.blur.is_some() { 255 } else { 200 };
                painter.rect_filled(
                    area,
                    0.0,
                    Color32::from_rgba_unmultiplied(
                        palette.bg.r(),
                        palette.bg.g(),
                        palette.bg.b(),
                        blur_alpha,
                    ),
                );
                if let Some(blur) = &self.blur {
                    painter.image(
                        blur.id(),
                        area,
                        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                        Color32::WHITE,
                    );
                    // Dim it, so the picture in front stays the subject.
                    painter.rect_filled(
                        area,
                        0.0,
                        Color32::from_rgba_unmultiplied(
                            palette.bg.r(),
                            palette.bg.g(),
                            palette.bg.b(),
                            150,
                        ),
                    );
                }
            }
        }
    }

    /// A line in the middle of the window saying what is happening.
    fn busy(&self, ui: &mut egui::Ui, text: &str) {
        let palette = self.palette;
        let rect = ui.max_rect();
        let centre = Pos2::new(rect.center().x, rect.center().y);
        let painter = ui.painter();
        let message = format!("{text}");

        let galley = painter.layout_no_wrap(
            message,
            FontId::monospace(15.0),
            palette.accent,
        );
        let box_rect = Rect::from_center_size(
            centre,
            galley.size() + Vec2::new(24.0, 14.0),
        );
        painter.rect_filled(box_rect, 2.0, palette.bg.gamma_multiply(0.85));
        painter.rect_stroke(box_rect, 2.0, Stroke::new(1.0, palette.dim), StrokeKind::Inside);
        painter.galley(
            Pos2::new(
                box_rect.center().x - galley.size().x / 2.0,
                box_rect.center().y - galley.size().y / 2.0,
            ),
            galley,
            palette.fg,
        );
    }

    fn draw_sheet(&self, ui: &mut egui::Ui) {
        let Some(sheet) = &self.sheet else {
            return;
        };
        let palette = self.palette;
        let painter = ui.painter();
        let area = ui.max_rect();
        let area = Rect::from_min_max(area.min, Pos2::new(area.max.x, area.max.y - 22.0));
        let size = Vec2::new(sheet.image.width() as f32, sheet.image.height() as f32);
        let fit = (area.width() / size.x)
            .min(area.height() / size.y)
            .min(1.0);
        let rect = Rect::from_center_size(area.center() + self.pan, size * fit * self.zoom);
        painter.image(
            sheet.texture.id(),
            rect,
            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            Color32::WHITE,
        );
        painter.rect_stroke(area, 0.0, Stroke::new(1.0, palette.dim), StrokeKind::Inside);
    }

    fn sheet_status(&self, ui: &mut egui::Ui) {
        let palette = self.palette;
        let Some(sheet) = &self.sheet else {
            return;
        };
        let font = FontId::monospace(12.0);
        let painter = ui.painter();
        let rect = ui.max_rect();
        let bar = Rect::from_min_max(
            Pos2::new(rect.left() + 6.0, rect.bottom() - 20.0),
            Pos2::new(rect.right() - 6.0, rect.bottom() - 2.0),
        );
        let room = bar.width() - 4.0;
        let text = fit_text(
            painter,
            &format!(
                "contact sheet · {} image(s) · zoom {:.0}%   [←/→] next file  [↑/↓ wheel] zoom  [ctrl+s] save  [p] shot  [F3] close",
                sheet.count,
                self.zoom * 100.0
            ),
            room,
            font.clone(),
            palette.fg,
        );
        painter.rect_filled(bar.expand2(Vec2::new(2.0, 1.0)), 2.0, palette.bg.gamma_multiply(0.7));
        painter.text(
            Pos2::new(bar.left(), bar.top()),
            Align2::LEFT_TOP,
            text,
            font,
            palette.fg,
        );
    }

    fn draw_image(&self, ui: &mut egui::Ui) {
        let Some(texture) = &self.texture else {
            return;
        };
        let palette = self.palette;
        let painter = ui.painter();
        let area = ui.max_rect();
        let area = Rect::from_min_max(area.min, Pos2::new(area.max.x, area.max.y - 22.0));

        let fit = (area.width() / self.image_size.x)
            .min(area.height() / self.image_size.y)
            .min(1.0);
        let scale = fit * self.zoom;
        let size = self.image_size * scale;
        let centre = area.center() + self.pan;
        let rect = Rect::from_center_size(centre, size);
        painter.image(
            texture.id(),
            rect,
            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            Color32::WHITE,
        );
        painter.rect_stroke(area, 0.0, Stroke::new(1.0, palette.dim), StrokeKind::Inside);
    }
}

/// One clickable row of a picker pane; returns true when clicked.
fn pane_row(ui: &mut egui::Ui, name: &str, selected: bool, palette: Palette) -> bool {
    let (colour, background) = if selected {
        (palette.bg, palette.accent)
    } else {
        (palette.fg, Color32::TRANSPARENT)
    };
    ui.add(
        egui::Button::new(egui::RichText::new(name).color(colour).font(FontId::monospace(13.0)))
            .fill(background)
            .frame(selected),
    )
    .clicked()
}

/// The key list F1 shows.
const KEYS: &[(&str, &str)] = &[
    ("← →", "previous / next file"),
    ("mouse left / right", "previous / next (see F2)"),
    ("wheel, ↑ ↓", "zoom in / out"),
    ("middle drag", "pan a zoomed picture"),
    ("0", "fit to window again"),
    ("ctrl + ← → ↑ ↓", "resize the window"),
    ("b", "background: clear, blur, solid"),
    ("p", "screenshot of the window"),
    ("space (video)", "pause"),
    ("↑ ↓ (video)", "volume"),
    ("m / s (video)", "mute / next subtitle track"),
    ("0-9 (video)", "subtitle track, 0 is off"),
    ("srt (video)", "load a subtitle from this folder"),
    ("ctrl+o", "open: folders left, files right"),
    ("v in the picker", "thumbnails beside the names"),
    ("ctrl+s", "save as (format of your choice)"),
    ("F1", "this list"),
    ("F2", "settings"),
    ("F3", "contact sheet of this folder"),
    ("F11", "batch rename the images here"),
    ("ctrl+q", "quit"),
];

/// A thumbnail for the picker's right column, cached per path so scrolling does
/// not decode the same picture twice.
fn thumb_for(
    ctx: &egui::Context,
    cache: &mut std::collections::HashMap<PathBuf, Option<egui::TextureHandle>>,
    path: &Path,
    budget: &mut u32,
    later: &mut bool,
) -> Option<egui::TextureHandle> {
    if let Some(entry) = cache.get(path) {
        return entry.clone();
    }
    // Decoding a big DDS takes a moment, so only a couple are made per frame and
    // the rest arrive over the next few frames instead of the list standing still.
    if *budget == 0 {
        *later = true;
        return None;
    }
    *budget -= 1;
    let made = load_image(path).ok().map(|image| {
        let small = thumbnail(&image, 96, 72);
        ctx.load_texture(
            format!("thumb-{}", path.to_string_lossy()),
            small,
            egui::TextureOptions::LINEAR,
        )
    });
    if cache.len() > 256 {
        cache.clear();
    }
    cache.insert(path.to_path_buf(), made.clone());
    made
}

/// `image` scaled to fill a w x h box and centre-cropped, for a picker thumbnail.
fn thumbnail(image: &egui::ColorImage, w: u32, h: u32) -> egui::ColorImage {
    let rgba: Vec<u8> = image
        .pixels
        .iter()
        .flat_map(|p| [p.r(), p.g(), p.b(), p.a()])
        .collect();
    let Some(src) = image::RgbaImage::from_raw(image.width() as u32, image.height() as u32, rgba)
    else {
        return image.clone();
    };
    let scale = (w as f32 / src.width() as f32).max(h as f32 / src.height() as f32);
    let sw = ((src.width() as f32 * scale).ceil() as u32).max(w);
    let sh = ((src.height() as f32 * scale).ceil() as u32).max(h);
    let scaled = image::imageops::resize(&src, sw, sh, image::imageops::FilterType::Triangle);
    let cropped =
        image::imageops::crop_imm(&scaled, (sw - w) / 2, (sh - h) / 2, w, h).to_image();
    egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], cropped.as_raw())
}

/// A track as a person reads it: its language and its title.
fn track_name(track: &SubTrack) -> String {
    let mut name = String::new();
    if !track.lang.is_empty() {
        name.push_str(&track.lang);
    }
    if !track.title.is_empty() {
        if !name.is_empty() {
            name.push_str(" — ");
        }
        name.push_str(&track.title);
    }
    if name.is_empty() {
        name = format!("track {}", track.id);
    }
    name
}

/// What the subtitle chip would do, and which tracks there are.
fn subtitle_hint(state: &VideoState) -> String {
    if state.tracks.is_empty() {
        return "no subtitle track in this file — srt loads one from the folder".to_string();
    }
    let mut text = String::from("subtitle tracks:\n");
    for (i, track) in state.tracks.iter().enumerate() {
        let mark = if state.sid == Some(track.id) { "●" } else { "○" };
        text.push_str(&format!("  {mark} {} {}\n", i + 1, track_name(track)));
    }
    text.push_str("click: next one · right-click: this list · 0-9 pick one");
    text
}

/// A thin bar with a filled part: the seek position and the volume.
fn paint_bar(ui: &egui::Ui, rect: Rect, fraction: f32, palette: Palette) {
    let fraction = fraction.clamp(0.0, 1.0);
    let track = Rect::from_center_size(rect.center(), Vec2::new(rect.width(), 6.0));
    ui.painter().rect_filled(track, 1.0, palette.dim.gamma_multiply(0.45));
    let filled = Rect::from_min_max(
        track.min,
        Pos2::new(track.left() + track.width() * fraction, track.bottom()),
    );
    ui.painter().rect_filled(filled, 1.0, palette.accent);
    let caret = Rect::from_center_size(
        Pos2::new(filled.right(), track.center().y),
        Vec2::new(3.0, track.height() + 4.0),
    );
    ui.painter().rect_filled(caret, 1.0, palette.fg);
}

/// One of the small toggles on the video bar.
fn paint_chip(ui: &egui::Ui, rect: Rect, text: &str, active: bool, palette: Palette) {
    let (fill, colour) = if active {
        (palette.accent, palette.bg)
    } else {
        (palette.bg, palette.fg)
    };
    ui.painter().rect_filled(rect, 2.0, fill);
    ui.painter().rect_stroke(rect, 2.0, Stroke::new(1.0, palette.dim), StrokeKind::Inside);
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        FontId::monospace(12.0),
        colour,
    );
}

/// A row of the panels: a button that reads like a terminal toggle.
fn button(ui: &mut egui::Ui, label: &str, picked: bool, palette: Palette) -> bool {
    ui.add(
        egui::Button::new(
            egui::RichText::new(label)
                .color(if picked { palette.bg } else { palette.fg })
                .monospace(),
        )
        .fill(if picked { palette.accent } else { Color32::TRANSPARENT })
        .frame(picked),
    )
    .clicked()
}

/// Write a picture in the chosen format. JPEG has no alpha, so it is composited
/// on white first; the others keep it.
fn write_image(image: &egui::ColorImage, path: &Path, format: Format) -> Result<(), String> {
    let width = image.width() as u32;
    let height = image.height() as u32;
    let rgba: Vec<u8> = image.pixels.iter().flat_map(|p| [p.r(), p.g(), p.b(), p.a()]).collect();
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut out = std::io::BufWriter::new(file);
    match format {
        Format::Png => {
            let encoder = image::codecs::png::PngEncoder::new(&mut out);
            image::ImageEncoder::write_image(encoder, &rgba, width, height, image::ExtendedColorType::Rgba8)
                .map_err(|e| e.to_string())
        }
        Format::Jpeg => {
            let mut rgb = Vec::with_capacity((width * height * 3) as usize);
            for p in &image.pixels {
                let a = p.a() as u32;
                let blend = |c: u8| ((c as u32 * a + 255 * (255 - a)) / 255) as u8;
                rgb.extend_from_slice(&[blend(p.r()), blend(p.g()), blend(p.b())]);
            }
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 92);
            encoder
                .encode(&rgb, width, height, image::ExtendedColorType::Rgb8)
                .map_err(|e| e.to_string())
        }
        Format::Webp => {
            let encoder = image::codecs::webp::WebPEncoder::new_lossless(&mut out);
            image::ImageEncoder::write_image(encoder, &rgba, width, height, image::ExtendedColorType::Rgba8)
                .map_err(|e| e.to_string())
        }
        Format::Bmp => {
            let encoder = image::codecs::bmp::BmpEncoder::new(&mut out);
            image::ImageEncoder::write_image(encoder, &rgba, width, height, image::ExtendedColorType::Rgba8)
                .map_err(|e| e.to_string())
        }
        Format::Tiff => {
            let encoder = image::codecs::tiff::TiffEncoder::new(&mut out);
            image::ImageEncoder::write_image(encoder, &rgba, width, height, image::ExtendedColorType::Rgba8)
                .map_err(|e| e.to_string())
        }
    }
}

/// The new name for `path` under `pattern`: `{name}` the old stem, `{n}` the
/// zero-padded number.
fn rename_name(pattern: &str, number: usize, width: usize, path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();
    let number = format!("{number:0width$}");
    let base = pattern
        .replace("{name}", &stem)
        .replace("{n}", &number);
    if ext.is_empty() {
        base
    } else {
        format!("{base}.{ext}")
    }
}

/// Rename every image in `files`, two-phase so renames never collide with each
/// other, and report what happened.
fn rename_images(files: &[PathBuf], pattern: &str, start: usize) -> String {
    let images: Vec<PathBuf> = files.iter().filter(|p| is_image(p)).cloned().collect();
    if images.is_empty() {
        return "no images to rename".to_string();
    }
    let width = images.len().to_string().len();
    let dir = images[0].parent().map(|p| p.to_path_buf()).unwrap_or_default();

    let mut staged = Vec::new();
    for (i, path) in images.iter().enumerate() {
        let temp = dir.join(format!(".peepin-rename-{i}"));
        if let Err(err) = std::fs::rename(path, &temp) {
            // Put back whatever moved already, then report.
            for (moved_from, moved_to) in staged.iter().rev() {
                let _ = std::fs::rename(moved_to, moved_from);
            }
            return format!("stopped: {} ({err})", path.display());
        }
        staged.push((path.clone(), temp));
    }

    let mut done = 0;
    for (i, (original, temp)) in staged.iter().enumerate() {
        let target = dir.join(rename_name(pattern, start + i, width, original));
        if target.exists() {
            let _ = std::fs::rename(temp, original);
            continue;
        }
        match std::fs::rename(temp, &target) {
            Ok(()) => done += 1,
            Err(_) => {
                let _ = std::fs::rename(temp, original);
            }
        }
    }
    format!("{done} of {} renamed", staged.len())
}

/// The font used for the name under each polaroid: the first of these that is on
/// disk and parses. Read once and kept.
fn sheet_font() -> Option<&'static ab_glyph::FontVec> {
    static FONT: std::sync::OnceLock<Option<ab_glyph::FontVec>> = std::sync::OnceLock::new();
    FONT.get_or_init(|| {
        for path in [
            "/usr/share/fonts/TTF/CascadiaCode.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/liberation/LiberationMono-Regular.ttf",
            "/usr/share/fonts/TTF/AdwaitaMono-Regular.ttf",
            "/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf",
        ] {
            if let Ok(bytes) = std::fs::read(path) {
                if let Ok(font) = ab_glyph::FontVec::try_from_vec(bytes) {
                    return Some(font);
                }
            }
        }
        None
    })
    .as_ref()
}

/// Break `text` into at most `max_lines` lines that fit `max_width`, adding an
/// ellipsis when it does not all fit.
fn wrap_text(
    font: &ab_glyph::FontVec,
    px: f32,
    max_width: f32,
    max_lines: usize,
    text: &str,
) -> Vec<String> {
    use ab_glyph::{Font as _, ScaleFont as _};
    let scaled = font.as_scaled(px);
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut width = 0.0f32;
    for ch in text.chars() {
        let advance = scaled.h_advance(scaled.scaled_glyph(ch).id);
        if width + advance > max_width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            width = 0.0;
            if lines.len() == max_lines {
                let last = lines.last_mut().unwrap();
                while !last.is_empty()
                    && width + scaled.h_advance(scaled.scaled_glyph('…').id) > max_width
                {
                    last.pop();
                }
                last.push('…');
                return lines;
            }
        }
        current.push(ch);
        width += advance;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Blend one line of text into the picture.
fn draw_text(
    image: &mut image::RgbaImage,
    font: &ab_glyph::FontVec,
    x: f32,
    y: f32,
    px: f32,
    text: &str,
    colour: [u8; 3],
) {
    use ab_glyph::{Font as _, ScaleFont as _};
    let scaled = font.as_scaled(px);
    let ascent = scaled.ascent();
    let mut caret = x;
    for ch in text.chars() {
        let mut glyph = scaled.scaled_glyph(ch);
        glyph.position = ab_glyph::point(caret, y + ascent);
        caret += scaled.h_advance(glyph.id);
        if let Some(outline) = scaled.outline_glyph(glyph) {
            let bounds = outline.px_bounds();
            outline.draw(|gx, gy, coverage| {
                let px_x = bounds.min.x as i32 + gx as i32;
                let px_y = bounds.min.y as i32 + gy as i32;
                if px_x < 0 || px_y < 0 {
                    return;
                }
                let (px_x, px_y) = (px_x as u32, px_y as u32);
                if px_x >= image.width() || px_y >= image.height() {
                    return;
                }
                let pixel = image.get_pixel_mut(px_x, px_y);
                let alpha = coverage.clamp(0.0, 1.0);
                for i in 0..3 {
                    pixel.0[i] = (pixel.0[i] as f32 * (1.0 - alpha) + colour[i] as f32 * alpha) as u8;
                }
                pixel.0[3] = 255;
            });
        }
    }
}

/// A contact sheet: every image in the folder as a polaroid, on a white board.
fn build_sheet(files: &[PathBuf]) -> Option<(egui::ColorImage, usize)> {
    let images: Vec<&PathBuf> = files.iter().filter(|p| is_image(p)).collect();
    if images.is_empty() {
        return None;
    }
    let images: Vec<&PathBuf> = images.into_iter().take(96).collect();
    let count = images.len();

    // A polaroid: 264x198 picture area, 16 around it, 44 below.
    const PICTURE_W: u32 = 264;
    const PICTURE_H: u32 = 198;
    const MARGIN: u32 = 16;
    const BOTTOM: u32 = 44;
    let cell_w = PICTURE_W + MARGIN * 2;
    let cell_h = PICTURE_H + MARGIN + BOTTOM;

    let cols = (count as f64).sqrt().ceil().max(1.0) as u32;
    let cols = cols.min(6);
    let rows = ((count as u32) + cols - 1) / cols;
    let gap = 18;
    let width = cols * cell_w + gap * (cols + 1);
    let height = rows * cell_h + gap * (rows + 1);

    let mut sheet = image::RgbaImage::from_pixel(width, height, image::Rgba([236, 233, 226, 255]));

    for (i, path) in images.iter().enumerate() {
        let Ok(picture) = load_image(path) else {
            continue;
        };
        let rgba: Vec<u8> = picture
            .pixels
            .iter()
            .flat_map(|p| [p.r(), p.g(), p.b(), p.a()])
            .collect();
        let Some(source) = image::RgbaImage::from_raw(picture.width() as u32, picture.height() as u32, rgba) else {
            continue;
        };
        // Keep the picture's own proportions and centre it on the card: stretching
        // it to the frame would not look like a print in a polaroid.
        let scale = (PICTURE_W as f32 / source.width() as f32)
            .min(PICTURE_H as f32 / source.height() as f32);
        let thumb_w = ((source.width() as f32 * scale).round() as u32).max(1);
        let thumb_h = ((source.height() as f32 * scale).round() as u32).max(1);
        let thumb = image::imageops::resize(
            &source,
            thumb_w,
            thumb_h,
            image::imageops::FilterType::Triangle,
        );

        let col = (i as u32) % cols;
        let row = (i as u32) / cols;
        let x = gap + col * (cell_w + gap);
        let y = gap + row * (cell_h + gap);

        // The white polaroid, then the picture on it.
        let card = image::RgbaImage::from_pixel(cell_w, cell_h, image::Rgba([252, 251, 248, 255]));
        image::imageops::overlay(&mut sheet, &card, i64::from(x), i64::from(y));
        image::imageops::overlay(
            &mut sheet,
            &thumb,
            i64::from(x + MARGIN + (PICTURE_W - thumb_w) / 2),
            i64::from(y + MARGIN + (PICTURE_H - thumb_h) / 2),
        );

        // The name, soft-wrapped into the polaroid's bottom band.
        if let Some(font) = sheet_font() {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let px = 13.0;
            let lines = wrap_text(font, px, (PICTURE_W + 2 * MARGIN) as f32 - 8.0, 2, &name);
            for (line_no, line) in lines.iter().enumerate() {
                draw_text(
                    &mut sheet,
                    font,
                    (x + MARGIN + 2) as f32,
                    (y + MARGIN + PICTURE_H + 6) as f32 + line_no as f32 * (px + 2.0),
                    px,
                    line,
                    [58, 55, 52],
                );
            }
        }
    }

    let color = egui::ColorImage::from_rgba_unmultiplied(
        [sheet.width() as usize, sheet.height() as usize],
        sheet.as_raw(),
    );
    Some((color, count))
}

/// `text` cut to fit `max_width`, with an ellipsis where it was cut. The
/// measurement is the painter's own, so a name and the rest of a status line
/// together stay inside the window whatever the font or the scale.
fn fit_text(
    painter: &egui::Painter,
    text: &str,
    max_width: f32,
    font: FontId,
    colour: Color32,
) -> String {
    let width = |candidate: &str| {
        painter
            .layout_no_wrap(candidate.to_string(), font.clone(), colour)
            .size()
            .x
    };
    if width(text) <= max_width {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut keep = 0;
    let mut low = 0;
    let mut high = chars.len();
    while low <= high {
        let mid = (low + high) / 2;
        let mut candidate: String = chars[..mid].iter().collect();
        candidate.push('…');
        if width(&candidate) <= max_width {
            keep = mid;
            low = mid + 1;
        } else if mid == 0 {
            break;
        } else {
            high = mid - 1;
        }
    }
    let mut out: String = chars[..keep].iter().collect();
    out.push('…');
    out
}

fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let tail: String = text
        .chars()
        .rev()
        .take(max.saturating_sub(3))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("...{tail}")
}

impl eframe::App for Peepin {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = &ui.ctx().clone();

        // A contact sheet built in the background lands here when it is ready.
        if let Some(rx) = self.sheet_job.as_ref() {
            match rx.try_recv() {
                Ok((image, count)) => {
                    let texture =
                        ctx.load_texture("sheet", image.clone(), egui::TextureOptions::LINEAR);
                    self.sheet = Some(Sheet { texture, image, count });
                    self.sheet_job = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.sheet_job = None;
                }
            }
        }

        // The window is on screen now, so the child window can be made and mpv
        // told where to draw.
        if let Some((at, path)) = self.screenshot_at.clone() {
            if at.elapsed() > Duration::from_millis(1200) {
                self.screenshot_at = None;
                if let Some(written) = self.screenshot(Some(path)) {
                    eprintln!("screenshot: {}", written.display());
                }
            }
        }
        if self.pending_video.is_some() {
            self.start_pending_video(ctx);
        }
        self.poll_video(ctx);

        let quit = ctx.input(|i| {
            (i.modifiers.command || i.modifiers.ctrl) && i.key_pressed(egui::Key::Q)
        });
        if quit {
            ctx.send_viewport_cmd(ViewportCommand::Close);
            return;
        }

        let (f1, f2, f3, f11, ctrl_s) = ctx.input(|i| {
            let ctrl = i.modifiers.command || i.modifiers.ctrl;
            (
                i.key_pressed(egui::Key::F1),
                i.key_pressed(egui::Key::F2),
                i.key_pressed(egui::Key::F3),
                i.key_pressed(egui::Key::F11),
                ctrl && i.key_pressed(egui::Key::S),
            )
        });

        if self.overlay.is_some() {
            self.overlay_keys(ctx);
        } else if f3 {
            self.toggle_sheet(ctx);
        } else if f11 {
            self.open_rename();
            if let Some(Overlay::Rename(_)) = self.overlay {
                // opened above
            }
        } else if f1 {
            self.overlay = Some(Overlay::Help);
        } else if f2 {
            self.open_settings();
        } else if ctrl_s {
            self.open_save();
        }

        if self.overlay.is_some() {
            self.draw_overlay(ui);
            ctx.request_repaint_after(std::time::Duration::from_millis(60));
            return;
        }

        if self.picker.is_some() {
            self.picker_keys(ctx);
            self.draw_picker(ui);
            ctx.request_repaint_after(std::time::Duration::from_millis(60));
            return;
        }

        let (left, right, up, down, open, wheel, clicked) = ctx.input(|i| {
            let plain = !(i.modifiers.command || i.modifiers.ctrl);
            (
                plain && i.key_pressed(egui::Key::ArrowLeft),
                plain && i.key_pressed(egui::Key::ArrowRight),
                plain && i.key_pressed(egui::Key::ArrowUp),
                plain && i.key_pressed(egui::Key::ArrowDown),
                (i.modifiers.command || i.modifiers.ctrl) && i.key_pressed(egui::Key::O),
                i.smooth_scroll_delta.y,
                (
                    i.pointer.button_clicked(egui::PointerButton::Primary),
                    i.pointer.button_clicked(egui::PointerButton::Secondary),
                ),
            )
        });

        if open {
            self.open_picker();
            return;
        }
        if left {
            self.step(ctx, false);
        }
        if right {
            self.step(ctx, true);
        }
        if self.video.is_some() {
            // A video has nothing to zoom: the arrows and the wheel are the
            // volume, and the bar's own keys live in `video_keys`.
            self.video_keys(ctx);
        } else {
            if up {
                self.zoom_by(1.25);
            }
            if down {
                self.zoom_by(0.8);
            }
            if wheel.abs() > 0.5 {
                self.zoom_by(if wheel > 0.0 { 1.15 } else { 1.0 / 1.15 });
            }
        }
        // The left button both steps and pans: a click (no movement) goes to the
        // next file, a drag moves the zoomed picture.
        let (primary_down, primary_released, pointer) = ctx.input(|i| {
            (
                i.pointer.primary_down(),
                i.pointer.primary_released(),
                i.pointer.interact_pos(),
            )
        });
        let over_bar = self.embedded
            && ctx
                .input(|i| i.pointer.interact_pos())
                .map(|p| p.y > ctx.content_rect().bottom() - VIDEO_BAR)
                .unwrap_or(false);
        if over_bar {
            self.left_drag = None;
        } else if primary_down {
            match self.left_drag {
                None => {
                    self.left_drag = Some((pointer.unwrap_or_default(), self.pan, false));
                }
                Some((start, from, moved)) => {
                    if let Some(now) = pointer {
                        let delta = now - start;
                        let moved = moved || delta.length() > 4.0;
                        // Only a zoomed picture is dragged: at "fit" there is
                        // nothing outside the window to move to.
                        if moved && self.zoom > 1.0 {
                            self.pan = from + delta;
                        }
                        self.left_drag = Some((start, from, moved));
                    }
                }
            }
        } else if primary_released {
            if let Some((_, _, moved)) = self.left_drag.take() {
                if !moved {
                    self.step(ctx, self.left_button == Step::Next);
                }
            }
        }
        if clicked.1 && !over_bar {
            self.step(ctx, self.right_button == Step::Next);
        }

        // Pan with the middle button, so the left/right buttons stay for stepping.
        let middle = ctx.input(|i| i.pointer.middle_down());
        if middle {
            let pointer = ctx.input(|i| i.pointer.interact_pos());
            if let (Some(now), Some((start, from))) = (pointer, self.drag) {
                self.pan = from + (now - start);
            } else if let Some(now) = pointer {
                self.drag = Some((now, self.pan));
            }
        } else {
            self.drag = None;
        }

        // Ctrl+arrows resize the window itself. A floating XWayland window cannot
        // always be dragged by its edges under niri, so peepin offers keys for it.
        let resize = ctx.input(|i| {
            let mut size = Vec2::ZERO;
            if i.modifiers.command || i.modifiers.ctrl {
                let step = 40.0;
                if i.key_pressed(egui::Key::ArrowRight) {
                    size.x += step;
                }
                if i.key_pressed(egui::Key::ArrowLeft) {
                    size.x -= step;
                }
                if i.key_pressed(egui::Key::ArrowDown) {
                    size.y += step;
                }
                if i.key_pressed(egui::Key::ArrowUp) {
                    size.y -= step;
                }
            }
            size
        });
        if resize != Vec2::ZERO {
            if let Some(rect) = ctx.input(|i| i.viewport().inner_rect) {
                let size = (rect.size() + resize).max(Vec2::new(240.0, 160.0));
                ctx.send_viewport_cmd(ViewportCommand::InnerSize(size));
            }
        }

        let plain_p = ctx.input(|i| {
            !(i.modifiers.command || i.modifiers.ctrl) && i.key_pressed(egui::Key::P)
        });
        if plain_p {
            self.screenshot(None);
        }
        if ctx.input(|i| i.key_pressed(egui::Key::B)) {
            self.background = match self.background {
                Background::Transparent => Background::Blur,
                Background::Blur => Background::Solid,
                Background::Solid => Background::Transparent,
            };
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Num0)) {
            self.zoom = 1.0;
            self.pan = Vec2::ZERO;
        }

        if self.embedded {
            // mpv paints in its own child window; what is left to us is the bar
            // underneath it.
            self.video_bar(ui);
            ctx.request_repaint_after(std::time::Duration::from_millis(60));
            return;
        }
        self.draw_background(ui);
        if self.sheet_job.is_some() {
            self.busy(ui, "building contact sheet…");
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
            return;
        }
        if self.sheet.is_some() {
            self.draw_sheet(ui);
            self.sheet_status(ui);
        } else {
            self.draw_image(ui);
            self.status_bar(ui);
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(60));
    }
}

impl Drop for Peepin {
    fn drop(&mut self) {
        self.stop_video();
    }
}

/// The X11 window id of our own window, when winit created it through XWayland.
/// The icon for the task bar and the window list. The picture is compiled into
/// the binary, so an installed peepin still carries it.
fn window_icon() -> Option<egui::IconData> {
    let bytes = include_bytes!("../assets/peep.png");
    let image = image::load_from_memory(bytes).ok()?.to_rgba8();
    // 128 square: one `XChangeProperty` can carry at most 262140 bytes, and a
    // 256 square icon is 262144.
    let size = 128u32;
    let small = image::imageops::resize(
        &image,
        size,
        size,
        image::imageops::FilterType::Lanczos3,
    );
    Some(egui::IconData {
        rgba: small.into_raw(),
        width: size,
        height: size,
    })
}

/// Put the icon on our own X11 window. winit offers it through
/// `ViewportBuilder::with_icon`, but the property comes out empty under
/// XWayland, so it is set here as well — the same `_NET_WM_ICON` the task bar
/// and the window list read.
fn set_window_icon(window: u64, icon: &egui::IconData) {
    let Ok(xlib) = x11_dl::xlib::Xlib::open() else {
        return;
    };
    unsafe {
        let display = (xlib.XOpenDisplay)(std::ptr::null());
        if display.is_null() {
            return;
        }
        let atom = (xlib.XInternAtom)(display, c"_NET_WM_ICON".as_ptr(), 0);
        let mut data: Vec<u64> = Vec::with_capacity(2 + icon.rgba.len() / 4);
        data.push(icon.width as u64);
        data.push(icon.height as u64);
        for pixel in icon.rgba.chunks_exact(4) {
            data.push(
                (pixel[3] as u64) << 24
                    | (pixel[0] as u64) << 16
                    | (pixel[1] as u64) << 8
                    | pixel[2] as u64,
            );
        }
        (xlib.XChangeProperty)(
            display,
            window as x11_dl::xlib::Window,
            atom,
            x11_dl::xlib::XA_CARDINAL,
            32,
            x11_dl::xlib::PropModeReplace,
            data.as_ptr() as *const u8,
            data.len() as i32,
        );
        (xlib.XFlush)(display);
        (xlib.XCloseDisplay)(display);
    }
}

fn x11_window_id(cc: &eframe::CreationContext<'_>) -> Option<u64> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = cc.window_handle().ok()?;
    match handle.as_raw() {
        RawWindowHandle::Xlib(h) => Some(h.window as u64),
        RawWindowHandle::Xcb(h) => Some(h.window.get() as u64),
        _ => None,
    }
}

fn config_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".config")
        });
    base.join("peepin").join("peepin.conf")
}

/// The files of a folder in name order, and the index of `path` in it.
fn scan(path: &Path) -> (Vec<PathBuf>, usize) {
    if path.is_file() {
        let dir = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
        let mut files = media_in(&dir);
        let index = files.iter().position(|p| p == path).unwrap_or(0);
        if files.is_empty() {
            files.push(path.to_path_buf());
        }
        return (files, index);
    }
    (media_in(path), 0)
}

fn media_in(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|read| {
            read.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file() && (is_image(p) || is_video(p)))
                .collect()
        })
        .unwrap_or_default();
    files.sort_by(|a, b| {
        a.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase()
            .cmp(
                &b.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_lowercase(),
            )
    });
    files
}

/// A tiny copy of the picture, blurred: the window backdrop in `Background::Blur`.
fn blur_version(image: &egui::ColorImage) -> egui::ColorImage {
    let width = 48u32;
    let height = ((image.height() as f32 / image.width() as f32) * width as f32).max(1.0) as u32;
    let mut small = image::RgbaImage::new(width, height);
    let step_x = image.width() as f32 / width as f32;
    let step_y = image.height() as f32 / height as f32;
    for y in 0..height {
        for x in 0..width {
            let px = (x as f32 * step_x) as usize;
            let py = (y as f32 * step_y) as usize;
            let px = px.min(image.width() - 1);
            let py = py.min(image.height() - 1);
            let p = image.pixels[py * image.width() + px];
            small.put_pixel(x, y, image::Rgba([p.r(), p.g(), p.b(), p.a()]));
        }
    }
    let blurred = image::imageops::blur(&small, 1.6);
    egui::ColorImage::from_rgba_unmultiplied(
        [blurred.width() as usize, blurred.height() as usize],
        blurred.as_raw(),
    )
}

/// Formats `image` does not read are handed to a converter that does — the same
/// idea as IrfanView's plugins, using the tools already on the machine. The
/// result comes back as a temporary PNG.
fn decode_with_tool(path: &Path) -> Option<egui::ColorImage> {
    use std::process::Command;
    let out = std::env::temp_dir().join(format!("peepin-decode-{}.png", std::process::id()));
    let _ = std::fs::remove_file(&out);

    let mut attempts: Vec<Command> = Vec::new();

    let mut magick = Command::new("magick");
    magick
        .arg(format!("{}[0]", path.display()))
        .arg("-auto-orient")
        .arg("-depth")
        .arg("8")
        .arg(&out);
    attempts.push(magick);

    let mut gm = Command::new("gm");
    gm.arg("convert").arg(path).arg(&out);
    attempts.push(gm);

    let mut heif = Command::new("heif-convert");
    heif.arg(path).arg(&out);
    attempts.push(heif);

    let mut djxl = Command::new("djxl");
    djxl.arg(path).arg(&out);
    attempts.push(djxl);

    let mut vips = Command::new("vips");
    vips.arg("copy").arg(path).arg(&out);
    attempts.push(vips);

    for mut attempt in attempts {
        let ok = attempt
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if ok && out.exists() {
            if let Ok(image) = image::open(&out) {
                let rgba = image.to_rgba8();
                let size = [rgba.width() as usize, rgba.height() as usize];
                let _ = std::fs::remove_file(&out);
                return Some(egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()));
            }
        }
        let _ = std::fs::remove_file(&out);
    }
    None
}

fn load_image(path: &Path) -> Result<egui::ColorImage, String> {
    if !has_ext(path, &["dds"]) && !image_ext_known(path) {
        if let Some(image) = decode_with_tool(path) {
            return Ok(image);
        }
    }
    let rgba = if has_ext(path, &["dds"]) {
        // Block-compressed surfaces (BC1-BC7) come through image_dds; anything
        // else falls back to `image`'s own DDS reader.
        let decoded = (|| {
            let file = std::fs::File::open(path).ok()?;
            let mut reader = std::io::BufReader::new(file);
            let dds = ddsfile::Dds::read(&mut reader).ok()?;
            image_dds::image_from_dds(&dds, 0).ok()
        })();
        match decoded {
            Some(image) => image,
            None => image::open(path)
                .map_err(|e| format!("dds: {e}"))?
                .to_rgba8(),
        }
    } else {
        match image::open(path) {
            Ok(image) => image.to_rgba8(),
            Err(native_error) => match decode_with_tool(path) {
                Some(image) => return Ok(image),
                None => return Err(native_error.to_string()),
            },
        }
    };
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok(egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

/// The extensions `image` itself decodes; anything else goes to a tool first.
fn image_ext_known(path: &Path) -> bool {
    has_ext(
        path,
        &[
            "png", "jpg", "jpeg", "gif", "bmp", "tga", "tif", "tiff", "webp", "qoi", "hdr",
            "exr", "dds", "ico", "pnm", "ff", "farbfeld", "avif",
        ],
    )
}

/// Exercise the code that needs no window: every save format, and the sheet.
fn selftest() {
    let arg = std::env::args().skip(1).find(|a| !a.starts_with("--"));
    let Some(path) = arg.map(PathBuf::from) else {
        println!("usage: peepin --selftest <file>");
        return;
    };
    match load_image(&path) {
        Ok(image) => {
            println!("loaded {} ({}x{})", path.display(), image.width(), image.height());
            for format in [Format::Png, Format::Jpeg, Format::Webp, Format::Bmp, Format::Tiff] {
                let out = std::path::PathBuf::from(format!("/tmp/opencode/peepin-test.{}", format.ext()));
                match write_image(&image, &out, format) {
                    Ok(()) => {
                        let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
                        println!("  {} -> {} ({size} bytes)", format.ext(), out.display());
                    }
                    Err(err) => println!("  {} failed: {err}", format.ext()),
                }
            }
        }
        Err(err) => println!("load failed: {err}"),
    }
    let dir = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let files = media_in(&dir);
    match build_sheet(&files) {
        Some((sheet, count)) => {
            let out = std::path::PathBuf::from("/tmp/opencode/peepin-sheet.png");
            match write_image(&sheet, &out, Format::Png) {
                Ok(()) => println!(
                    "  sheet of {count} image(s) -> {} ({}x{})",
                    out.display(),
                    sheet.width(),
                    sheet.height()
                ),
                Err(err) => println!("  sheet save failed: {err}"),
            }
        }
        None => println!("  no sheet (no images in {})", dir.display()),
    }

    // Rename, in a throwaway copy so the real folder is untouched.
    let scratch = std::path::PathBuf::from("/tmp/opencode/peepin-rename");
    let _ = std::fs::remove_dir_all(&scratch);
    if std::fs::create_dir_all(&scratch).is_ok() {
        let mut copies = Vec::new();
        for (i, file) in files.iter().take(4).enumerate() {
            let target = scratch.join(format!("copy-{i}.png"));
            if std::fs::copy(file, &target).is_ok() {
                copies.push(target);
            }
        }
        if !copies.is_empty() {
            let result = rename_images(&copies, "photo-{n}", 1);
            let mut names: Vec<String> = std::fs::read_dir(&scratch)
                .map(|read| {
                    read.flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            println!("  rename: {result} -> {names:?}");
        }
    }
}

fn main() -> eframe::Result<()> {
    if std::env::args().any(|a| a == "--selftest") {
        selftest();
        return Ok(());
    }
    if std::env::args().any(|a| a == "--palette") {
        Peepin::palette_for_debug();
        return Ok(());
    }
    let picker_at_start = std::env::args().any(|a| a == "--picker");
    clean_old_mpv_sockets();
    let previews_at_start = std::env::args().any(|a| a == "--previews");
    let settings_at_start = std::env::args().any(|a| a == "--settings");
    let sheet_at_start = std::env::args().any(|a| a == "--sheet");
    let help_at_start = std::env::args().any(|a| a == "--help-overlay");
    let save_at_start = std::env::args().any(|a| a == "--save-overlay");
    let rename_at_start = std::env::args().any(|a| a == "--rename-overlay");
    let arg = std::env::args()
        .skip(1)
        .find(|a| !a.starts_with("--"))
        .map(PathBuf::from);
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("peepin")
        .with_inner_size([960.0, 640.0])
        .with_transparent(true)
        .with_min_inner_size([240.0, 160.0]);
    if let Some(icon) = window_icon() {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }
    let mut options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    // XWayland, so the window has an X11 id: that is what mpv needs to draw its
    // video inside peepin instead of opening a window of its own.
    options.event_loop_builder = Some(Box::new(|builder| {
        use winit::platform::x11::EventLoopBuilderExtX11;
        builder.with_x11();
    }));
    eframe::run_native(
        "peepin",
        options,
        Box::new(move |cc| {
            let mut app = Peepin::new(
                cc,
                arg.clone(),
                picker_at_start,
                settings_at_start,
                sheet_at_start,
                previews_at_start,
            );
            if help_at_start {
                app.overlay = Some(Overlay::Help);
            }
            if let Some(seconds) = std::env::args()
                .find_map(|a| a.strip_prefix("--seek=").and_then(|v| v.parse::<f64>().ok()))
            {
                app.seek_at_start = Some(seconds);
            }
            if let Some(path) = std::env::args()
                .find_map(|a| a.strip_prefix("--screenshot=").map(|p| p.to_string()))
            {
                app.screenshot_at = Some((Instant::now(), PathBuf::from(path)));
            }
            if std::env::args().any(|a| a == "--sub-load") {
                app.sub_load_at_start = true;
            }
            if let Some(steps) = std::env::args()
                .find_map(|a| a.strip_prefix("--sub-cycle=").and_then(|v| v.parse::<usize>().ok()))
            {
                app.sub_cycle_at_start = steps;
            }
            if let Some(number) = std::env::args()
                .find_map(|a| a.strip_prefix("--sub-pick=").and_then(|v| v.parse::<usize>().ok()))
            {
                app.sub_pick_at_start = Some(number);
            }
            if std::env::args().any(|a| a == "--sub-menu") {
                app.sub_menu_at_start = true;
            }
            if save_at_start {
                app.open_save();
            }
            if rename_at_start {
                app.open_rename();
            }
            Ok(Box::new(app))
        }),
    )
}
