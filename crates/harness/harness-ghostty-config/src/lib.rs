//! The user's Ghostty config, read live, so harness-native draws and harnessd answers colour queries (OSC 4/10/11)
//! the way the user's Ghostty does.
//!
//! - [`Config::load`] reads Ghostty's config files (`$XDG_CONFIG_HOME/ghostty/config.ghostty`, `.../config`, then
//!   `~/Library/Application Support/com.mitchellh.ghostty/config(.ghostty)`; every one that exists, later winning, as
//!   Ghostty does), follows `config-file` includes (`?path` optional) after the file that names them, resolves
//!   `theme = dark:X,light:Y` (or one name) against `$XDG_CONFIG_HOME/ghostty/themes`, then Ghostty.app's themes, and
//!   applies `$HARNESS_HOME/theme` (same syntax) last.
//! - Within a layer the theme's colours go down first and the layer's own colour keys over them; later lines win.
//! - [`Config::theme`] is one appearance's colours filled in from [`BUILTIN`] where nothing set them.
//! - [`Live`] re-reads when any file it read (or looked for) changes; [`system_dark`] is the system appearance.
//!
//! Keys taken: background, foreground, palette (0..=255), cursor-color, cursor-text, selection-background,
//! selection-foreground, font-family (and -bold, -italic, -bold-italic), font-size, window-padding-x/y,
//! cursor-style-blink, theme, config-file. Everything else (custom-shader among them) is ignored.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

pub type Rgb = [u8; 3];

/// Colours as a config layer sets them: `None` where it says nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Colors {
    pub background: Option<Rgb>,
    pub foreground: Option<Rgb>,
    pub palette: [Option<Rgb>; 256],
    pub cursor_color: Option<Rgb>,
    pub cursor_text: Option<Rgb>,
    pub selection_background: Option<Rgb>,
    pub selection_foreground: Option<Rgb>,
}

impl Default for Colors {
    fn default() -> Self {
        Self { background: None, foreground: None, palette: [None; 256], cursor_color: None, cursor_text: None, selection_background: None, selection_foreground: None }
    }
}

impl Colors {
    /// `top`'s colours over these.
    fn overlay(&mut self, top: &Self) {
        let pick = |a: &mut Option<Rgb>, b: Option<Rgb>| {
            if b.is_some() {
                *a = b;
            }
        };
        pick(&mut self.background, top.background);
        pick(&mut self.foreground, top.foreground);
        for (a, b) in self.palette.iter_mut().zip(top.palette) {
            pick(a, b);
        }
        pick(&mut self.cursor_color, top.cursor_color);
        pick(&mut self.cursor_text, top.cursor_text);
        pick(&mut self.selection_background, top.selection_background);
        pick(&mut self.selection_foreground, top.selection_foreground);
    }

    /// Set colour key `key` to `value`; false when `key` is not a colour key.
    fn set(&mut self, key: &str, value: &str) -> bool {
        let slot = match key {
            "background" => &mut self.background,
            "foreground" => &mut self.foreground,
            "cursor-color" => &mut self.cursor_color,
            "cursor-text" => &mut self.cursor_text,
            "selection-background" => &mut self.selection_background,
            "selection-foreground" => &mut self.selection_foreground,
            "palette" => {
                if let Some((i, c)) = value.split_once('=')
                    && let Ok(i) = i.trim().parse::<u8>()
                {
                    self.palette[usize::from(i)] = parse_rgb(c);
                }
                return true;
            }
            _ => return false,
        };
        *slot = parse_rgb(value);
        true
    }
}

/// One appearance's colours, every one filled in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    pub fg: Rgb,
    pub bg: Rgb,
    pub palette: [Rgb; 256],
    /// None: the app's own default cursor colour.
    pub cursor: Option<Rgb>,
    pub cursor_text: Option<Rgb>,
    /// None: the app's own selection look.
    pub selection_bg: Option<Rgb>,
    pub selection_fg: Option<Rgb>,
}

