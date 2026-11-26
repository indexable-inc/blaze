//! Open-source IntelliJ New UI file icons (`crates/harness/harness-native-render/icons/files/`)
//! and the two-tone macOS blue folder icon (`site/static/logos/folder.svg`), parsed directly
//! from SVG and rasterized on demand at native Retina resolution into an RGBA8 Metal icon atlas.

use std::collections::HashMap;
use std::sync::OnceLock;

/// One absolute SVG path segment (user coordinates, y down).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Seg {
    Move([f64; 2]),
    Line([f64; 2]),
    Quad([f64; 2], [f64; 2]),
    Cubic([f64; 2], [f64; 2], [f64; 2]),
    Close,
}

/// One painted vector layer of an SVG icon.
#[derive(Clone, Debug, PartialEq)]
pub struct Layer {
    pub segs: Vec<Seg>,
    pub fill: Option<[f32; 4]>,
    pub even_odd: bool,
    pub stroke: Option<([f32; 4], f64)>,
}

/// Parsed multi-layer vector icon with its SVG viewBox.
#[derive(Clone, Debug, PartialEq)]
pub struct SvgArt {
    pub view: [f64; 4],
    pub layers: Vec<Layer>,
}

/// Authentic two-tone macOS Finder blue folder SVG (matching `ix.dev/blaze`'s `target/`).
const MACOS_FOLDER_SVG: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" xmlns="http://www.w3.org/2000/svg">
<path d="M1.2 3.2C1.2 2.48 1.78 1.9 2.5 1.9H6.0C6.42 1.9 6.81 2.1 7.06 2.44L7.85 3.45H13.5C14.22 3.45 14.8 4.03 14.8 4.75V12.4C14.8 13.12 14.22 13.7 13.5 13.7H2.5C1.78 13.7 1.2 13.12 1.2 12.4V3.2Z" fill="#3B89E6"/>
<rect x="1.2" y="4.8" width="13.6" height="8.9" rx="1.45" fill="#63B4FB"/>
<line x1="2.4" y1="5.1" x2="13.6" y2="5.1" stroke="#8ECEFF" stroke-width="0.6"/>
</svg>"##;

/// Warm amber excluded build-directory folder (`target/`, `.blaze/`, `node_modules/`), matching RustRover.
const EXCLUDED_FOLDER_SVG: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" xmlns="http://www.w3.org/2000/svg">
<path d="M1.2 3.2C1.2 2.48 1.78 1.9 2.5 1.9H6.0C6.42 1.9 6.81 2.1 7.06 2.44L7.85 3.45H13.5C14.22 3.45 14.8 4.03 14.8 4.75V12.4C14.8 13.12 14.22 13.7 13.5 13.7H2.5C1.78 13.7 1.2 13.12 1.2 12.4V3.2Z" fill="#B57234"/>
<rect x="1.2" y="4.8" width="13.6" height="8.9" rx="1.45" fill="#D49253"/>
<line x1="2.4" y1="5.1" x2="13.6" y2="5.1" stroke="#E8B27A" stroke-width="0.6"/>
</svg>"##;

/// RustRover / IntelliJ New UI `Cargo.toml` icon: 3 Cargo orange crate boxes + circular TOML badge.
const CARGO_TOML_SVG: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" xmlns="http://www.w3.org/2000/svg">
<rect x="1.6" y="1.6" width="5.0" height="5.0" rx="0.9" fill="#3B2719" stroke="#D98245" stroke-width="1.25"/>
<rect x="8.6" y="1.6" width="5.0" height="5.0" rx="0.9" fill="#3B2719" stroke="#D98245" stroke-width="1.25"/>
<rect x="1.6" y="8.6" width="5.0" height="5.0" rx="0.9" fill="#3B2719" stroke="#D98245" stroke-width="1.25"/>
<circle cx="11.7" cy="11.7" r="3.1" fill="#232529" stroke="#CED0D6" stroke-width="1.15"/>
<path d="M10.1 10.6H13.3M11.7 10.6V13.4" stroke="#CED0D6" stroke-width="1.1" stroke-linecap="round"/>
</svg>"##;

/// RustRover / IntelliJ New UI `Cargo.lock` icon: 3 Cargo orange crate boxes + silver padlock badge.
const CARGO_LOCK_SVG: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" xmlns="http://www.w3.org/2000/svg">
<rect x="1.6" y="1.6" width="5.0" height="5.0" rx="0.9" fill="#3B2719" stroke="#D98245" stroke-width="1.25"/>
<rect x="8.6" y="1.6" width="5.0" height="5.0" rx="0.9" fill="#3B2719" stroke="#D98245" stroke-width="1.25"/>
<rect x="1.6" y="8.6" width="5.0" height="5.0" rx="0.9" fill="#3B2719" stroke="#D98245" stroke-width="1.25"/>
<path d="M10.2 10.4V9.3C10.2 8.45 10.85 7.8 11.7 7.8C12.55 7.8 13.2 8.45 13.2 9.3V10.4" stroke="#CED0D6" stroke-width="1.15" stroke-linecap="round"/>
<rect x="9.1" y="10.4" width="5.2" height="4.2" rx="0.9" fill="#232529" stroke="#CED0D6" stroke-width="1.15"/>
</svg>"##;

/// Blaze `blaze.lock` icon: 3 blaze-ember crate boxes (`#E0461A`) + silver padlock badge.
const BLAZE_LOCK_SVG: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" xmlns="http://www.w3.org/2000/svg">
<rect x="1.6" y="1.6" width="5.0" height="5.0" rx="0.9" fill="#3D1C14" stroke="#E85A30" stroke-width="1.25"/>
<rect x="8.6" y="1.6" width="5.0" height="5.0" rx="0.9" fill="#3D1C14" stroke="#E85A30" stroke-width="1.25"/>
<rect x="1.6" y="8.6" width="5.0" height="5.0" rx="0.9" fill="#3D1C14" stroke="#E85A30" stroke-width="1.25"/>
<path d="M10.2 10.4V9.3C10.2 8.45 10.85 7.8 11.7 7.8C12.55 7.8 13.2 8.45 13.2 9.3V10.4" stroke="#CED0D6" stroke-width="1.15" stroke-linecap="round"/>
<rect x="9.1" y="10.4" width="5.2" height="4.2" rx="0.9" fill="#232529" stroke="#CED0D6" stroke-width="1.15"/>
</svg>"##;

