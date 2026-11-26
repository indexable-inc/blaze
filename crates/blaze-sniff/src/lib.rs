//! `blaze-sniff`: `#![no_std]` `#![blaze::sans_io]` `#![forbid(unsafe_code)]` zero-allocation
//! file & directory analyzer (magic bytes, Mach-O/ELF/WASM/PNG/Parquet/SQLite/archives,
//! shebangs, Git worktree links, semantic dotfiles, extensions, and directory markers).

#![no_std]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![forbid(unsafe_code)]

use core::fmt::{self, Write};

/// Recommended prefix size to read from a regular file before calling [`sniff_file`].
pub const SNIFF_HEAD_BYTES: usize = 512;

/// Coarse semantic category for styling and grouping in UI tables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileCategory {
    Directory,
    Symlink,
    Code,
    Config,
    Data,
    Document,
    Executable,
    Image,
    Archive,
    Media,
    Binary,
    Text,
}

/// Optional cheap filesystem markers for classifying a directory (no IO inside `blaze-sniff`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DirMarkers {
    pub has_cargo_toml: bool,
    pub has_package_json: bool,
    pub has_pyproject: bool,
    pub has_go_mod: bool,
    pub has_flake_nix: bool,
    pub has_git: bool,
}

/// Rich structured classification returned by [`sniff_file`] / [`sniff_dir`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SniffResult {
    pub category: FileCategory,
    /// Concise human label (e.g. `"Git worktree"`, `"Clippy config"`, `"Rust"`, `"Mach-O arm64"`, `"PNG"`).
    pub label: &'static str,
    /// Optional image pixel dimensions `(width, height)` parsed directly from PNG/GIF/JPEG headers.
    pub dims: Option<(u32, u32)>,
    /// True when content appears binary (magic header or NUL byte in prefix).
    pub is_binary: bool,
}

impl SniffResult {
    /// Format a concise display label (including `W×H` when image dimensions are known).
    pub fn format_label(&self, out: &mut dyn Write) -> fmt::Result {
        out.write_str(self.label)?;
        if let Some((w, h)) = self.dims
            && w > 0
            && h > 0
        {
            write!(out, " {w}×{h}")?;
        }
        Ok(())
    }
}

/// Classify a directory by its name and optional child-marker hints (zero IO, zero alloc).
pub fn sniff_dir(name: &str, markers: DirMarkers) -> SniffResult {
    let base = basename(name);
    let label = match base {
        ".git" => "Git repo",
        ".github" => "GitHub",
        ".cargo" => "Cargo config",
        ".claude" | ".claude-plugin" | ".agents" | ".cursor" | ".codex" => "Agent config",
        ".vscode" | ".idea" => "IDE config",
        ".blaze" => "Blaze cache",
        "target" | "build" | "dist" | "out" | "node_modules" | "__pycache__" => "Build output",
        "crates" => "Workspace",
        "docs" | "doc" => "Docs",
        "scripts" | "tools" | "bin" => "Tools",
        "tests" | "test" | "benches" => "Tests",
        "src" | "lib" | "library" => "Source",
        "vendor" | "third_party" => "Vendor",
        "plugins" => "Plugins",
        "site" | "web" | "public" => "Web",
        "licenses" => "Licenses",
        _ if markers.has_cargo_toml => "Rust crate",
        _ if markers.has_package_json => "Node pkg",
        _ if markers.has_pyproject => "Python pkg",
        _ if markers.has_go_mod => "Go module",
        _ if markers.has_flake_nix => "Nix project",
        _ if markers.has_git => "Git repo",
        _ => "Folder",
    };
    SniffResult {
        category: FileCategory::Directory,
        label,
        dims: None,
        is_binary: false,
    }
}

/// True when `label` is a directory classification produced by [`sniff_dir`] or `"dir"`.
pub fn is_dir_label(label: &str) -> bool {
    matches!(
        label,
        "dir"
            | "Folder"
            | "Git repo"
            | "GitHub"
            | "Cargo config"
            | "Agent config"
            | "IDE config"
            | "Blaze cache"
            | "Build output"
            | "Workspace"
            | "Docs"
            | "Tools"
            | "Tests"
            | "Source"
            | "Vendor"
            | "Plugins"
            | "Web"
            | "Licenses"
            | "Rust crate"
            | "Node pkg"
            | "Python pkg"
            | "Go module"
            | "Nix project"
    )
}