/// The 256-colour palette's colour `i` past the 16 named ones: the 6x6x6 cube, then 24 greys (xterm's, as Ghostty).
#[must_use]
pub fn xterm_256(i: u8) -> Rgb {
    match i {
        0..=15 => BUILTIN_ANSI[usize::from(i)],
        16..=231 => {
            let i = i - 16;
            let step = |c: u8| if c == 0 { 0 } else { 55 + 40 * c };
            [step(i / 36), step((i / 6) % 6), step(i % 6)]
        }
        _ => {
            let v = 8 + 10 * (i - 232);
            [v, v, v]
        }
    }
}

/// harness's built-in dark theme: what shows with no Ghostty config.
pub const BUILTIN_FG: Rgb = [0xdc, 0xdc, 0xe0];
/// The blaze site's --bg (#101011), as the scene's tone_term_ground is.
pub const BUILTIN_BG: Rgb = [0x10, 0x10, 0x11];
pub const BUILTIN_ANSI: [Rgb; 16] = [
    [0x2a, 0x2a, 0x2e],
    [0xeb, 0x6b, 0x6b],
    [0x4c, 0xb7, 0x82],
    [0xf2, 0xbb, 0x52],
    [0x6e, 0x8c, 0xff],
    [0xc4, 0x8c, 0xff],
    [0x5e, 0xc4, 0xd6],
    [0xc9, 0xc9, 0xcf],
    [0x5c, 0x5c, 0x64],
    [0xff, 0x8a, 0x8a],
    [0x6f, 0xd6, 0xa0],
    [0xff, 0xd2, 0x7a],
    [0x93, 0xa9, 0xff],
    [0xd7, 0xaa, 0xff],
    [0x86, 0xdb, 0xe8],
    [0xed, 0xed, 0xf0],
];

/// The built-in theme as a [`Theme`].
#[must_use]
pub fn builtin() -> Theme {
    Theme { fg: BUILTIN_FG, bg: BUILTIN_BG, palette: std::array::from_fn(|i| xterm_256(i as u8)), cursor: None, cursor_text: None, selection_bg: None, selection_fg: None }
}

/// The user's Ghostty config, as harness reads it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    pub dark: Colors,
    pub light: Colors,
    /// Font families in Ghostty's order (the first is the primary face); empty: the app's default.
    pub font_family: Vec<String>,
    pub font_family_bold: Vec<String>,
    pub font_family_italic: Vec<String>,
    pub font_family_bold_italic: Vec<String>,
    pub font_size: Option<f32>,
    /// Left and right, top and bottom, points.
    pub padding_x: Option<(f32, f32)>,
    pub padding_y: Option<(f32, f32)>,
    pub cursor_blink: Option<bool>,
    /// Whether any config file (or the override) was read: false means the built-in theme throughout.
    pub found: bool,
    /// Every file read or looked for (a theme name's candidates, a missing config), for [`Live`] to watch.
    pub watched: Vec<PathBuf>,
}

/// Where a layer comes from.
#[derive(Clone, Debug)]
pub struct Sources {
    /// Config files, in load order (missing ones skipped).
    pub configs: Vec<PathBuf>,
    /// Directories a theme name is looked up in, in order.
    pub theme_dirs: Vec<PathBuf>,
    /// Applied last: `$HARNESS_HOME/theme`.
    pub overrides: Vec<PathBuf>,
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

fn expand(p: &str, base: Option<&Path>) -> PathBuf {
    let p = p.trim();
    if let Some(rest) = p.strip_prefix("~/") {
        return home().join(rest);
    }
    let pb = PathBuf::from(p);
    match base {
        Some(b) if pb.is_relative() => b.join(pb),
        _ => pb,
    }
}

impl Sources {
    /// Ghostty's own places on this machine, and `$HARNESS_HOME/theme`.
    #[must_use]
    pub fn standard() -> Self {
        let xdg = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()).map_or_else(|| home().join(".config"), PathBuf::from).join("ghostty");
        let app = home().join("Library/Application Support/com.mitchellh.ghostty");
        let harness = std::env::var_os("HARNESS_HOME").map_or_else(|| home().join(".harness"), PathBuf::from);
        let mut theme_dirs = vec![xdg.join("themes")];
        if let Some(r) = std::env::var_os("GHOSTTY_RESOURCES_DIR") {
            theme_dirs.push(PathBuf::from(r).join("themes"));
        }
        theme_dirs.push(PathBuf::from("/Applications/Ghostty.app/Contents/Resources/ghostty/themes"));
        Self {
            configs: vec![xdg.join("config.ghostty"), xdg.join("config"), app.join("config.ghostty"), app.join("config")],
            theme_dirs,
            overrides: vec![harness.join("theme")],
        }
    }
}