/// Open-perspective Apple MacBook Pro vector icon: glossy jet-black display with diagonal
/// reflection, brushed-silver trapezoidal aluminum deck, black chiclet keyboard, & trackpad.
const MACBOOK_PRO_SVG: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" xmlns="http://www.w3.org/2000/svg">
<path d="M2.25 1.75C2.2 1.3 2.55 0.95 3.0 0.95H13.0C13.45 0.95 13.8 1.3 13.75 1.75L13.2 8.2H2.8L2.25 1.75Z" fill="#090A0C" stroke="#A2A7B0" stroke-width="0.72"/>
<path d="M8.9 1.3H13.15L12.78 7.85H11.45L8.9 1.3Z" fill="#3A3D43"/>
<rect x="2.8" y="8.0" width="10.4" height="0.55" fill="#4C5058"/>
<path d="M2.7 8.45H13.3L14.9 14.15C15.0 14.55 14.7 14.95 14.25 14.95H1.75C1.3 14.95 1.0 14.55 1.1 14.15L2.7 8.45Z" fill="#C9CCD1"/>
<path d="M1.2 14.35H14.8L14.6 14.88C14.5 14.95 14.38 14.98 14.25 14.98H1.75C1.62 14.98 1.5 14.95 1.4 14.88L1.2 14.35Z" fill="#989DA6"/>
<rect x="6.5" y="14.38" width="3.0" height="0.55" rx="0.25" fill="#6D727A"/>
<rect x="3.2" y="9.0" width="1.25" height="0.65" rx="0.18" fill="#1F2125"/>
<rect x="4.75" y="9.0" width="1.25" height="0.65" rx="0.18" fill="#1F2125"/>
<rect x="6.3" y="9.0" width="1.25" height="0.65" rx="0.18" fill="#1F2125"/>
<rect x="7.85" y="9.0" width="1.25" height="0.65" rx="0.18" fill="#1F2125"/>
<rect x="9.4" y="9.0" width="1.25" height="0.65" rx="0.18" fill="#1F2125"/>
<rect x="10.95" y="9.0" width="1.85" height="0.65" rx="0.18" fill="#1F2125"/>
<rect x="2.9" y="10.0" width="1.55" height="0.68" rx="0.18" fill="#1F2125"/>
<rect x="4.75" y="10.0" width="1.3" height="0.68" rx="0.18" fill="#1F2125"/>
<rect x="6.35" y="10.0" width="1.3" height="0.68" rx="0.18" fill="#1F2125"/>
<rect x="7.95" y="10.0" width="1.3" height="0.68" rx="0.18" fill="#1F2125"/>
<rect x="9.55" y="10.0" width="1.3" height="0.68" rx="0.18" fill="#1F2125"/>
<rect x="11.15" y="10.0" width="1.95" height="0.68" rx="0.18" fill="#1F2125"/>
<rect x="2.6" y="11.02" width="1.9" height="0.72" rx="0.18" fill="#1F2125"/>
<rect x="4.85" y="11.02" width="6.3" height="0.72" rx="0.18" fill="#1F2125"/>
<rect x="11.5" y="11.02" width="1.9" height="0.72" rx="0.18" fill="#1F2125"/>
<path d="M5.65 12.18H10.35L10.75 14.08H5.25L5.65 12.18Z" fill="#9DA1A8"/>
</svg>"##;

/// Modern dark-slate remote server rack vector icon with crisp luminous status LEDs.
const SERVER_RACK_SVG: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" xmlns="http://www.w3.org/2000/svg">
<rect x="1.8" y="2.2" width="12.4" height="5.0" rx="1.1" fill="#22252B" stroke="#8A92A0" stroke-width="0.9"/>
<rect x="1.8" y="8.8" width="12.4" height="5.0" rx="1.1" fill="#22252B" stroke="#8A92A0" stroke-width="0.9"/>
<circle cx="4.2" cy="4.7" r="1.05" fill="#52C482"/>
<circle cx="4.2" cy="11.3" r="1.05" fill="#5CA4E8"/>
<line x1="7.2" y1="4.7" x2="12.0" y2="4.7" stroke="#6E7684" stroke-width="1.0" stroke-linecap="round"/>
<line x1="7.2" y1="11.3" x2="12.0" y2="11.3" stroke="#6E7684" stroke-width="1.0" stroke-linecap="round"/>
</svg>"##;

/// IntelliJ New UI Dark backup file icon (`.bak`, `.backup`, `.orig`, `.old`, `.swp`, `*~`):
/// folded dark document sheet + warm-amber circular rewind/history badge.
const BACKUP_FILE_SVG: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" xmlns="http://www.w3.org/2000/svg">
<path d="M3.0 2.0C2.45 2.0 2.0 2.45 2.0 3.0V13.0C2.0 13.55 2.45 14.0 3.0 14.0H9.2V12.8H3.2V3.2H8.4V5.8C8.4 6.24 8.76 6.6 9.2 6.6H11.8V8.4H13.0V5.6L9.4 2.0H3.0Z" fill="#25272C" stroke="#8C93A1" stroke-width="0.9"/>
<line x1="4.4" y1="7.8" x2="8.8" y2="7.8" stroke="#6E7684" stroke-width="1.0" stroke-linecap="round"/>
<line x1="4.4" y1="10.2" x2="7.6" y2="10.2" stroke="#6E7684" stroke-width="1.0" stroke-linecap="round"/>
<circle cx="11.6" cy="11.6" r="3.4" fill="#3B2B18" stroke="#E5B567" stroke-width="1.15"/>
<path d="M10.1 11.1C10.3 10.2 11.1 9.6 12.0 9.8C12.8 10.0 13.3 10.8 13.2 11.7C13.1 12.6 12.3 13.2 11.4 13.1C10.9 13.05 10.5 12.8 10.2 12.4" stroke="#F0C987" stroke-width="1.05" stroke-linecap="round"/>
<path d="M9.6 10.2L10.0 11.4L11.2 11.0" stroke="#F0C987" stroke-width="1.05" stroke-linecap="round" stroke-linejoin="round"/>
</svg>"##;

macro_rules! intellij_dark {
    ($($name:literal),* $(,)?) => {
        &[
            // Two-tone macOS blue folder & RustRover excluded amber folder.
            ("folder_macos", MACOS_FOLDER_SVG),
            ("folder_target", EXCLUDED_FOLDER_SVG),
            // Modern MacBook Pro & remote server icons for prompt host segment & Agent Hub.
            ("macbook_pro", MACBOOK_PRO_SVG),
            ("server_rack", SERVER_RACK_SVG),
            // RustRover / IntelliJ New UI Cargo, Blaze, & Backup file icons.
            ("cargo_toml", CARGO_TOML_SVG),
            ("cargo_lock", CARGO_LOCK_SVG),
            ("blaze_lock", BLAZE_LOCK_SVG),
            ("backup_dark", BACKUP_FILE_SVG),
            // Open-source IntelliJ New UI dark file icons (Apache-2.0, see harness-native-render/icons/files/NOTICE).
            ("gitignore", include_str!("../../harness/harness-native-render/icons/files/gitignore.svg")),
            $(($name, include_str!(concat!("../../harness/harness-native-render/icons/files/", $name, ".svg")))),*
        ]
    };
}

