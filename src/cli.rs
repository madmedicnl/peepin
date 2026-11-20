//! The command line: what peepin takes when it is called from a shell, and the
//! jobs it finishes without opening a window.
//!
//! The flags follow feh where the two programs do the same thing (`-F`, `-g`,
//! `-S`, `-r`, `-f`, `-Z`, `-l`, `--info`, `--start-at`, `--title`), and the
//! screenshot mode is the one `scrot` has: `peepin --screenshot [FILE]` writes
//! the screen and exits.

use std::path::PathBuf;

pub const USAGE: &str = "\
peepin — a tiny image and video viewer with a terminal feel

    peepin [OPTIONS] [FILE|FOLDER]

Opens FILE, or the first picture in FOLDER; ← → walk the folder.

  -g, --geometry WxH     the window's size in points
  -F, --fullscreen       start fullscreen
  -S, --sort KEY         folder order: name (default), size, mtime, width, height
  -r, --reverse          walk the folder the other way
      --start-at FILE    start at that file instead of the first
  -f, --filelist FILE    the files to walk, one a line, instead of a folder
      --title TEXT       the window title
  -Z, --zoom             show the picture at 100 % instead of fitting it
  -l, --list             print NAME:WIDTH:HEIGHT:FORMAT:SIZE per picture and exit
      --info             print one line per picture: NAME WxH FORMAT SIZE
      --screenshot [FILE]  take a screenshot of the whole screen and exit
  -u, --window           with --screenshot: the window that has the focus
  -d, --delay SECONDS    with --screenshot: wait before taking it
  -v, --version          print the version
  -h, --help             this text

The screenshot is written as peepin-<date>-<time>.png (never over an older one)
in the current directory, or to FILE; the path it wrote is printed on stdout.
Under Wayland `grim` is asked for the screen when it is installed; otherwise the
X11 window is read with XGetImage, which cannot see native Wayland windows.
";

/// How a folder is ordered.
#[derive(Clone, Copy, PartialEq)]
pub enum Sort {
    Name,
    Size,
    Mtime,
    Width,
    Height,
}

impl Sort {
    fn parse(key: &str) -> Option<Self> {
        Some(match key {
            "name" | "filename" => Self::Name,
            "size" => Self::Size,
            "mtime" | "time" => Self::Mtime,
            "width" => Self::Width,
            "height" => Self::Height,
            _ => return None,
        })
    }
}

/// What the program should do: open the viewer (the default), or one of the
/// jobs that end before a window is made.
#[derive(Clone, PartialEq)]
pub enum Mode {
    Viewer,
    List,
    Info,
    Screenshot { file: Option<PathBuf>, window: bool, delay: f64 },
    Help,
    Version,
}

#[derive(Clone)]
pub struct Options {
    pub path: Option<PathBuf>,
    /// Every file named on the line, in order; the viewer opens the first.
    pub files: Vec<PathBuf>,
    pub filelist: Option<PathBuf>,
    pub geometry: Option<(f32, f32)>,
    pub fullscreen: bool,
    pub sort: Sort,
    pub reverse: bool,
    pub start_at: Option<PathBuf>,
    pub title: Option<String>,
    pub zoom: bool,
    pub mode: Mode,
    // The hooks the checks in the source use to open a panel at start.
    pub picker: bool,
    pub settings: bool,
    pub sheet: bool,
    pub previews: bool,
    pub help_overlay: bool,
    pub save_overlay: bool,
    pub rename_overlay: bool,
    pub seek: Option<f64>,
    pub sub_load: bool,
    pub sub_cycle: usize,
    pub sub_pick: Option<usize>,
    pub sub_menu: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            path: None,
            files: Vec::new(),
            filelist: None,
            geometry: None,
            fullscreen: false,
            sort: Sort::Name,
            reverse: false,
            start_at: None,
            title: None,
            zoom: false,
            mode: Mode::Viewer,
            picker: false,
            settings: false,
            sheet: false,
            previews: false,
            help_overlay: false,
            save_overlay: false,
            rename_overlay: false,
            seek: None,
            sub_load: false,
            sub_cycle: 0,
            sub_pick: None,
            sub_menu: false,
        }
    }
}