/// A file's `key = value` lines in order, its `config-file` includes' lines after its own (Ghostty loads them after the
/// file that names them). Full-line `#` comments; values may be double-quoted.
fn read_lines(path: &Path, watched: &mut Vec<PathBuf>, depth: u8, out: &mut Vec<(String, String)>) -> bool {
    watched.push(path.to_owned());
    let Ok(text) = std::fs::read_to_string(path) else { return false };
    let mut includes = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let (k, mut v) = (k.trim(), v.trim());
        if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
            v = &v[1..v.len() - 1];
        }
        if k == "config-file" {
            includes.push(v.to_owned());
        } else {
            out.push((k.to_owned(), v.to_owned()));
        }
    }
    for inc in includes {
        let (optional, p) = inc.strip_prefix('?').map_or((false, inc.as_str()), |p| (true, p));
        let p = expand(p, path.parent());
        if depth < 8 && !read_lines(&p, watched, depth + 1, out) && !optional {
            eprintln!("harness-ghostty-config: {}: config-file {} not found", path.display(), p.display());
        }
    }
    true
}

pub fn parse_rgb(v: &str) -> Option<Rgb> {
    let v = v.trim();
    let h = v.strip_prefix('#').unwrap_or(v);
    let n = |s: &str| u8::from_str_radix(s, 16).ok();
    match h.len() {
        6 => Some([n(&h[0..2])?, n(&h[2..4])?, n(&h[4..6])?]),
        3 => {
            let d = |i: usize| n(&h[i..=i]).map(|x| x * 17);
            Some([d(0)?, d(1)?, d(2)?])
        }
        _ => None,
    }
}

fn pair(v: &str) -> Option<(f32, f32)> {
    let mut it = v.split(',').map(|s| s.trim().parse::<f32>());
    let a = it.next()?.ok()?;
    let b = it.next().map_or(Some(a), Result::ok)?;
    Some((a, b))
}

/// `theme = dark:X,light:Y` or `theme = X`: (dark, light) names.
fn theme_names(v: &str) -> (Option<String>, Option<String>) {
    let (mut dark, mut light, mut both) = (None, None, None);
    for part in v.split(',') {
        let part = part.trim();
        if let Some(d) = part.strip_prefix("dark:") {
            dark = Some(d.trim().to_owned());
        } else if let Some(l) = part.strip_prefix("light:") {
            light = Some(l.trim().to_owned());
        } else if !part.is_empty() {
            both = Some(part.to_owned());
        }
    }
    (dark.or_else(|| both.clone()), light.or(both))
}

/// One layer: the main config (with its includes) or the override.
#[derive(Default)]
struct Layer {
    theme: Option<(Option<String>, Option<String>)>,
    colors: Colors,
}

impl Config {
    /// Read the user's config from Ghostty's standard places.
    #[must_use]
    pub fn load() -> Self {
        Self::load_from(&Sources::standard())
    }