/// All vendored SVG icons embedded directly in the binary (zero external runtime files).
pub const SVG_SOURCES: &[(&str, &str)] = intellij_dark![
    "archive_dark",
    "binaryData_dark",
    "c_dark",
    "config_dark",
    "cpp_dark",
    "css_dark",
    "csv_dark",
    "docker_dark",
    "editorConfig_dark",
    "folderGithub_dark",
    "font_dark",
    "go_dark",
    "graphql_dark",
    "h_dark",
    "html_dark",
    "image_dark",
    "java_dark",
    "javaScript_dark",
    "json_dark",
    "kotlin_dark",
    "lockFile_dark",
    "markdown_dark",
    "nix_dark",
    "python_dark",
    "ruby_dark",
    "rust_dark",
    "shell_dark",
    "sql_dark",
    "swiftLang_dark",
    "terraform_dark",
    "text_dark",
    "toml_dark",
    "typeScript_dark",
    "xml_dark",
    "yaml_dark",
];

fn all_parsed() -> &'static [(&'static str, SvgArt)] {
    static ALL: OnceLock<Vec<(&'static str, SvgArt)>> = OnceLock::new();
    ALL.get_or_init(|| {
        SVG_SOURCES
            .iter()
            .map(|&(name, svg)| {
                let art = parse_svg(svg)
                    .unwrap_or_else(|e| panic!("vendored SVG icon `{name}` failed to parse: {e}"));
                (name, art)
            })
            .collect()
    })
}

/// Look up a parsed SVG icon by canonical name.
pub fn get_svg(name: &str) -> Option<&'static SvgArt> {
    all_parsed()
        .iter()
        .find(|(k, _)| *k == name)
        .map(|(_, art)| art)
}

/// Map a file or folder name + optional `ls` type (`"dir"`, `"file"`, `"symlink"`) to its
/// canonical macOS folder or IntelliJ New UI dark SVG icon name.
pub fn icon_name_for_file(path: &str, kind: Option<&str>) -> &'static str {
    let is_dir = kind.is_some_and(blaze_sniff::is_dir_label) || path.ends_with('/');
    let base_raw = path.trim_end_matches('/').rsplit('/').next().unwrap_or(path);
    if is_dir {
        return match base_raw {
            ".github" => "folderGithub_dark",
            "target" | ".blaze" | "node_modules" | "dist" | "build" | "out" | "__pycache__" => {
                "folder_target"
            }
            _ => "folder_macos",
        };
    }
    let base = base_raw.to_ascii_lowercase();
    match base.as_str() {
        ".git" => return "gitignore",
        "cargo.toml" => return "cargo_toml",
        "cargo.lock" => return "cargo_lock",
        "blaze.lock" => return "blaze_lock",
        "flake.nix" | "default.nix" | "shell.nix" => return "nix_dark",
        "flake.lock"
        | "package-lock.json"
        | "yarn.lock"
        | "bun.lock"
        | "bun.lockb"
        | "pnpm-lock.yaml"
        | "uv.lock"
        | "poetry.lock"
        | "gemfile.lock"
        | "go.sum" => return "lockFile_dark",
        ".gitignore"
        | ".gitattributes"
        | ".gitmodules"
        | ".gitkeep"
        | ".ignore"
        | ".dockerignore"
        | ".npmignore"
        | ".prettierignore" => return "gitignore",
        ".bashrc" | ".zshrc" | ".profile" | ".bash_profile" | ".zprofile" | ".zshenv" | ".envrc" => {
            return "shell_dark";
        }
        "dockerfile"
        | "containerfile"
        | "docker-compose.yml"
        | "docker-compose.yaml"
        | "compose.yml"
        | "compose.yaml" => return "docker_dark",
        ".editorconfig" => return "editorConfig_dark",
        "makefile"
        | "gnumakefile"
        | "justfile"
        | ".env"
        | ".npmrc"
        | ".clang-format"
        | "rustfmt.toml"
        | ".rustfmt.toml" => return "config_dark",
        "gemfile" | "rakefile" => return "ruby_dark",
        _ => {}
    }
    if ["license", "licence", "copying", "notice"]
        .iter()
        .any(|l| base.strip_prefix(l).is_some_and(|r| r.is_empty() || r.starts_with(['-', '.'])))
    {
        return "text_dark";
    }
    if base.starts_with("dockerfile.") || base.ends_with(".dockerfile") {
        return "docker_dark";
    }
    let ext = base.rsplit_once('.').map_or("", |(_, e)| e);
    match ext {
        "rs" => "rust_dark",
        "ts" | "mts" | "cts" | "tsx" => "typeScript_dark",
        "js" | "mjs" | "cjs" | "jsx" => "javaScript_dark",
        "json" | "jsonc" | "json5" | "jsonl" | "ndjson" => "json_dark",
        "toml" => "toml_dark",
        "yaml" | "yml" => "yaml_dark",
        "md" | "mdx" | "markdown" => "markdown_dark",
        "nix" => "nix_dark",
        "sh" | "bash" | "zsh" | "fish" | "nu" => "shell_dark",
        "py" | "pyi" => "python_dark",
        "go" => "go_dark",
        "html" | "htm" => "html_dark",
        "css" | "scss" | "sass" | "less" => "css_dark",
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "ico" | "bmp" | "tiff" | "avif"
        | "heic" => "image_dark",
        "lock" => "lockFile_dark",
        "c" => "c_dark",
        "cc" | "cpp" | "cxx" | "c++" | "mm" => "cpp_dark",
        "h" | "hh" | "hpp" | "hxx" => "h_dark",
        "java" => "java_dark",
        "kt" | "kts" => "kotlin_dark",
        "rb" => "ruby_dark",
        "swift" => "swiftLang_dark",
        "sql" => "sql_dark",
        "xml" | "plist" | "xhtml" => "xml_dark",
        "csv" | "tsv" => "csv_dark",
        "graphql" | "gql" => "graphql_dark",
        "tf" | "tfvars" | "hcl" => "terraform_dark",
        "zip" | "tar" | "gz" | "xz" | "zst" | "bz2" | "7z" | "rar" => "archive_dark",
        "ttf" | "otf" | "woff" | "woff2" => "font_dark",
        "wasm" | "bin" | "o" | "a" | "dylib" | "so" | "dll" | "exe" | "parquet" => "binaryData_dark",
        "ini" | "cfg" | "conf" | "env" => "config_dark",
        "bak" | "backup" | "orig" | "old" | "swp" | "swo" | "tmp" => "backup_dark",
        _ if base_raw.ends_with('~') => "backup_dark",
        _ => "text_dark",
    }
}

