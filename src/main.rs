//! peepin — a tiny image and video viewer with a terminal feel.
//!
//! The window has no background of its own: the picture sits on whatever is behind
//! it, and the menu, status line and picker take their colours from the terminal
//! (`~/.config/kitty/kitty.conf`, with `peepin.conf` able to override them).
//! Left/right (or the mouse's left/right button) walk the folder, the wheel or
//! up/down zoom, Ctrl+O opens the picker and Ctrl+Q quits. Videos are handed to
//! mpv, which draws inside this window; everything else goes through `image`,
//! with `.dds` (Fallout's textures) through `image_dds`.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use eframe::egui;
use egui::{
    Align2, Color32, FontFamily, FontId, Pos2, Rect, Stroke, StrokeKind, Vec2, ViewportCommand,
};

/// Everything `image` can open out of the box.
const IMAGE_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "tga", "tif", "tiff", "webp", "qoi", "hdr", "exr",
    "dds", "ico", "pnm", "ff", "farbfeld",
];

/// Handed to mpv.
const VIDEO_EXTS: &[&str] = &[
    "mp4", "mkv", "webm", "avi", "mov", "m4v", "wmv", "flv", "mpg", "mpeg", "ts", "ogv",
];

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
    left_button: Step,
    right_button: Step,
    drag: Option<(Pos2, Vec2)>,
    /// X11 window id, when the window could be created through XWayland. mpv
    /// draws its video into that window with `--wid`.
    window_id: Option<u64>,
    /// True while mpv owns the window: peepin then only listens for keys.
    embedded: bool,
    /// A video waiting for the window to be mapped.
    pending_video: Option<PathBuf>,
    palette: Palette,
    background: Background,
    /// A small, blurred copy of the current picture, stretched over the window in
    /// `Background::Blur`.
    blur: Option<egui::TextureHandle>,
    /// The F1 panel, when it is open.
    settings: Option<Settings>,
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
            left_button: Step::Previous,
            right_button: Step::Next,
            drag: None,
            window_id: x11_window_id(cc),
            embedded: false,
            pending_video: None,
            palette,
            background: Background::Transparent,
            blur: None,
            settings: None,
        };
        app.read_config();
        app.load(&cc.egui_ctx);
        if open_picker_now {
            app.open_picker();
        }
        if open_settings_now {
            app.open_settings();
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
        self.settings = Some(Settings::from(self));
    }

    fn save_settings(&mut self, ctx: &egui::Context) {
        let Some(settings) = self.settings.as_ref() else {
            return;
        };
        let text = format!(
            "# peepin — written by the F1 panel\n\
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
        match std::fs::write(&path, text) {
            Ok(()) => {
                self.background = settings.background;
                self.left_button = settings.left;
                self.right_button = settings.right;
                self.palette = Palette::load();
                self.apply_palette(ctx);
                if let Some(settings) = self.settings.as_mut() {
                    settings.note = "saved".to_string();
                }
            }
            Err(err) => {
                if let Some(settings) = self.settings.as_mut() {
                    settings.note = format!("could not write {}: {err}", path.display());
                }
            }
        }
    }

    fn settings_keys(&mut self, ctx: &egui::Context) {
        let close = ctx.input(|i| {
            i.key_pressed(egui::Key::Escape)
                || i.key_pressed(egui::Key::F1)
                || ((i.modifiers.command || i.modifiers.ctrl) && i.key_pressed(egui::Key::S))
        });
        if close {
            let save = ctx.input(|i| i.modifiers.command || i.modifiers.ctrl);
            if save {
                self.save_settings(ctx);
            }
            self.settings = None;
        }
    }

    fn draw_settings(&mut self, ui: &mut egui::Ui) {
        let Some(settings) = self.settings.as_mut() else {
            return;
        };
        let palette = self.palette;
        let screen = ui.max_rect();
        let size = Vec2::new(screen.width().min(560.0), screen.height().min(320.0));
        let rect = Rect::from_min_size(Pos2::new(screen.left() + 12.0, screen.top() + 12.0), size);
        let mut panel = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );
        panel.painter().rect_filled(rect, 2.0, palette.bg.gamma_multiply(0.96));
        panel.painter().rect_stroke(rect, 2.0, Stroke::new(1.0, palette.dim), StrokeKind::Inside);

        let mut save_clicked = false;
        let mut close_clicked = false;

        egui::Frame::new().inner_margin(8.0).show(&mut panel, |ui| {
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
                    if ui
                        .add(
                            egui::Button::new(egui::RichText::new(label).color(if picked {
                                palette.bg
                            } else {
                                palette.fg
                            }))
                            .fill(if picked { palette.accent } else { Color32::TRANSPARENT }),
                        )
                        .clicked()
                    {
                        settings.background = mode;
                    }
                }
            });

            for (label, slot) in [("mouse left", 0), ("mouse right", 1)] {
                ui.horizontal(|ui| {
                    ui.label(label);
                    for (text, step) in [("prev", Step::Previous), ("next", Step::Next)] {
                        let current = if slot == 0 { settings.left } else { settings.right };
                        let picked = current == step;
                        if ui
                            .add(
                                egui::Button::new(egui::RichText::new(text).color(if picked {
                                    palette.bg
                                } else {
                                    palette.fg
                                }))
                                .fill(if picked { palette.accent } else { Color32::TRANSPARENT }),
                            )
                            .clicked()
                        {
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
                    save_clicked = true;
                }
                if ui.button("close  (esc)").clicked() {
                    close_clicked = true;
                }
                if !settings.note.is_empty() {
                    ui.label(egui::RichText::new(&settings.note).color(palette.dim));
                }
            });
            ui.label(
                egui::RichText::new("colours as #rrggbb; the terminal's own are read at start")
                    .color(palette.dim),
            );
        });

        if save_clicked {
            self.save_settings(ui.ctx());
        }
        if close_clicked {
            self.settings = None;
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

    fn start_pending_video(&mut self) {
        let Some(path) = self.pending_video.take() else {
            return;
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut cmd = Command::new("mpv");
        // X11 explicitly: with WAYLAND_DISPLAY set, mpv would pick Wayland,
        // where `--wid` does not exist and it opens a window of its own.
        cmd.env_remove("WAYLAND_DISPLAY")
            .arg("--gpu-context=x11egl")
            .arg("--really-quiet")
            .arg("--no-terminal")
            .arg("--loop-file=inf")
            .arg("--no-osc")
            .arg("--no-input-default-bindings")
            .arg("--no-osd-bar");
        let embedded = match self.window_id {
            Some(id) => {
                cmd.arg(format!("--wid={id}"));
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
                self.status = if embedded {
                    format!("▶ {name}  (mpv, embedded)")
                } else {
                    format!("▶ {name}  (mpv)")
                };
            }
            Err(err) => self.status = format!("mpv: {err}"),
        }
    }

    fn stop_video(&mut self) {
        self.pending_video = None;
        self.embedded = false;
        if let Some(mut child) = self.video.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
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
        if up || down {
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
        let Some(pick) = self.picker.as_ref() else {
            return;
        };
        let palette = self.palette;
        let mut folder_click: Option<PathBuf> = None;
        let mut file_click: Option<PathBuf> = None;

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
                            ui.label(egui::RichText::new("files").color(palette.dim));
                            egui::ScrollArea::vertical()
                                .id_salt("files")
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    for (i, (name, path)) in pick.files.entries.iter().enumerate() {
                                        let picked = !pick.on_folders && i == pick.files.selected;
                                        if pane_row(ui, name, picked, palette) {
                                            file_click = Some(path.clone());
                                        }
                                    }
                                    if pick.files.entries.is_empty() {
                                        ui.label(
                                            egui::RichText::new("no images or videos here")
                                                .color(palette.dim),
                                        );
                                    }
                                });
                        },
                    );
                });

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
        let text = format!(
            "{}   {}   zoom {:.0}%   [←/→] next  [↑/↓ wheel] zoom  [b] {}  [ctrl+o] open  [ctrl+q] quit",
            self.files
                .get(self.index)
                .map(|p| p.file_name().unwrap_or_default().to_string_lossy().into_owned())
                .unwrap_or_else(|| "peepin".to_string()),
            if self.files.is_empty() {
                String::new()
            } else {
                format!("{}/{}", self.index + 1, self.files.len())
            },
            self.zoom * 100.0,
            match self.background {
                Background::Transparent => "clear",
                Background::Blur => "blur",
                Background::Solid => "solid",
            }
        );
        let painter = ui.painter();
        let rect = ui.max_rect();
        let bar = Rect::from_min_max(
            Pos2::new(rect.left() + 6.0, rect.bottom() - 20.0),
            Pos2::new(rect.right() - 6.0, rect.bottom() - 2.0),
        );
        painter.rect_filled(bar.expand2(Vec2::new(2.0, 1.0)), 2.0, palette.bg.gamma_multiply(0.7));
        painter.text(
            Pos2::new(bar.left(), bar.top()),
            Align2::LEFT_TOP,
            text,
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

        // The window is on screen now, so mpv can attach to it.
        if self.pending_video.is_some() {
            self.start_pending_video();
        }

        let quit = ctx.input(|i| {
            (i.modifiers.command || i.modifiers.ctrl) && i.key_pressed(egui::Key::Q)
        });
        if quit {
            ctx.send_viewport_cmd(ViewportCommand::Close);
            return;
        }

        if ctx.input(|i| i.key_pressed(egui::Key::F1)) {
            if self.settings.is_some() {
                self.settings = None;
            } else {
                self.open_settings();
            }
        }
        if self.settings.is_some() {
            self.settings_keys(ctx);
        }
        if self.settings.is_some() {
            self.draw_settings(ui);
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
        if up {
            self.zoom_by(1.25);
        }
        if down {
            self.zoom_by(0.8);
        }
        if wheel.abs() > 0.5 {
            self.zoom_by(if wheel > 0.0 { 1.15 } else { 1.0 / 1.15 });
        }
        if clicked.0 {
            self.step(ctx, self.left_button == Step::Next);
        }
        if clicked.1 {
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
            // mpv owns the window while a video plays; drawing here would erase it.
            return;
        }
        self.draw_background(ui);
        self.draw_image(ui);
        self.status_bar(ui);

        if !self.embedded {
            ctx.request_repaint_after(std::time::Duration::from_millis(60));
        }
    }
}