/// Classify a symlink entry.
pub fn sniff_symlink() -> SniffResult {
    SniffResult {
        category: FileCategory::Symlink,
        label: "Symlink",
        dims: None,
        is_binary: false,
    }
}

/// Classify a regular file from its filename and up to [`SNIFF_HEAD_BYTES`] of header bytes.
pub fn sniff_file(name: &str, head: &[u8]) -> SniffResult {
    let base = basename(name);

    // 1. High-confidence binary magic headers (beats misleading extensions!).
    if let Some(m) = sniff_magic(head, base) {
        return m;
    }

    // 2. Git worktree link file (`.git` regular file starting with `gitdir:`).
    if base == ".git" && head.starts_with(b"gitdir:") {
        return SniffResult {
            category: FileCategory::Config,
            label: "Git worktree",
            dims: None,
            is_binary: false,
        };
    }

    // 3. Exact semantic filenames (dotfiles, manifests, lockfiles, licenses, build scripts).
    if let Some(f) = sniff_exact_filename(base) {
        return f;
    }

    // 4. Executable shebang (`#!...`).
    if let Some(sh) = sniff_shebang(head) {
        return sh;
    }

    // 5. File extension mapping.
    if let Some(ext_res) = sniff_extension(base) {
        return ext_res;
    }

    // 6. Content fallback: NUL-byte binary check vs UTF-8 text.
    if head.is_empty() {
        return SniffResult {
            category: FileCategory::Text,
            label: "Empty",
            dims: None,
            is_binary: false,
        };
    }
    if looks_binary(head) {
        SniffResult {
            category: FileCategory::Binary,
            label: "Binary",
            dims: None,
            is_binary: true,
        }
    } else if head.starts_with(b"{") || head.starts_with(b"[") {
        SniffResult {
            category: FileCategory::Data,
            label: "JSON",
            dims: None,
            is_binary: false,
        }
    } else {
        SniffResult {
            category: FileCategory::Text,
            label: "Plain text",
            dims: None,
            is_binary: false,
        }
    }
}