// =============================================================================
// Pure-Rust SVG & Path Parser (identical subset to harness-native-render)
// =============================================================================

const KAPPA: f64 = 0.552_284_749_830_793_4;

pub fn parse_svg(svg: &str) -> Result<SvgArt, String> {
    let mut view = None;
    let mut root_fill = Some([0.0, 0.0, 0.0, 1.0]);
    let mut layers = Vec::new();
    let mut rest = svg;
    while let Some(at) = rest.find('<') {
        rest = &rest[at..];
        if let Some((open, close)) =
            [("<!--", "-->"), ("<?", "?>"), ("</", ">")].into_iter().find(|(o, _)| rest.starts_with(o))
        {
            let end = rest.find(close).ok_or_else(|| format!("unclosed {open}"))?;
            rest = &rest[end + close.len()..];
            continue;
        }
        let end = tag_end(rest).ok_or("unclosed tag")?;
        let (tag, after) = (&rest[1..end], &rest[end + 1..]);
        rest = after;
        let tag = tag.strip_suffix('/').unwrap_or(tag);
        let name_end = tag.find(|c: char| c.is_ascii_whitespace()).unwrap_or(tag.len());
        let (name, attrs) = (&tag[..name_end], attributes(&tag[name_end..])?);
        let attr = |k: &str| attrs.iter().find(|(n, _)| *n == k).map(|(_, v)| *v);
        let num = |k: &str| -> Result<f64, String> {
            attr(k).map_or(Ok(0.0), |v| {
                v.trim()
                    .parse::<f64>()
                    .map_err(|_| format!("<{name}> {k}=\"{v}\" is not a number"))
            })
        };
        if name == "title" {
            let close = rest.find("</title>").ok_or("<title> not closed")?;
            rest = &rest[close + "</title>".len()..];
            continue;
        }
        if name == "svg" {
            let v: Vec<f64> = attr("viewBox")
                .ok_or("<svg> has no viewBox")?
                .split(|c: char| c.is_ascii_whitespace() || c == ',')
                .filter(|t| !t.is_empty())
                .map(|t| t.parse::<f64>().map_err(|_| format!("bad viewBox number {t:?}")))
                .collect::<Result<_, _>>()?;
            let [x, y, w, h] = v[..] else {
                return Err(format!("viewBox has {} numbers", v.len()));
            };
            view = Some([x, y, w, h]);
            if let Some(f) = attr("fill") {
                root_fill = parse_color(f)?;
            }
            continue;
        }
        let segs = match name {
            "path" => parse_path_d(attr("d").ok_or("<path> missing d")?)?,
            "rect" => {
                let rx = attr("rx").map(|_| num("rx")).transpose()?;
                let ry = attr("ry").map(|_| num("ry")).transpose()?;
                rect_segs(
                    num("x")?,
                    num("y")?,
                    num("width")?,
                    num("height")?,
                    rx.or(ry).unwrap_or(0.0),
                    ry.or(rx).unwrap_or(0.0),
                )
            }
            "circle" => ellipse_segs(num("cx")?, num("cy")?, num("r")?, num("r")?),
            "ellipse" => ellipse_segs(num("cx")?, num("cy")?, num("rx")?, num("ry")?),
            "line" => vec![Seg::Move([num("x1")?, num("y1")?]), Seg::Line([num("x2")?, num("y2")?])],
            "polyline" | "polygon" => {
                let n: Vec<f64> = attr("points")
                    .unwrap_or("")
                    .split(|c: char| c.is_ascii_whitespace() || c == ',')
                    .filter(|t| !t.is_empty())
                    .map(|t| t.parse::<f64>().map_err(|_| format!("bad point {t:?}")))
                    .collect::<Result<_, _>>()?;
                let mut segs: Vec<Seg> = n
                    .chunks_exact(2)
                    .enumerate()
                    .map(|(i, p)| if i == 0 { Seg::Move([p[0], p[1]]) } else { Seg::Line([p[0], p[1]]) })
                    .collect();
                if name == "polygon" {
                    segs.push(Seg::Close);
                }
                segs
            }
            _ => continue,
        };
        let opacity = attr("opacity").map(|_| num("opacity")).transpose()?.unwrap_or(1.0);
        let faded = |ink: Option<[f32; 4]>, k: &str| -> Result<Option<[f32; 4]>, String> {
            let a = attr(k).map(|_| num(k)).transpose()?.unwrap_or(1.0) * opacity;
            Ok(ink.map(|c| [c[0], c[1], c[2], c[3] * a as f32]).filter(|c| c[3] > 0.0))
        };
        let fill = match attr("fill") {
            Some(f) => parse_color(f)?,
            None => root_fill,
        };
        let stroke = attr("stroke").map(parse_color).transpose()?.flatten();
        let width = attr("stroke-width").map(|_| num("stroke-width")).transpose()?.unwrap_or(1.0);
        let even_odd = matches!(attr("fill-rule"), Some("evenodd"));
        let stroke = faded(stroke, "stroke-opacity")?.filter(|_| width > 0.0).map(|c| (c, width));
        layers.push(Layer { segs, fill: faded(fill, "fill-opacity")?, even_odd, stroke });
    }
    Ok(SvgArt { view: view.ok_or("no <svg>")?, layers })
}

fn tag_end(s: &str) -> Option<usize> {
    let mut quote = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '>') => return Some(i),
            _ => {}
        }
    }
    None
}

fn attributes(mut s: &str) -> Result<Vec<(&str, &str)>, String> {
    let mut out = Vec::new();
    loop {
        s = s.trim_start();
        if s.is_empty() {
            return Ok(out);
        }
        let eq = s.find('=').ok_or_else(|| format!("attr {s:?} has no ="))?;
        let name = s[..eq].trim();
        let v = s[eq + 1..].trim_start();
        let q = v.chars().next().filter(|c| matches!(c, '"' | '\'')).ok_or("unquoted attr")?;
        let end = v[1..].find(q).ok_or("unclosed attr")?;
        out.push((name, &v[1..1 + end]));
        s = &v[end + 2..];
    }
}