impl Drop for Peepin {
    fn drop(&mut self) {
        self.stop_video();
    }
}

/// The X11 window id of our own window, when winit created it through XWayland.
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

fn load_image(path: &Path) -> Result<egui::ColorImage, String> {
    let rgba = if has_ext(path, &["dds"]) {
        let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let mut reader = std::io::BufReader::new(file);
        let dds = ddsfile::Dds::read(&mut reader).map_err(|e| format!("dds: {e}"))?;
        image_dds::image_from_dds(&dds, 0).map_err(|e| format!("dds: {e}"))?
    } else {
        image::open(path).map_err(|e| e.to_string())?.to_rgba8()
    };
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok(egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

fn main() -> eframe::Result<()> {
    if std::env::args().any(|a| a == "--palette") {
        Peepin::palette_for_debug();
        return Ok(());
    }
    let picker_at_start = std::env::args().any(|a| a == "--picker");
    let settings_at_start = std::env::args().any(|a| a == "--settings");
    let arg = std::env::args()
        .skip(1)
        .find(|a| !a.starts_with("--"))
        .map(PathBuf::from);
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("peepin")
            .with_inner_size([960.0, 640.0])
            .with_transparent(true)
            .with_min_inner_size([240.0, 160.0]),
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
            Ok(Box::new(Peepin::new(
                cc,
                arg.clone(),
                picker_at_start,
                settings_at_start,
            )))
        }),
    )
}