fn sniff_magic(head: &[u8], base: &str) -> Option<SniffResult> {
    if head.len() < 4 {
        return None;
    }

    // Mach-O 64-bit / 32-bit / Universal (macOS binaries & dylibs).
    match &head[..4] {
        [0xCF, 0xFA, 0xED, 0xFE] | [0xFE, 0xED, 0xFA, 0xCF] => {
            let cpu = if head.len() >= 8 {
                if head[0] == 0xCF {
                    u32::from_le_bytes([head[4], head[5], head[6], head[7]])
                } else {
                    u32::from_be_bytes([head[4], head[5], head[6], head[7]])
                }
            } else {
                0
            };
            let label = match cpu {
                0x0100_000C => "Mach-O arm64",
                0x0100_0007 => "Mach-O x86_64",
                _ => "Mach-O 64",
            };
            return Some(SniffResult {
                category: FileCategory::Executable,
                label,
                dims: None,
                is_binary: true,
            });
        }
        [0xCA, 0xFE, 0xBA, 0xBE] | [0xBE, 0xBA, 0xFE, 0xCA] => {
            // Distinguish Mach-O fat binary from Java `.class`.
            let is_java = ends_with_ignore_ascii_case(base, ".class");
            return Some(SniffResult {
                category: FileCategory::Executable,
                label: if is_java { "Java bytecode" } else { "Mach-O universal" },
                dims: None,
                is_binary: true,
            });
        }
        // ELF binary (`\x7FELF`)
        [0x7F, b'E', b'L', b'F'] => {
            let label = if head.len() >= 20 {
                let le = head[5] == 1;
                let mach = if le {
                    u16::from_le_bytes([head[18], head[19]])
                } else {
                    u16::from_be_bytes([head[18], head[19]])
                };
                match mach {
                    183 => "ELF arm64",
                    62 => "ELF x86_64",
                    243 => "ELF riscv",
                    _ => "ELF binary",
                }
            } else {
                "ELF binary"
            };
            return Some(SniffResult {
                category: FileCategory::Executable,
                label,
                dims: None,
                is_binary: true,
            });
        }
        // WebAssembly (`\0asm`)
        [0x00, b'a', b's', b'm'] => {
            return Some(SniffResult {
                category: FileCategory::Executable,
                label: "WebAssembly",
                dims: None,
                is_binary: true,
            });
        }
        // PNG (`89 50 4E 47 0D 0A 1A 0A`) + IHDR width/height!
        [0x89, b'P', b'N', b'G'] if head.len() >= 8 && &head[4..8] == b"\r\n\x1a\n" => {
            let dims = if head.len() >= 24 && &head[12..16] == b"IHDR" {
                let w = u32::from_be_bytes([head[16], head[17], head[18], head[19]]);
                let h = u32::from_be_bytes([head[20], head[21], head[22], head[23]]);
                Some((w, h))
            } else {
                None
            };
            return Some(SniffResult {
                category: FileCategory::Image,
                label: "PNG",
                dims,
                is_binary: true,
            });
        }
        // JPEG (`FF D8 FF`)
        [0xFF, 0xD8, 0xFF, _] => {
            return Some(SniffResult {
                category: FileCategory::Image,
                label: "JPEG",
                dims: None,
                is_binary: true,
            });
        }
        // GIF87a / GIF89a + width/height!
        [b'G', b'I', b'F', b'8'] if head.len() >= 10 => {
            let w = u16::from_le_bytes([head[6], head[7]]) as u32;
            let h = u16::from_le_bytes([head[8], head[9]]) as u32;
            return Some(SniffResult {
                category: FileCategory::Image,
                label: "GIF",
                dims: Some((w, h)),
                is_binary: true,
            });
        }
        // RIFF (WebP / WAV)
        [b'R', b'I', b'F', b'F'] if head.len() >= 12 => {
            if &head[8..12] == b"WEBP" {
                return Some(SniffResult {
                    category: FileCategory::Image,
                    label: "WebP",
                    dims: None,
                    is_binary: true,
                });
            }
            if &head[8..12] == b"WAVE" {
                return Some(SniffResult {
                    category: FileCategory::Media,
                    label: "WAV audio",
                    dims: None,
                    is_binary: true,
                });
            }
        }
        // PDF (`%PDF`)
        [b'%', b'P', b'D', b'F'] => {
            return Some(SniffResult {
                category: FileCategory::Document,
                label: "PDF",
                dims: None,
                is_binary: true,
            });
        }
        // Zstandard (`28 B5 2F FD`)
        [0x28, 0xB5, 0x2F, 0xFD] => {
            return Some(SniffResult {
                category: FileCategory::Archive,
                label: "Zstd",
                dims: None,
                is_binary: true,
            });
        }
        // Zip / JAR / DOCX (`PK\x03\x04`)
        [b'P', b'K', 0x03, 0x04] | [b'P', b'K', 0x05, 0x06] => {
            let label = if ends_with_ignore_ascii_case(base, ".jar") {
                "JAR archive"
            } else if ends_with_ignore_ascii_case(base, ".docx") {
                "Word doc"
            } else if ends_with_ignore_ascii_case(base, ".xlsx") {
                "Excel sheet"
            } else {
                "Zip archive"
            };
            return Some(SniffResult {
                category: FileCategory::Archive,
                label,
                dims: None,
                is_binary: true,
            });
        }
        // Apache Parquet (`PAR1`)
        [b'P', b'A', b'R', b'1'] => {
            return Some(SniffResult {
                category: FileCategory::Data,
                label: "Parquet",
                dims: None,
                is_binary: true,
            });
        }
        // Blaze PGO profile (`BPG1`)
        [b'B', b'P', b'G', b'1'] => {
            return Some(SniffResult {
                category: FileCategory::Data,
                label: "Blaze PGO",
                dims: None,
                is_binary: true,
            });
        }
        // WOFF2 font
        [b'w', b'O', b'F', b'2'] => {
            return Some(SniffResult {
                category: FileCategory::Binary,
                label: "WOFF2 font",
                dims: None,
                is_binary: true,
            });
        }
        // FLAC audio
        [b'f', b'L', b'a', b'C'] => {
            return Some(SniffResult {
                category: FileCategory::Media,
                label: "FLAC audio",
                dims: None,
                is_binary: true,
            });
        }
        _ => {}
    }

    // Gzip (`1F 8B`)
    if head[0] == 0x1F && head[1] == 0x8B {
        let label = if ends_with_ignore_ascii_case(base, ".tar.gz")
            || ends_with_ignore_ascii_case(base, ".tgz")
        {
            "Tarball (gz)"
        } else {
            "Gzip"
        };
        return Some(SniffResult {
            category: FileCategory::Archive,
            label,
            dims: None,
            is_binary: true,
        });
    }
    // XZ (`FD 37 7A 58 5A 00`)
    if head.len() >= 6 && head.starts_with(&[0xFD, 0x37, 0x7A, 0x58, 0x5A, 0x00]) {
        return Some(SniffResult {
            category: FileCategory::Archive,
            label: "XZ archive",
            dims: None,
            is_binary: true,
        });
    }
    // SQLite 3 (`SQLite format 3\0`)
    if head.starts_with(b"SQLite format 3\0") {
        return Some(SniffResult {
            category: FileCategory::Data,
            label: "SQLite DB",
            dims: None,
            is_binary: true,
        });
    }
    // POSIX tar (`ustar` at offset 257)
    if head.len() >= 262 && &head[257..262] == b"ustar" {
        return Some(SniffResult {
            category: FileCategory::Archive,
            label: "Tar archive",
            dims: None,
            is_binary: true,
        });
    }
    // Unix `ar` archive (`.a` / `.rlib`)
    if head.starts_with(b"!<arch>\n") {
        let label = if ends_with_ignore_ascii_case(base, ".rlib") {
            "Rust rlib"
        } else {
            "Static lib"
        };
        return Some(SniffResult {
            category: FileCategory::Executable,
            label,
            dims: None,
            is_binary: true,
        });
    }
    // Windows PE (`MZ`)
    if head[0] == b'M' && head[1] == b'Z' && (ends_with_ignore_ascii_case(base, ".exe") || ends_with_ignore_ascii_case(base, ".dll")) {
        return Some(SniffResult {
            category: FileCategory::Executable,
            label: "PE binary",
            dims: None,
            is_binary: true,
        });
    }

    None
}