fn parse_color(v: &str) -> Result<Option<[f32; 4]>, String> {
    let v = v.trim();
    let hex = |s: &str| {
        u8::from_str_radix(s, 16)
            .map(|b| f32::from(b) / 255.0)
            .map_err(|_| format!("bad hex {v:?}"))
    };
    Ok(Some(match v {
        "none" | "transparent" => return Ok(None),
        "white" | "currentColor" | "currentcolor" => [1.0, 1.0, 1.0, 1.0],
        "black" => [0.0, 0.0, 0.0, 1.0],
        _ => match v.strip_prefix('#') {
            Some(h) if h.len() == 8 && h.is_ascii() => {
                [hex(&h[0..2])?, hex(&h[2..4])?, hex(&h[4..6])?, hex(&h[6..8])?]
            }
            Some(h) if h.len() == 6 && h.is_ascii() => [hex(&h[0..2])?, hex(&h[2..4])?, hex(&h[4..6])?, 1.0],
            Some(h) if h.len() == 3 && h.is_ascii() => {
                let d = |i: usize| hex(&h[i..=i].repeat(2));
                [d(0)?, d(1)?, d(2)?, 1.0]
            }
            _ => return Err(format!("unsupported color {v:?}")),
        },
    }))
}

fn rect_segs(x: f64, y: f64, w: f64, h: f64, rx: f64, ry: f64) -> Vec<Seg> {
    let (rx, ry) = (rx.clamp(0.0, w * 0.5), ry.clamp(0.0, h * 0.5));
    let (kx, ky) = (rx * KAPPA, ry * KAPPA);
    let (r, b) = (x + w, y + h);
    if rx == 0.0 || ry == 0.0 {
        return vec![Seg::Move([x, y]), Seg::Line([r, y]), Seg::Line([r, b]), Seg::Line([x, b]), Seg::Close];
    }
    vec![
        Seg::Move([x + rx, y]),
        Seg::Line([r - rx, y]),
        Seg::Cubic([r - rx + kx, y], [r, y + ry - ky], [r, y + ry]),
        Seg::Line([r, b - ry]),
        Seg::Cubic([r, b - ry + ky], [r - rx + kx, b], [r - rx, b]),
        Seg::Line([x + rx, b]),
        Seg::Cubic([x + rx - kx, b], [x, b - ry + ky], [x, b - ry]),
        Seg::Line([x, y + ry]),
        Seg::Cubic([x, y + ry - ky], [x + rx - kx, y], [x + rx, y]),
        Seg::Close,
    ]
}

fn ellipse_segs(cx: f64, cy: f64, rx: f64, ry: f64) -> Vec<Seg> {
    let (kx, ky) = (rx * KAPPA, ry * KAPPA);
    vec![
        Seg::Move([cx + rx, cy]),
        Seg::Cubic([cx + rx, cy + ky], [cx + kx, cy + ry], [cx, cy + ry]),
        Seg::Cubic([cx - kx, cy + ry], [cx - rx, cy + ky], [cx - rx, cy]),
        Seg::Cubic([cx - rx, cy - ky], [cx - kx, cy - ry], [cx, cy - ry]),
        Seg::Cubic([cx + kx, cy - ry], [cx + rx, cy - ky], [cx + rx, cy]),
        Seg::Close,
    ]
}

struct PathLex<'a> {
    s: &'a [u8],
    i: usize,
}

impl PathLex<'_> {
    fn skip(&mut self) {
        while self.i < self.s.len() && (self.s[self.i].is_ascii_whitespace() || self.s[self.i] == b',') {
            self.i += 1;
        }
    }

    fn command(&mut self) -> Option<u8> {
        self.skip();
        let c = *self.s.get(self.i)?;
        c.is_ascii_alphabetic().then(|| {
            self.i += 1;
            c
        })
    }

    fn more(&mut self) -> bool {
        self.skip();
        self.s.get(self.i).is_some_and(|c| c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.'))
    }

    fn number(&mut self) -> Result<f64, String> {
        self.skip();
        let start = self.i;
        let at = |i: usize| self.s.get(i).copied().unwrap_or(0);
        let mut i = self.i;
        if matches!(at(i), b'-' | b'+') {
            i += 1;
        }
        let mut dot = false;
        while at(i).is_ascii_digit() || (at(i) == b'.' && !dot) {
            dot |= at(i) == b'.';
            i += 1;
        }
        if matches!(at(i), b'e' | b'E')
            && (at(i + 1).is_ascii_digit()
                || (matches!(at(i + 1), b'-' | b'+') && at(i + 2).is_ascii_digit()))
        {
            i += 2;
            while at(i).is_ascii_digit() {
                i += 1;
            }
        }
        self.i = i;
        core::str::from_utf8(&self.s[start..i])
            .ok()
            .and_then(|t| t.parse().ok())
            .ok_or_else(|| format!("bad path number at {start}"))
    }

    fn flag(&mut self) -> Result<bool, String> {
        self.skip();
        let c = self.s.get(self.i).copied();
        self.i += 1;
        match c {
            Some(b'0') => Ok(false),
            Some(b'1') => Ok(true),
            _ => Err(format!("bad arc flag at {}", self.i - 1)),
        }
    }
}