    #[must_use]
    pub fn load_from(src: &Sources) -> Self {
        let mut cfg = Self::default();
        let mut watched = Vec::new();
        let mut layers = Vec::new();
        for group in [&src.configs, &src.overrides] {
            let mut lines = Vec::new();
            let mut any = false;
            for p in group {
                any |= read_lines(p, &mut watched, 0, &mut lines);
            }
            cfg.found |= any;
            let mut layer = Layer::default();
            for (k, v) in &lines {
                cfg.apply(&mut layer, k, v);
            }
            layers.push(layer);
        }
        // Each layer: its theme's colours, then its own keys over them.
        for layer in layers {
            if let Some((dark, light)) = &layer.theme {
                for (name, out) in [(dark, &mut cfg.dark), (light, &mut cfg.light)] {
                    let Some(name) = name else { continue };
                    if let Some(c) = theme_colors(name, &src.theme_dirs, &mut watched) {
                        out.overlay(&c);
                    } else {
                        eprintln!("harness-ghostty-config: theme {name} not found");
                    }
                }
            }
            cfg.dark.overlay(&layer.colors);
            cfg.light.overlay(&layer.colors);
        }
        cfg.watched = watched;
        cfg
    }

    fn apply(&mut self, layer: &mut Layer, k: &str, v: &str) {
        if layer.colors.set(k, v) {
            return;
        }
        let fam = |list: &mut Vec<String>| {
            if v.is_empty() {
                list.clear();
            } else {
                list.push(v.to_owned());
            }
        };
        match k {
            "theme" => layer.theme = (!v.is_empty()).then(|| theme_names(v)),
            "font-family" => fam(&mut self.font_family),
            "font-family-bold" => fam(&mut self.font_family_bold),
            "font-family-italic" => fam(&mut self.font_family_italic),
            "font-family-bold-italic" => fam(&mut self.font_family_bold_italic),
            "font-size" => self.font_size = v.parse().ok().filter(|s: &f32| *s > 0.0),
            "window-padding-x" => self.padding_x = pair(v),
            "window-padding-y" => self.padding_y = pair(v),
            "cursor-style-blink" => self.cursor_blink = v.parse().ok(),
            _ => {}
        }
    }

    /// The colours as set for the dark or the light appearance.
    #[must_use]
    pub fn colors(&self, dark: bool) -> &Colors {
        if dark { &self.dark } else { &self.light }
    }

    /// The dark or the light appearance's theme, filled in from the built-in one where nothing set a colour.
    #[must_use]
    pub fn theme(&self, dark: bool) -> Theme {
        let c = self.colors(dark);
        let b = builtin();
        Theme {
            fg: c.foreground.unwrap_or(b.fg),
            bg: c.background.unwrap_or(b.bg),
            palette: std::array::from_fn(|i| c.palette[i].unwrap_or(b.palette[i])),
            cursor: c.cursor_color,
            cursor_text: c.cursor_text,
            selection_bg: c.selection_background,
            selection_fg: c.selection_foreground,
        }
    }
}

/// A theme's colours: an absolute path, or a name looked up in `dirs` in turn.
fn theme_colors(name: &str, dirs: &[PathBuf], watched: &mut Vec<PathBuf>) -> Option<Colors> {
    let candidates: Vec<PathBuf> = if Path::new(name).is_absolute() || name.starts_with("~/") { vec![expand(name, None)] } else { dirs.iter().map(|d| d.join(name)).collect() };
    for p in candidates {
        let mut lines = Vec::new();
        if read_lines(&p, watched, 0, &mut lines) {
            let mut c = Colors::default();
            for (k, v) in &lines {
                c.set(k, v);
            }
            return Some(c);
        }
    }
    None
}

/// What a file looked like: where its symlinks led, when it changed and how long it was (None: not there).
type Print = Vec<(PathBuf, Option<(PathBuf, SystemTime, u64)>)>;

fn print(paths: &[PathBuf]) -> Print {
    paths
        .iter()
        .map(|p| {
            let st = std::fs::canonicalize(p).ok().and_then(|real| {
                let m = std::fs::metadata(&real).ok()?;
                Some((real, m.modified().ok()?, m.len()))
            });
            (p.clone(), st)
        })
        .collect()
}

/// The config, re-read when a file it read (or looked for) changed. Checks at most every [`Live::EVERY`].
pub struct Live {
    config: Config,
    src: Sources,
    print: Print,
    checked: Instant,
}

impl Live {
    pub const EVERY: Duration = Duration::from_millis(500);

    #[must_use]
    pub fn new() -> Self {
        Self::with(Sources::standard())
    }