fn sniff_exact_filename(base: &str) -> Option<SniffResult> {
    let (cat, label) = match base {
        ".clippy.toml" | "clippy.toml" => (FileCategory::Config, "Clippy config"),
        ".rustfmt.toml" | "rustfmt.toml" => (FileCategory::Config, "Rustfmt config"),
        ".gitignore" => (FileCategory::Config, "Git ignore"),
        ".gitattributes" => (FileCategory::Config, "Git attrs"),
        ".gitmodules" => (FileCategory::Config, "Git modules"),
        ".python-version" => (FileCategory::Config, "Python pin"),
        ".node-version" | ".nvmrc" => (FileCategory::Config, "Node pin"),
        ".ruby-version" => (FileCategory::Config, "Ruby pin"),
        ".editorconfig" => (FileCategory::Config, "EditorConfig"),
        ".dockerignore" => (FileCategory::Config, "Docker ignore"),
        ".env" | ".env.local" | ".env.example" | ".env.production" => (FileCategory::Config, "Env config"),
        "Cargo.toml" => (FileCategory::Config, "Cargo manifest"),
        "Cargo.lock" => (FileCategory::Data, "Cargo lock"),
        "blaze.lock" => (FileCategory::Data, "Blaze lock"),
        "uv.lock" => (FileCategory::Data, "uv lock"),
        "bun.lock" | "bun.lockb" => (FileCategory::Data, "Bun lock"),
        "flake.nix" => (FileCategory::Config, "Nix flake"),
        "flake.lock" => (FileCategory::Data, "Nix lock"),
        "pyproject.toml" => (FileCategory::Config, "Python proj"),
        "package.json" => (FileCategory::Config, "Node manifest"),
        "tsconfig.json" => (FileCategory::Config, "TS config"),
        "justfile" | "Justfile" => (FileCategory::Code, "Justfile"),
        "Makefile" | "makefile" | "GNUmakefile" => (FileCategory::Code, "Makefile"),
        "Dockerfile" => (FileCategory::Config, "Dockerfile"),
        "CMakeLists.txt" => (FileCategory::Config, "CMake"),
        "go.mod" => (FileCategory::Config, "Go module"),
        "go.sum" => (FileCategory::Data, "Go checksums"),
        "BUILD" | "BUILD.bazel" | "WORKSPACE" => (FileCategory::Config, "Bazel"),
        "rust-toolchain" | "rust-toolchain.toml" => (FileCategory::Config, "Rust pin"),
        _ if starts_with_ignore_ascii_case(base, "license")
            || starts_with_ignore_ascii_case(base, "copying") =>
        {
            (FileCategory::Document, "License")
        }
        _ if starts_with_ignore_ascii_case(base, "dockerfile.") => {
            (FileCategory::Config, "Dockerfile")
        }
        _ => return None,
    };
    Some(SniffResult {
        category: cat,
        label,
        dims: None,
        is_binary: false,
    })
}