fn parse_path_d(d: &str) -> Result<Vec<Seg>, String> {
    let mut lx = PathLex { s: d.as_bytes(), i: 0 };
    let mut out = Vec::new();
    if d.trim().is_empty() {
        return Ok(out);
    }
    let (mut cur, mut start) = ([0.0f64; 2], [0.0f64; 2]);
    let mut last_c: Option<(u8, [f64; 2])> = None;
    let mut cmd = lx.command().ok_or("path missing command")?;
    loop {
        let rel = cmd.is_ascii_lowercase();
        let up = cmd.to_ascii_uppercase();
        let base = if rel { cur } else { [0.0, 0.0] };
        let pt = |lx: &mut PathLex<'_>| -> Result<[f64; 2], String> {
            Ok([base[0] + lx.number()?, base[1] + lx.number()?])
        };
        let reflect = |kind: u8| match last_c {
            Some((k, c)) if k == kind => [2.0 * cur[0] - c[0], 2.0 * cur[1] - c[1]],
            _ => cur,
        };
        match up {
            b'M' => {
                let p = pt(&mut lx)?;
                out.push(Seg::Move(p));
                (cur, start) = (p, p);
                last_c = None;
                cmd = if rel { b'l' } else { b'L' };
                if lx.more() {
                    continue;
                }
            }
            b'L' => {
                let p = pt(&mut lx)?;
                out.push(Seg::Line(p));
                cur = p;
                last_c = None;
            }
            b'H' => {
                let x = lx.number()? + if rel { cur[0] } else { 0.0 };
                cur = [x, cur[1]];
                out.push(Seg::Line(cur));
                last_c = None;
            }
            b'V' => {
                let y = lx.number()? + if rel { cur[1] } else { 0.0 };
                cur = [cur[0], y];
                out.push(Seg::Line(cur));
                last_c = None;
            }
            b'C' | b'S' => {
                let c1 = if up == b'C' { pt(&mut lx)? } else { reflect(b'C') };
                let c2 = pt(&mut lx)?;
                let p = pt(&mut lx)?;
                out.push(Seg::Cubic(c1, c2, p));
                cur = p;
                last_c = Some((b'C', c2));
            }
            b'Q' | b'T' => {
                let c = if up == b'Q' { pt(&mut lx)? } else { reflect(b'Q') };
                let p = pt(&mut lx)?;
                out.push(Seg::Quad(c, p));
                cur = p;
                last_c = Some((b'Q', c));
            }
            b'A' => {
                let (rx, ry, rot) = (lx.number()?, lx.number()?, lx.number()?);
                let (large, sweep) = (lx.flag()?, lx.flag()?);
                let p = pt(&mut lx)?;
                for [c1, c2, e] in arc_cubics(cur, [rx, ry], rot, large, sweep, p) {
                    out.push(Seg::Cubic(c1, c2, e));
                }
                cur = p;
                last_c = None;
            }
            b'Z' => {
                out.push(Seg::Close);
                cur = start;
                last_c = None;
            }
            _ => return Err(format!("unknown path command {:?}", cmd as char)),
        }
        if up != b'Z' && lx.more() {
            continue;
        }
        match lx.command() {
            Some(c) => cmd = c,
            None if lx.i >= lx.s.len() => break,
            None => return Err(format!("stray path byte at {}", lx.i)),
        }
    }
    Ok(out)
}

fn arc_cubics(
    a: [f64; 2],
    r: [f64; 2],
    rot_deg: f64,
    large: bool,
    sweep: bool,
    b: [f64; 2],
) -> Vec<[[f64; 2]; 3]> {
    let (mut rx, mut ry) = (r[0].abs(), r[1].abs());
    if a == b {
        return Vec::new();
    }
    if rx == 0.0 || ry == 0.0 {
        return vec![[a, b, b]];
    }
    let (sn, cs) = rot_deg.to_radians().sin_cos();
    let (hx, hy) = ((a[0] - b[0]) * 0.5, (a[1] - b[1]) * 0.5);
    let (x1, y1) = (cs * hx + sn * hy, -sn * hx + cs * hy);
    let lam = (x1 * x1) / (rx * rx) + (y1 * y1) / (ry * ry);
    if lam > 1.0 {
        let k = lam.sqrt();
        rx *= k;
        ry *= k;
    }
    let num = (rx * rx * ry * ry - rx * rx * y1 * y1 - ry * ry * x1 * x1).max(0.0);
    let den = rx * rx * y1 * y1 + ry * ry * x1 * x1;
    let mut co = (num / den).sqrt();
    if large == sweep {
        co = -co;
    }
    let (cxp, cyp) = (co * rx * y1 / ry, -co * ry * x1 / rx);
    let (cx, cy) =
        (cs * cxp - sn * cyp + (a[0] + b[0]) * 0.5, sn * cxp + cs * cyp + (a[1] + b[1]) * 0.5);
    let angle = |ux: f64, uy: f64, vx: f64, vy: f64| (ux * vy - uy * vx).atan2(ux * vx + uy * vy);
    let t1 = angle(1.0, 0.0, (x1 - cxp) / rx, (y1 - cyp) / ry);
    let mut dt = angle((x1 - cxp) / rx, (y1 - cyp) / ry, (-x1 - cxp) / rx, (-y1 - cyp) / ry);
    if !sweep && dt > 0.0 {
        dt -= core::f64::consts::TAU;
    } else if sweep && dt < 0.0 {
        dt += core::f64::consts::TAU;
    }
    let n = (dt.abs() / core::f64::consts::FRAC_PI_2).ceil().max(1.0) as usize;
    let step = dt / n as f64;
    let k = 4.0 / 3.0 * (step / 4.0).tan();
    let at = |t: f64| {
        let (s, c) = t.sin_cos();
        [cx + rx * c * cs - ry * s * sn, cy + rx * c * sn + ry * s * cs]
    };
    let dir = |t: f64| {
        let (s, c) = t.sin_cos();
        [-rx * s * cs - ry * c * sn, -rx * s * sn + ry * c * cs]
    };
    let mut out = Vec::with_capacity(n);
    let mut p0 = a;
    for i in 0..n {
        let (ta, tb) = (t1 + step * i as f64, t1 + step * (i + 1) as f64);
        let p3 = if i + 1 == n { b } else { at(tb) };
        let (da, db) = (dir(ta), dir(tb));
        out.push([
            [p0[0] + k * da[0], p0[1] + k * da[1]],
            [p3[0] - k * db[0], p3[1] - k * db[1]],
            p3,
        ]);
        p0 = p3;
    }
    out
}

// =============================================================================
// macOS CoreGraphics Retina RGBA8 Icon Atlas
// =============================================================================

pub const ICON_ATLAS_SIZE: usize = 512;

#[derive(Clone, Copy, Debug)]
pub struct IconSlot {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

#[cfg(target_os = "macos")]
pub struct IconAtlas {
    cell_px: usize,
    slots: HashMap<&'static str, Option<IconSlot>>,
    emoji_slots: HashMap<(String, usize), Option<IconSlot>>,
    custom_slots: HashMap<(String, u64, usize, usize), Option<IconSlot>>,
    next_x: usize,
    next_y: usize,
    row_h: usize,
    /// Newly rasterized `(slot, straight RGBA8 bytes)` waiting for Metal upload.
    pub pending: Vec<(IconSlot, Vec<u8>)>,
}

#[cfg(target_os = "macos")]
impl IconAtlas {
    pub fn new(scale: f64) -> Self {
        let cell_px = (16.0 * scale.max(1.0)).round().clamp(16.0, 64.0) as usize;
        Self {
            cell_px,
            slots: HashMap::new(),
            emoji_slots: HashMap::new(),
            custom_slots: HashMap::new(),
            next_x: 0,
            next_y: 0,
            row_h: 0,
            pending: Vec::new(),
        }
    }