/// `1200x700` (a trailing `+X+Y`, which feh allows, is ignored).
fn geometry(text: &str) -> Option<(f32, f32)> {
    let (width, rest) = text.trim().split_once(['x', 'X'])?;
    let height = rest.split('+').next()?;
    let width: f32 = width.trim().parse().ok()?;
    let height: f32 = height.trim().parse().ok()?;
    (width >= 120.0 && height >= 80.0).then_some((width, height))
}

/// Read the command line. `Err` carries what is wrong with it, for the shell.
pub fn parse(args: Vec<String>) -> Result<Options, String> {
    let mut options = Options::default();
    let mut files: Vec<PathBuf> = Vec::new();
    let mut shot_file: Option<PathBuf> = None;
    let mut shot = false;
    let mut window = false;
    let mut delay: f64 = 0.0;
    let mut i = 0;
    let mut only_files = false;
    while i < args.len() {
        let arg = args[i].clone();
        i += 1;
        if only_files {
            files.push(PathBuf::from(arg));
            continue;
        }
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if arg.starts_with("--") => (flag.to_string(), Some(value.to_string())),
            _ => (arg.clone(), None),
        };
        let mut value = |name: &str| -> Result<String, String> {
            if let Some(inline) = inline.clone() {
                return Ok(inline);
            }
            if i < args.len() {
                let next = args[i].clone();
                i += 1;
                return Ok(next);
            }
            Err(format!("{name} needs a value"))
        };
        match flag.as_str() {
            "--" => only_files = true,
            "-h" | "--help" => options.mode = Mode::Help,
            "-v" | "--version" => options.mode = Mode::Version,
            "-F" | "--fullscreen" => options.fullscreen = true,
            "-Z" | "--zoom" => options.zoom = true,
            "-r" | "--reverse" => options.reverse = true,
            "--info" => options.mode = Mode::Info,
            "-l" | "--list" => options.mode = Mode::List,
            "-u" | "--window" => window = true,
            "-d" | "--delay" => {
                delay = value("--delay")?
                    .parse()
                    .map_err(|_| "--delay wants seconds".to_string())?;
            }
            "--screenshot" | "--shot" | "-s" => {
                shot = true;
                if let Some(inline) = inline.clone() {
                    shot_file = Some(PathBuf::from(inline));
                }
            }
            "-g" | "--geometry" => {
                let text = value("--geometry")?;
                options.geometry = Some(
                    geometry(&text).ok_or_else(|| format!("--geometry: {text:?} is not WxH"))?,
                );
            }
            "-S" | "--sort" => {
                let key = value("--sort")?.to_lowercase();
                options.sort =
                    Sort::parse(&key).ok_or_else(|| format!("--sort: {key:?} is not a key"))?;
            }
            "-f" | "--filelist" => options.filelist = Some(PathBuf::from(value("--filelist")?)),
            "--start-at" => options.start_at = Some(PathBuf::from(value("--start-at")?)),
            "--title" => options.title = Some(value("--title")?),
            "--picker" => options.picker = true,
            "--previews" => options.previews = true,
            "--settings" => options.settings = true,
            "--sheet" => options.sheet = true,
            "--help-overlay" => options.help_overlay = true,
            "--save-overlay" => options.save_overlay = true,
            "--rename-overlay" => options.rename_overlay = true,
            "--sub-load" => options.sub_load = true,
            "--sub-menu" => options.sub_menu = true,
            "--seek" => {
                options.seek = Some(value("--seek")?.parse().map_err(|_| "--seek wants seconds".to_string())?)
            }
            "--sub-cycle" => {
                options.sub_cycle = value("--sub-cycle")?.parse().map_err(|_| "--sub-cycle wants a count".to_string())?
            }
            "--sub-pick" => {
                options.sub_pick = Some(value("--sub-pick")?.parse().map_err(|_| "--sub-pick wants a number".to_string())?)
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!("{other} is not a flag peepin knows"));
            }
            _ => files.push(PathBuf::from(arg)),
        }
    }
    options.path = files.first().cloned();
    options.files = files.clone();
    if shot {
        // `peepin --screenshot file.png` and `--screenshot=file.png` both work;
        // a second free argument is the file when the flag came first.
        let file = shot_file.or_else(|| files.first().cloned());
        options.mode = Mode::Screenshot { file, window, delay };
        options.path = None;
    } else if window || delay > 0.0 {
        return Err("--window and --delay belong to --screenshot".to_string());
    }
    Ok(options)
}