    #[must_use]
    pub fn with(src: Sources) -> Self {
        let config = Config::load_from(&src);
        let print = print(&config.watched);
        Self { config, src, print, checked: Instant::now() }
    }

    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Re-read when a watched file changed since the last read; true when the config is now different.
    pub fn poll(&mut self) -> bool {
        if self.checked.elapsed() < Self::EVERY {
            return false;
        }
        self.checked = Instant::now();
        if print(&self.config.watched) == self.print {
            return false;
        }
        let config = Config::load_from(&self.src);
        self.print = print(&config.watched);
        let changed = config != self.config;
        self.config = config;
        changed
    }
}

/// Call `on_change` (on a thread of its own, checking once a second) whenever the config read from `src` changes: an
/// idle window wakes to re-read it.
pub fn watch(src: Sources, on_change: impl Fn() + Send + 'static) {
    let _ = std::thread::Builder::new().name("ghostty-config-watch".into()).spawn(move || {
        let mut live = Live::with(src);
        loop {
            std::thread::sleep(Duration::from_secs(1));
            if live.poll() {
                on_change();
            }
        }
    });
}

impl Default for Live {
    fn default() -> Self {
        Self::new()
    }
}

/// The process's shared [`Live`] config (harness-vt's terminals read their default colours from it).
pub fn shared() -> std::sync::MutexGuard<'static, Live> {
    static LIVE: std::sync::OnceLock<std::sync::Mutex<Live>> = std::sync::OnceLock::new();
    let m = LIVE.get_or_init(|| std::sync::Mutex::new(Live::new()));
    let mut g = m.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    g.poll();
    g
}

/// The theme for the current system appearance, from the shared live config.
#[must_use]
pub fn current_theme() -> Theme {
    shared().config().theme(system_dark())
}