    pub fn rasterize_quads(
        &mut self,
        quads: &[crate::view::IconQuad],
    ) -> Vec<blaze_ui::shaders::GlyphInst> {
        let inv = 1.0 / ICON_ATLAS_SIZE as f32;
        let mut out = Vec::with_capacity(quads.len());
        for q in quads {
            let Some(slot) = self.slot(q.name) else {
                continue;
            };
            out.push(blaze_ui::shaders::GlyphInst {
                rect: [q.x, q.y, q.x + q.w, q.y + q.h],
                uv: [
                    slot.x as f32 * inv,
                    slot.y as f32 * inv,
                    (slot.x + slot.w) as f32 * inv,
                    (slot.y + slot.h) as f32 * inv,
                ],
                color: [1.0, 1.0, 1.0, q.alpha],
                clip_info: [q.clip_id, 0, 0, 0],
            });
        }
        out
    }

    /// Rasterize full-color Apple Color Emoji / custom Unicode symbols (e.g. `"🦑"`, `"👋🌎"`)
    /// directly into the RGBA8 Metal icon atlas and append their quads into `out`.
    pub fn rasterize_emoji_quads_into(
        &mut self,
        quads: &[crate::view::EmojiQuad],
        out: &mut Vec<blaze_ui::shaders::GlyphInst>,
    ) {
        let inv = 1.0 / ICON_ATLAS_SIZE as f32;
        for q in quads {
            if q.text.is_empty() || q.h <= 0.0 {
                continue;
            }
            let aspect = (q.w / q.h).clamp(0.8, 6.0);
            let w_px = ((self.cell_px as f32) * aspect).round().clamp(16.0, 224.0) as usize;
            let Some(slot) = self.emoji_slot(&q.text, w_px) else {
                continue;
            };
            out.push(blaze_ui::shaders::GlyphInst {
                rect: [q.x, q.y, q.x + q.w, q.y + q.h],
                uv: [
                    slot.x as f32 * inv,
                    slot.y as f32 * inv,
                    (slot.x + slot.w) as f32 * inv,
                    (slot.y + slot.h) as f32 * inv,
                ],
                color: [1.0, 1.0, 1.0, q.alpha],
                clip_info: [q.clip_id, 0, 0, 0],
            });
        }
    }

    /// Rasterize custom symbols registered via the Blaze Terminal API (SVG, in-memory font glyph,
    /// PNG bitmap, or composite text) into the RGBA8 Metal icon atlas and append their quads into `out`.
    pub fn rasterize_custom_symbol_quads_into(
        &mut self,
        quads: &[crate::view::CustomSymbolQuad],
        out: &mut Vec<blaze_ui::shaders::GlyphInst>,
    ) {
        let inv = 1.0 / ICON_ATLAS_SIZE as f32;
        for q in quads {
            if q.name.is_empty() || q.w <= 0.0 || q.h <= 0.0 {
                continue;
            }
            let Some(sym) = crate::term_server::lookup_global_symbol(&q.name) else {
                continue;
            };
            let h_px = self.cell_px;
            let aspect = (q.w / q.h).clamp(0.5, 4.0);
            let w_px = ((h_px as f32) * aspect).round().clamp(16.0, 256.0) as usize;
            let Some(slot) = self.custom_symbol_slot(&sym, w_px, h_px) else {
                continue;
            };
            let color = if let Some(t) = q.tint {
                [t[0], t[1], t[2], t[3] * q.alpha]
            } else {
                [1.0, 1.0, 1.0, q.alpha]
            };
            out.push(blaze_ui::shaders::GlyphInst {
                rect: [q.x, q.y, q.x + q.w, q.y + q.h],
                uv: [
                    slot.x as f32 * inv,
                    slot.y as f32 * inv,
                    (slot.x + slot.w) as f32 * inv,
                    (slot.y + slot.h) as f32 * inv,
                ],
                color,
                clip_info: [q.clip_id, 0, 0, 0],
            });
        }
    }

    fn custom_symbol_slot(
        &mut self,
        sym: &crate::term_server::SymbolSnapshot,
        w_px: usize,
        h_px: usize,
    ) -> Option<IconSlot> {
        let key = (sym.name.clone(), sym.revision, w_px, h_px);
        if let Some(&cached) = self.custom_slots.get(&key) {
            return cached;
        }
        let rgba = match &sym.source {
            crate::term_server::SymbolSourceSnapshot::Svg { markup, monochrome } => parse_svg(markup)
                .ok()
                .and_then(|art| rasterize_svg_art_rgba(&art, w_px, h_px, *monochrome)),
            crate::term_server::SymbolSourceSnapshot::FontGlyph {
                font_name,
                font_data,
                codepoint,
                tint,
            } => crate::image_atlas::rasterize_custom_font_glyph_rgba(
                font_name.as_deref(),
                font_data.as_deref(),
                *codepoint,
                w_px,
                h_px,
                *tint,
            ),
            crate::term_server::SymbolSourceSnapshot::Png { data } => {
                crate::image_atlas::rasterize_png_bytes_rgba(data, w_px, h_px)
            }
            crate::term_server::SymbolSourceSnapshot::Text { text } => {
                rasterize_coretext_rgba(text, w_px, h_px)
            }
        };
        let slot = rgba.and_then(|bytes| {
            let s = self.alloc_rect(w_px, h_px)?;
            self.pending.push((s, bytes));
            Some(s)
        });
        self.custom_slots.insert(key, slot);
        slot
    }

    fn alloc_rect(&mut self, w_px: usize, h_px: usize) -> Option<IconSlot> {
        let step_w = w_px + 2;
        let step_h = h_px + 2;
        if step_w > ICON_ATLAS_SIZE || step_h > ICON_ATLAS_SIZE {
            return None;
        }
        if self.next_x + step_w > ICON_ATLAS_SIZE {
            self.next_x = 0;
            self.next_y += self.row_h;
            self.row_h = 0;
        }
        if self.next_y + step_h > ICON_ATLAS_SIZE {
            return None;
        }
        let slot = IconSlot { x: self.next_x + 1, y: self.next_y + 1, w: w_px, h: h_px };
        self.next_x += step_w;
        self.row_h = self.row_h.max(step_h);
        Some(slot)
    }

    fn emoji_slot(&mut self, text: &str, w_px: usize) -> Option<IconSlot> {
        let key = (text.to_owned(), w_px);
        if let Some(&cached) = self.emoji_slots.get(&key) {
            return cached;
        }
        let s = self.rasterize_emoji(text, w_px);
        self.emoji_slots.insert(key, s);
        s
    }

