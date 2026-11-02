//! The files cargo writes for a unit: `compilation_files.rs` (`output_dir`, `pkg_dir`,
//! `calc_outputs_rustc`, `uplift_to`, `build_script_dir`, `build_script_run_dir`), `layout.rs`
//! (both of the pinned cargo's layouts of the build directory: the new one, its default, and the
//! one `__CARGO_TEMPORARY_BUILD_DIR_NEW_LAYOUT_OPT_OUT=1` keeps), `target_info.rs`
//! (`file_types`, `rustc_outputs`), `output_depinfo.rs`, and rustc's dep-info beside its output.

use std::path::{Path, PathBuf};

use blaze_manifest::targets::{CrateType, TargetKind};

use super::hash::CompileMode;
use crate::query::LoadError;
use crate::stdlib::target as platform;

fn error(what: String) -> LoadError {
    LoadError { what, at: None }
}

/// cargo's `Layout` of one kind: its artifact directory (`target/<triple>/debug`), and its
/// build directory's root (`build-dir/<triple>`) and profile directory (`.../debug`).
pub(super) struct Layout {
    pub dest: PathBuf,
    pub build_root: PathBuf,
    pub build_dest: PathBuf,
}

impl Layout {
    pub fn new(target_dir: &Path, build_dir: &Path, triple: Option<&str>, profile_dir: &str) -> Layout {
        let (root, build_root) = match triple {
            Some(triple) => (target_dir.join(triple), build_dir.join(triple)),
            None => (target_dir.to_path_buf(), build_dir.to_path_buf()),
        };
        Layout { dest: root.join(profile_dir), build_dest: build_root.join(profile_dir), build_root }
    }

    fn build(&self) -> PathBuf {
        self.build_dest.join("build")
    }

    /// `BuildDirLayout::deps`.
    pub fn deps(&self, new_layout: bool, pkg_dir: &str) -> PathBuf {
        if new_layout { self.out_new_layout(pkg_dir) } else { self.build_dest.join("deps") }
    }

    /// `BuildDirLayout::out_force_new_layout`.
    pub fn out_new_layout(&self, pkg_dir: &str) -> PathBuf {
        self.build().join(pkg_dir).join("out")
    }

    /// `BuildDirLayout::build_script`.
    pub fn build_script(&self, new_layout: bool, pkg_dir: &str) -> PathBuf {
        if new_layout { self.deps(true, pkg_dir) } else { self.build().join(pkg_dir) }
    }

    /// `BuildDirLayout::build_script_execution`.
    pub fn build_script_execution(&self, new_layout: bool, pkg_dir: &str) -> PathBuf {
        if new_layout { self.build().join(pkg_dir).join("run") } else { self.build().join(pkg_dir) }
    }

    pub fn tmp(&self) -> PathBuf {
        self.build_root.join("tmp")
    }
}

/// cargo's `FileFlavor`, of the flavors a compile has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Flavor {
    Normal,
    Auxiliary,
    Linkable,
    Rmeta,
    DebugInfo,
}

/// cargo's `FileType`.
#[derive(Clone, Debug)]
pub(super) struct FileType {
    pub flavor: Flavor,
    pub crate_type: Option<CrateType>,
    prefix: String,
    suffix: String,
    replace_hyphens: bool,
}

impl FileType {
    fn rmeta() -> FileType {
        FileType { flavor: Flavor::Rmeta, crate_type: None, prefix: "lib".to_owned(), suffix: ".rmeta".to_owned(), replace_hyphens: true }
    }

    fn output_filename(&self, crate_name: &str, extra: Option<&str>) -> String {
        match extra {
            Some(extra) => format!("{}{crate_name}-{extra}{}", self.prefix, self.suffix),
            None => format!("{}{crate_name}{}", self.prefix, self.suffix),
        }
    }

    pub fn uplift_filename(&self, name: &str, crate_name: &str, bin_name: Option<&str>) -> String {
        let name = match bin_name {
            Some(it) => it,
            None if self.replace_hyphens => crate_name,
            None => name,
        };
        format!("{}{name}{}", self.prefix, self.suffix)
    }
}

const MARK: &str = "\u{1}";

/// rustc's file name of a crate type for `triple` (`--print file-names`), as a prefix and a
/// suffix; `None` for a crate type the target lacks.
fn prefix_suffix(triple: &str, crate_type: &CrateType) -> Result<Option<(String, String)>, LoadError> {
    let name = platform::file_name(triple, crate_type.as_str(), MARK).map_err(error)?;
    Ok(match name {
        None => None,
        Some(name) => {
            let (prefix, suffix) = name
                .split_once(MARK)
                .ok_or_else(|| error(format!("rustc's `{}` file for `{triple}` has no crate name in it", crate_type.as_str())))?;
            Some((prefix.to_owned(), suffix.to_owned()))
        }
    })
}