/// Whether the system appearance is dark: `HARNESS_NATIVE_APPEARANCE=light|dark` when set, else macOS's
/// `AppleInterfaceStyle` (read through CFPreferences, synchronised, at most every quarter second), else dark.
#[must_use]
pub fn system_dark() -> bool {
    match std::env::var("HARNESS_NATIVE_APPEARANCE").as_deref() {
        Ok("light") => return false,
        Ok("dark") => return true,
        _ => {}
    }
    static LAST: std::sync::Mutex<Option<(Instant, bool)>> = std::sync::Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((at, v)) = *last
        && at.elapsed() < Duration::from_millis(250)
    {
        return v;
    }
    let v = mac::interface_style_dark().unwrap_or(true);
    *last = Some((Instant::now(), v));
    v
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::{c_char, c_void};
    type CFStringRef = *const c_void;
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFPreferencesAnyApplication: CFStringRef;
        fn CFStringCreateWithCString(alloc: *const c_void, s: *const c_char, enc: u32) -> CFStringRef;
        fn CFStringGetCString(s: CFStringRef, buf: *mut c_char, len: isize, enc: u32) -> bool;
        fn CFGetTypeID(v: *const c_void) -> usize;
        fn CFStringGetTypeID() -> usize;
        fn CFPreferencesCopyAppValue(key: CFStringRef, app: CFStringRef) -> *const c_void;
        fn CFPreferencesAppSynchronize(app: CFStringRef) -> bool;
        fn CFRelease(v: *const c_void);
    }
    const UTF8: u32 = 0x0800_0100;

    /// Some(true) when the global `AppleInterfaceStyle` is "Dark", Some(false) when it is unset (light).
    pub fn interface_style_dark() -> Option<bool> {
        // SAFETY: CoreFoundation calls on objects created and released here; the global domain constant is static.
        unsafe {
            let any = kCFPreferencesAnyApplication;
            CFPreferencesAppSynchronize(any);
            let key = CFStringCreateWithCString(std::ptr::null(), c"AppleInterfaceStyle".as_ptr(), UTF8);
            if key.is_null() {
                return None;
            }
            let v = CFPreferencesCopyAppValue(key, any);
            CFRelease(key);
            if v.is_null() {
                return Some(false);
            }
            let mut buf = [0 as c_char; 32];
            let dark = CFGetTypeID(v) == CFStringGetTypeID() && CFStringGetCString(v, buf.as_mut_ptr(), buf.len() as isize, UTF8) && std::ffi::CStr::from_ptr(buf.as_ptr()).to_bytes() == b"Dark";
            CFRelease(v);
            Some(dark)
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod mac {
    pub fn interface_style_dark() -> Option<bool> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("harness-ghostty-config-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("themes")).unwrap();
        d
    }

    #[test]
    fn includes_themes_palette_and_override() {
        let d = tmp("all");
        std::fs::write(d.join("themes/dk"), "background = 1e1e1e\nforeground = d4d4d4\npalette = 1=#f38ba8\npalette = 200=010203\ncursor-color = f5e0dc\n").unwrap();
        std::fs::write(d.join("themes/lt"), "background = f9f9f9\nforeground = 2a2c33\n").unwrap();
        std::fs::write(
            d.join("config"),
            "# comment\nfont-family = \"Berkeley Mono\"\nfont-size = 12\ntheme = dark:dk,light:lt\nwindow-padding-x = 5\nwindow-padding-y = 3,7\nconfig-file = inc\nconfig-file = ?missing\npalette = 2=00ff00\ncursor-style-blink = false\ncustom-shader = x.glsl\n",
        )
        .unwrap();
        // An include loads after its file: its palette 2 wins over the line after `config-file`.
        std::fs::write(d.join("inc"), "palette = 2=112233\nfont-size = 13\n").unwrap();
        std::fs::write(d.join("override"), "foreground = ffffff\n").unwrap();
        let src = Sources { configs: vec![d.join("config")], theme_dirs: vec![d.join("themes")], overrides: vec![d.join("override")] };
        let c = Config::load_from(&src);
        assert!(c.found);
        assert_eq!(c.font_family, vec!["Berkeley Mono".to_owned()]);
        assert_eq!(c.font_size, Some(13.0));
        assert_eq!(c.padding_x, Some((5.0, 5.0)));
        assert_eq!(c.padding_y, Some((3.0, 7.0)));
        assert_eq!(c.cursor_blink, Some(false));
        let dark = c.theme(true);
        assert_eq!(dark.bg, [0x1e, 0x1e, 0x1e]);
        assert_eq!(dark.fg, [0xff, 0xff, 0xff], "the override wins");
        assert_eq!(dark.palette[1], [0xf3, 0x8b, 0xa8]);
        assert_eq!(dark.palette[2], [0x11, 0x22, 0x33], "config keys win over the theme; the include last");
        assert_eq!(dark.palette[200], [1, 2, 3]);
        assert_eq!(dark.palette[3], BUILTIN_ANSI[3]);
        assert_eq!(dark.palette[16], [0, 0, 0]);
        assert_eq!(dark.cursor, Some([0xf5, 0xe0, 0xdc]));
        let light = c.theme(false);
        assert_eq!(light.bg, [0xf9, 0xf9, 0xf9]);
        assert_eq!(light.palette[1], BUILTIN_ANSI[1]);
        assert_eq!(light.palette[2], [0x11, 0x22, 0x33]);
        assert!(c.watched.contains(&d.join("missing")));
    }

    #[test]
    fn single_theme_and_nothing() {
        let d = tmp("one");
        std::fs::write(d.join("themes/one"), "background = #abc\n").unwrap();
        std::fs::write(d.join("config"), "theme = one\n").unwrap();
        let src = Sources { configs: vec![d.join("config")], theme_dirs: vec![d.join("themes")], overrides: vec![] };
        let c = Config::load_from(&src);
        assert_eq!(c.theme(true).bg, [0xaa, 0xbb, 0xcc]);
        assert_eq!(c.theme(false).bg, [0xaa, 0xbb, 0xcc]);
        let none = Config::load_from(&Sources { configs: vec![d.join("nope")], theme_dirs: vec![], overrides: vec![] });
        assert!(!none.found);
        assert_eq!(none.theme(false), builtin());
    }
}