    fn rasterize_emoji(&mut self, text: &str, w_px: usize) -> Option<IconSlot> {
        let h_px = self.cell_px;
        let rgba = rasterize_coretext_rgba(text, w_px, h_px)?;
        let slot = self.alloc_rect(w_px, h_px)?;
        self.pending.push((slot, rgba));
        Some(slot)
    }

    fn slot(&mut self, name: &'static str) -> Option<IconSlot> {
        if let Some(&cached) = self.slots.get(name) {
            return cached;
        }
        let s = self.rasterize(name);
        self.slots.insert(name, s);
        s
    }

    fn rasterize(&mut self, name: &'static str) -> Option<IconSlot> {
        let art = get_svg(name)?;
        let sz = self.cell_px;
        let buf = rasterize_svg_art_rgba(art, sz, sz, false)?;
        let slot = self.alloc_rect(sz, sz)?;
        self.pending.push((slot, buf));
        Some(slot)
    }
}

/// Rasterize a parsed [`SvgArt`] at Retina resolution (`w_px x h_px`) into straight-RGBA8 bytes
/// using CoreGraphics. When `monochrome` is true, every fill/stroke renders white RGB while
/// preserving layer alpha so Metal's `GlyphInst.color` tints it on the GPU.
#[cfg(target_os = "macos")]
fn rasterize_svg_art_rgba(
    art: &SvgArt,
    w_px: usize,
    h_px: usize,
    monochrome: bool,
) -> Option<Vec<u8>> {
    let [vx, vy, vw, vh] = art.view;
    if w_px == 0 || h_px == 0 || vw <= 0.0 || vh <= 0.0 {
        return None;
    }

    let mut buf = vec![0u8; w_px * h_px * 4];
    {
        use objc2_core_graphics::{
            CGBitmapContextCreate, CGColorSpace, CGContext, CGLineCap, CGLineJoin,
        };

        let space = CGColorSpace::new_device_rgb()?;
        // kCGImageAlphaPremultipliedLast (1) | kCGBitmapByteOrder32Big (0).
        // SAFETY: `buf` holds `w_px * h_px * 4` bytes and lives past context drop.
        let ctx = unsafe {
            CGBitmapContextCreate(buf.as_mut_ptr().cast(), w_px, h_px, 8, w_px * 4, Some(&space), 1)
        }?;
        CGContext::set_should_antialias(Some(&ctx), true);
        CGContext::set_allows_antialiasing(Some(&ctx), true);
        CGContext::set_line_cap(Some(&ctx), CGLineCap::Round);
        CGContext::set_line_join(Some(&ctx), CGLineJoin::Round);

        let s = (w_px as f64 / vw).min(h_px as f64 / vh);
        let ox = (w_px as f64 - vw * s) * 0.5 - vx * s;
        let oy = (h_px as f64 - vh * s) * 0.5 - vy * s;
        // CoreGraphics origin is bottom-left; SVG y goes down, so flip y into [0, h_px].
        let map = |p: [f64; 2]| -> (f64, f64) { (ox + p[0] * s, h_px as f64 - (oy + p[1] * s)) };

        for layer in &art.layers {
            let emit_path = |ctx: &CGContext| {
                CGContext::begin_path(Some(ctx));
                for seg in &layer.segs {
                    match *seg {
                        Seg::Move(p) => {
                            let (x, y) = map(p);
                            CGContext::move_to_point(Some(ctx), x, y);
                        }
                        Seg::Line(p) => {
                            let (x, y) = map(p);
                            CGContext::add_line_to_point(Some(ctx), x, y);
                        }
                        Seg::Quad(c, p) => {
                            let (cx, cy) = map(c);
                            let (x, y) = map(p);
                            CGContext::add_quad_curve_to_point(Some(ctx), cx, cy, x, y);
                        }
                        Seg::Cubic(c1, c2, p) => {
                            let (x1, y1) = map(c1);
                            let (x2, y2) = map(c2);
                            let (x, y) = map(p);
                            CGContext::add_curve_to_point(Some(ctx), x1, y1, x2, y2, x, y);
                        }
                        Seg::Close => CGContext::close_path(Some(ctx)),
                    }
                }
            };

            if let Some(ink) = layer.fill {
                emit_path(&ctx);
                let (r, g, b, a) = if monochrome {
                    (1.0, 1.0, 1.0, f64::from(ink[3]))
                } else {
                    (f64::from(ink[0]), f64::from(ink[1]), f64::from(ink[2]), f64::from(ink[3]))
                };
                CGContext::set_rgb_fill_color(Some(&ctx), r, g, b, a);
                if layer.even_odd {
                    CGContext::eo_fill_path(Some(&ctx));
                } else {
                    CGContext::fill_path(Some(&ctx));
                }
            }
            if let Some((ink, w)) = layer.stroke {
                emit_path(&ctx);
                let (r, g, b, a) = if monochrome {
                    (1.0, 1.0, 1.0, f64::from(ink[3]))
                } else {
                    (f64::from(ink[0]), f64::from(ink[1]), f64::from(ink[2]), f64::from(ink[3]))
                };
                CGContext::set_rgb_stroke_color(Some(&ctx), r, g, b, a);
                CGContext::set_line_width(Some(&ctx), (w * s).max(0.75));
                CGContext::stroke_path(Some(&ctx));
            }
        }
    }

    // Convert premultiplied RGBA from CoreGraphics into straight RGBA for `ui_sprite_fs`.
    for px in buf.chunks_exact_mut(4) {
        let a = px[3];
        if a == 0 {
            px[0] = 0;
            px[1] = 0;
            px[2] = 0;
        } else if a < 255 {
            let af = f32::from(a) / 255.0;
            px[0] = ((f32::from(px[0]) / af).round().clamp(0.0, 255.0)) as u8;
            px[1] = ((f32::from(px[1]) / af).round().clamp(0.0, 255.0)) as u8;
            px[2] = ((f32::from(px[2]) / af).round().clamp(0.0, 255.0)) as u8;
        }
    }

    Some(buf)
}

/// Rasterize a UTF-8 emoji or custom symbol string (`"🦑"`, `"👋🌎"`, `"🔥"`, `"λ"`) into a
/// straight-RGBA8 Retina bitmap using CoreText + Apple Color Emoji cascade.
#[cfg(target_os = "macos")]
fn rasterize_coretext_rgba(text: &str, w_px: usize, h_px: usize) -> Option<Vec<u8>> {
    crate::image_atlas::rasterize_coretext_rgba(text, w_px, h_px)
}