/// cargo's `TargetInfo::file_types`.
fn file_types(crate_type: &CrateType, flavor: Flavor, triple: &str) -> Result<Option<Vec<FileType>>, LoadError> {
    let crate_type = if *crate_type == CrateType::Lib { CrateType::Rlib } else { crate_type.clone() };
    let Some((prefix, suffix)) = prefix_suffix(triple, &crate_type)? else { return Ok(None) };
    let file = |suffix: &str, prefix: &str, flavor: Flavor, replace_hyphens: bool| FileType {
        flavor,
        crate_type: Some(crate_type.clone()),
        prefix: prefix.to_owned(),
        suffix: suffix.to_owned(),
        replace_hyphens,
    };
    let mut out = vec![file(&suffix, &prefix, flavor, crate_type != CrateType::Bin)];
    if crate_type.is_dynamic() {
        if triple.ends_with("-windows-msvc") && suffix == ".dll" {
            out.push(file(".dll.lib", &prefix, Flavor::Auxiliary, true));
            out.push(file(".dll.exp", &prefix, Flavor::Auxiliary, true));
        } else if suffix == ".dll"
            && (triple.ends_with("windows-gnu") || triple.ends_with("windows-gnullvm") || triple.ends_with("cygwin"))
        {
            out.push(file(".dll.a", "lib", Flavor::Auxiliary, true));
        }
    }
    if triple.starts_with("wasm32-") && crate_type == CrateType::Bin && suffix == ".js" {
        out.push(file(".wasm", &prefix, Flavor::Auxiliary, true));
        out.push(file(".wasm.map", &prefix, Flavor::DebugInfo, true));
    }
    if matches!(crate_type, CrateType::Bin | CrateType::Dylib | CrateType::Cdylib | CrateType::ProcMacro) {
        if triple.contains("-apple-") {
            let suffix = if crate_type == CrateType::Bin { ".dSYM" } else { ".dylib.dSYM" };
            out.push(file(suffix, &prefix, Flavor::DebugInfo, false));
        } else if triple.ends_with("-msvc") || triple.ends_with("-uefi") {
            out.push(file(".pdb", &prefix, Flavor::DebugInfo, true));
        } else {
            out.push(file(&format!("{suffix}.dwp"), &prefix, Flavor::DebugInfo, crate_type != CrateType::Bin));
        }
    }
    Ok(Some(out))
}

/// cargo's `TargetInfo::rustc_outputs`, with `calc_outputs_rustc`'s errors when the target
/// has none of the crate types. `embed-metadata` is always on (`-Zno-embed-metadata` unstable).
pub(super) fn rustc_outputs(mode: CompileMode, kind: &TargetKind, triple: &str, package: &str) -> Result<Vec<FileType>, LoadError> {
    let (types, unsupported) = match mode {
        CompileMode::Build => {
            let crate_types = kind.rustc_crate_types();
            let mut unsupported = Vec::new();
            let mut types = Vec::new();
            for crate_type in crate_types {
                let flavor = if crate_type.is_linkable() { Flavor::Linkable } else { Flavor::Normal };
                match file_types(crate_type, flavor, triple)? {
                    Some(it) => types.extend(it),
                    None => unsupported.push(crate_type.clone()),
                }
            }
            if !types.is_empty() && !crate_types.iter().any(CrateType::requires_upstream_objects) {
                types.push(FileType::rmeta());
            }
            (types, unsupported)
        }
        CompileMode::Test => match file_types(&CrateType::Bin, Flavor::Normal, triple)? {
            Some(it) => (it, Vec::new()),
            None => (Vec::new(), vec![CrateType::Bin]),
        },
        CompileMode::Check { .. } => (vec![FileType::rmeta()], Vec::new()),
        other => return Err(error(format!("cargo asks rustc for no output of a `{other:?}` unit"))),
    };
    if types.is_empty() {
        if !unsupported.is_empty() {
            let names: Vec<&str> = unsupported.iter().map(CrateType::as_str).collect();
            return Err(error(format!(
                "cannot produce {} for `{package}` as the target `{triple}` does not support these crate types",
                names.join(", ")
            )));
        }
        return Err(error(format!(
            "cannot compile `{package}` as the target `{triple}` does not support any of the output crate types"
        )));
    }
    Ok(types)
}