/// Run a mode that does not open a window; `None` means "go on to the viewer".
pub fn run(options: &Options) -> Option<i32> {
    match &options.mode {
        Mode::Viewer => None,
        Mode::Help => {
            print!("{USAGE}");
            Some(0)
        }
        Mode::Version => {
            println!("peepin {}", env!("CARGO_PKG_VERSION"));
            Some(0)
        }
        Mode::List => Some(list(options, false)),
        Mode::Info => Some(list(options, true)),
        Mode::Screenshot { file, window, delay } => Some(screenshot(file.clone(), *window, *delay)),
    }
}

/// The files a `--list` or `--info` should describe: the ones named, folders
/// opened up, or the folder of the single path given.
pub fn for_listing(options: &Options) -> Vec<PathBuf> {
    let order = crate::Order {
        kind: match options.sort {
            Sort::Name => crate::SortKind::Name,
            Sort::Size => crate::SortKind::Size,
            Sort::Mtime => crate::SortKind::Mtime,
            Sort::Width => crate::SortKind::Width,
            Sort::Height => crate::SortKind::Height,
        },
        reverse: options.reverse,
    };
    if let Some(list) = &options.filelist {
        return std::fs::read_to_string(list)
            .map(|text| {
                text.lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default();
    }
    if !options.files.is_empty() {
        let mut out = Vec::new();
        for path in &options.files {
            if path.is_dir() {
                out.extend(crate::media_in(path, order));
            } else {
                out.push(path.clone());
            }
        }
        return out;
    }
    match &options.path {
        Some(path) => crate::scan(path, order).0,
        None => Vec::new(),
    }
}

fn describe(path: &std::path::Path) -> Option<(String, u32, u32, String, u64)> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let format = path
        .extension()
        .map(|e| e.to_string_lossy().to_uppercase())
        .unwrap_or_default();
    let (width, height) = image::image_dimensions(path).unwrap_or((0, 0));
    Some((name, width, height, format, size))
}

fn list(options: &Options, info: bool) -> i32 {
    let files = for_listing(options);
    if files.is_empty() {
        eprintln!("peepin: nothing to list");
        return 1;
    }
    for (number, path) in files.iter().enumerate() {
        match describe(path) {
            Some((name, width, height, format, size)) => {
                if info {
                    println!("{name} {width}x{height} {format} {size}");
                } else {
                    println!("{}:{name}:{width}:{height}:{format}:{size}", number + 1);
                }
            }
            None => eprintln!("peepin: {} cannot be read", path.display()),
        }
    }
    0
}

fn screenshot(file: Option<PathBuf>, window: bool, delay: f64) -> i32 {
    if delay > 0.0 {
        std::thread::sleep(std::time::Duration::from_secs_f64(delay));
    }
    let Some(pixels) = crate::grab_screen(window) else {
        if window {
            eprintln!(
                "peepin: no X11 window has the focus — --window reads X windows, and a Wayland one cannot be read this way"
            );
        } else {
            eprintln!("peepin: no screen to take a picture of (is DISPLAY set?)");
        }
        return 1;
    };
    let path = match file {
        Some(path) => path,
        None => {
            // The folder set in F2, else the one the shell is in.
            let folder = crate::config_value("screenshot.dir")
                .map(|value| crate::expand_home(&value))
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            let _ = std::fs::create_dir_all(&folder);
            crate::free_name(&folder, &format!("peepin-{}.png", crate::stamp()))
        }
    };
    let mut png: Vec<u8> = Vec::new();
    if let Err(err) = image::DynamicImage::ImageRgba8(pixels).write_to(
        &mut std::io::Cursor::new(&mut png),
        image::ImageFormat::Png,
    ) {
        eprintln!("peepin: {err}");
        return 1;
    }
    if let Err(err) = std::fs::write(&path, &png) {
        eprintln!("peepin: {}: {err}", path.display());
        return 1;
    }
    if !crate::copy_to_clipboard(&png) {
        eprintln!("peepin: the picture was written, but no clipboard tool took it");
    }
    println!("{}", path.display());
    0
}