fn sniff_shebang(head: &[u8]) -> Option<SniffResult> {
    if !head.starts_with(b"#!") {
        return None;
    }
    let end = head.iter().position(|&b| b == b'\n').unwrap_or(head.len()).min(120);
    let line = core::str::from_utf8(&head[2..end]).ok()?.trim();
    let label = if line.contains("python") {
        "Python script"
    } else if line.contains("bun") {
        "Bun script"
    } else if line.contains("node") || line.contains("deno") {
        "JS script"
    } else if line.contains("bash") || line.contains("zsh") || line.contains("fish") || line.ends_with("/sh") || line.ends_with(" sh") {
        "Shell script"
    } else if line.contains("ruby") {
        "Ruby script"
    } else if line.contains("perl") {
        "Perl script"
    } else if line.contains("nix") {
        "Nix script"
    } else {
        "Script"
    };
    Some(SniffResult {
        category: FileCategory::Code,
        label,
        dims: None,
        is_binary: false,
    })
}

fn sniff_extension(base: &str) -> Option<SniffResult> {
    let dot = base.rfind('.')?;
    if dot == 0 || dot + 1 >= base.len() {
        return None;
    }
    let ext = &base[dot + 1..];
    let mut lower = [0u8; 16];
    if ext.len() > lower.len() {
        return None;
    }
    for (i, b) in ext.bytes().enumerate() {
        lower[i] = b.to_ascii_lowercase();
    }
    let ext_lc = core::str::from_utf8(&lower[..ext.len()]).ok()?;

    let (cat, label, is_bin) = match ext_lc {
        "rs" => (FileCategory::Code, "Rust", false),
        "ts" => (FileCategory::Code, "TypeScript", false),
        "tsx" => (FileCategory::Code, "TSX", false),
        "js" | "mjs" | "cjs" => (FileCategory::Code, "JavaScript", false),
        "jsx" => (FileCategory::Code, "JSX", false),
        "py" | "pyi" => (FileCategory::Code, "Python", false),
        "go" => (FileCategory::Code, "Go", false),
        "c" => (FileCategory::Code, "C", false),
        "h" => (FileCategory::Code, "C header", false),
        "cc" | "cpp" | "cxx" => (FileCategory::Code, "C++", false),
        "hpp" | "hh" => (FileCategory::Code, "C++ header", false),
        "zig" => (FileCategory::Code, "Zig", false),
        "lean" => (FileCategory::Code, "Lean 4", false),
        "flecs" => (FileCategory::Code, "Flecs", false),
        "sh" | "bash" | "zsh" | "fish" => (FileCategory::Code, "Shell", false),
        "nix" => (FileCategory::Config, "Nix", false),
        "lua" => (FileCategory::Code, "Lua", false),
        "rb" => (FileCategory::Code, "Ruby", false),
        "java" => (FileCategory::Code, "Java", false),
        "kt" | "kts" => (FileCategory::Code, "Kotlin", false),
        "swift" => (FileCategory::Code, "Swift", false),
        "m" | "mm" => (FileCategory::Code, "Obj-C", false),
        "sql" => (FileCategory::Code, "SQL", false),
        "wgsl" | "metal" | "glsl" | "hlsl" => (FileCategory::Code, "GPU shader", false),
        "asm" | "s" => (FileCategory::Code, "Assembly", false),
        "toml" => (FileCategory::Config, "TOML", false),
        "json" => (FileCategory::Data, "JSON", false),
        "jsonl" | "ndjson" => (FileCategory::Data, "JSONL", false),
        "yaml" | "yml" => (FileCategory::Config, "YAML", false),
        "ini" | "cfg" | "conf" => (FileCategory::Config, "Config", false),
        "xml" | "plist" => (FileCategory::Config, "XML", false),
        "csv" => (FileCategory::Data, "CSV", false),
        "tsv" => (FileCategory::Data, "TSV", false),
        "parquet" => (FileCategory::Data, "Parquet", true),
        "sqlite" | "db" => (FileCategory::Data, "Database", true),
        "md" | "mdx" | "markdown" => (FileCategory::Document, "Markdown", false),
        "txt" => (FileCategory::Document, "Plain text", false),
        "rst" => (FileCategory::Document, "reST doc", false),
        "html" | "htm" => (FileCategory::Code, "HTML", false),
        "css" | "scss" => (FileCategory::Code, "CSS", false),
        "svg" => (FileCategory::Image, "SVG", false),
        "png" => (FileCategory::Image, "PNG", true),
        "jpg" | "jpeg" => (FileCategory::Image, "JPEG", true),
        "webp" => (FileCategory::Image, "WebP", true),
        "gif" => (FileCategory::Image, "GIF", true),
        "ico" => (FileCategory::Image, "Icon", true),
        "pdf" => (FileCategory::Document, "PDF", true),
        "lock" => (FileCategory::Data, "Lockfile", false),
        "log" => (FileCategory::Document, "Log", false),
        "bak" | "backup" | "orig" | "old" | "swp" | "swo" | "tmp" => {
            (FileCategory::Document, "Backup", false)
        }
        "patch" | "diff" => (FileCategory::Document, "Patch", false),
        "wasm" => (FileCategory::Executable, "WebAssembly", true),
        "dylib" | "so" => (FileCategory::Executable, "Shared lib", true),
        "a" | "rlib" => (FileCategory::Executable, "Static lib", true),
        "o" | "obj" => (FileCategory::Executable, "Object", true),
        "zst" | "gz" | "tar" | "zip" | "xz" | "bz2" => (FileCategory::Archive, "Archive", true),
        "ttf" | "otf" | "woff" | "woff2" => (FileCategory::Binary, "Font", true),
        "mp4" | "mov" | "mkv" | "webm" => (FileCategory::Media, "Video", true),
        "mp3" | "wav" | "flac" | "ogg" => (FileCategory::Media, "Audio", true),
        _ => return None,
    };
    Some(SniffResult {
        category: cat,
        label,
        dims: None,
        is_binary: is_bin,
    })
}

fn looks_binary(head: &[u8]) -> bool {
    let check = &head[..head.len().min(512)];
    let mut non_text = 0usize;
    for &b in check {
        if b == 0 {
            return true;
        }
        if b < 0x09 || (b > 0x0D && b < 0x20 && b != 0x1B) {
            non_text += 1;
        }
    }
    non_text * 10 > check.len()
}

fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or(trimmed)
}

fn starts_with_ignore_ascii_case(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

fn ends_with_ignore_ascii_case(s: &str, suffix: &str) -> bool {
    s.len() >= suffix.len()
        && s.as_bytes()[s.len() - suffix.len()..].eq_ignore_ascii_case(suffix.as_bytes())
}

#[cfg(test)]
mod tests;