/// What a compile unit's files are decided by.
pub(super) struct CompileFiles<'a> {
    pub mode: CompileMode,
    pub kind: &'a TargetKind,
    pub name: &'a str,
    pub crate_name: &'a str,
    pub bin_name: Option<&'a str>,
    pub package: &'a str,
    pub triple: &'a str,
    /// `-C extra-filename`'s hex, when there is one.
    pub extra: Option<&'a str>,
    pub pkg_dir: &'a str,
    pub layout: &'a Layout,
    pub host_layout: &'a Layout,
    pub new_layout: bool,
    /// Whether the artifact directory is written (not for `cargo check`).
    pub artifact_dir: bool,
    /// cargo's nightly stop of uplifting rlibs, and its opt-out.
    pub stop_rlib_uplift: bool,
    pub is_root: bool,
}

/// cargo's `output_dir` of a compile unit.
pub(super) fn output_dir(it: &CompileFiles<'_>) -> PathBuf {
    if it.kind.is_custom_build() {
        it.host_layout.build_script(it.new_layout, it.pkg_dir)
    } else if it.kind.is_example() && !it.new_layout {
        it.layout.build_dest.join("examples")
    } else {
        it.layout.deps(it.new_layout, it.pkg_dir)
    }
}

/// Every file of a compile unit: each of `calc_outputs_rustc`'s with its uplifted copy, rustc's
/// dep-info, and of a root unit `output_depinfo`'s `.d` beside each copy.
pub(super) fn compile_outputs(it: &CompileFiles<'_>) -> Result<Vec<String>, LoadError> {
    let out_dir = output_dir(it);
    let types = rustc_outputs(it.mode, it.kind, it.triple, it.package)?;
    let mut outputs = Vec::with_capacity(types.len() * 2 + 2);
    let mut depinfo = Vec::new();
    for file in &types {
        let path = out_dir.join(file.output_filename(it.crate_name, it.extra));
        let hardlink = uplift_to(it, file, &path);
        outputs.push(path.display().to_string());
        if let Some(link) = hardlink {
            if it.is_root && !matches!(file.flavor, Flavor::DebugInfo | Flavor::Auxiliary) {
                depinfo.push(link.with_extension("d").display().to_string());
            }
            outputs.push(link.display().to_string());
        }
    }
    let stem = match it.extra {
        Some(extra) => format!("{}-{extra}.d", it.crate_name),
        None => format!("{}.d", it.crate_name),
    };
    outputs.push(out_dir.join(stem).display().to_string());
    outputs.extend(depinfo);
    Ok(outputs)
}

/// cargo's `uplift_to`.
fn uplift_to(it: &CompileFiles<'_>, file: &FileType, from: &Path) -> Option<PathBuf> {
    if it.mode != CompileMode::Build || file.flavor == Flavor::Rmeta {
        return None;
    }
    if it.new_layout && it.kind.is_custom_build() {
        return None;
    }
    if file.crate_type == Some(CrateType::Rlib) && it.stop_rlib_uplift {
        return None;
    }
    if !it.kind.is_bin() && !it.kind.is_custom_build() && file.crate_type != Some(CrateType::Dylib) && !it.is_root {
        return None;
    }
    let filename = file.uplift_filename(it.name, it.crate_name, it.bin_name);
    let path = if it.kind.is_example() {
        if !it.artifact_dir {
            return None;
        }
        it.layout.dest.join("examples").join(filename)
    } else if it.kind.is_custom_build() {
        it.host_layout.build_script(it.new_layout, it.pkg_dir).join(filename)
    } else {
        if !it.artifact_dir {
            return None;
        }
        it.layout.dest.join(filename)
    };
    (path != from).then_some(path)
}

/// A build script's run: its `OUT_DIR`, and the files of `BuildScriptRunFiles`.
pub(super) fn run_files(layout: &Layout, new_layout: bool, pkg_dir: &str) -> (String, Vec<String>) {
    let root = layout.build_script_execution(new_layout, pkg_dir);
    let out_dir = if new_layout { layout.out_new_layout(pkg_dir) } else { root.join("out") };
    let stdout = if new_layout { "stdout" } else { "output" };
    let files = [stdout, "stderr", "root-output"].map(|it| root.join(it).display().to_string());
    (out_dir.display().to_string(), files.into())
}

/// `bin_link_for_target`: where a bin of the package is uplifted for a unit of `triple`
/// (`CARGO_BIN_EXE_*`), when the artifact directory is written.
pub(super) fn bin_link(layout: &Layout, triple: &str, name: &str, bin_name: Option<&str>) -> Result<PathBuf, LoadError> {
    let types = rustc_outputs(CompileMode::Build, &TargetKind::Bin, triple, name)?;
    let file = types
        .iter()
        .find(|it| it.flavor == Flavor::Normal)
        .ok_or_else(|| error(format!("the target `{triple}` has no executable")))?;
    let crate_name = name.replace('-', "_");
    Ok(layout.dest.join(file.uplift_filename(name, &crate_name, bin_name)))
}
