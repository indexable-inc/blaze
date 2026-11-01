//! The environment as it was built before a sorted vector: a hash map of what cargo sets,
//! applied to a `BTreeMap` of what it inherits, a `format!` per variable. Kept as the
//! reference [`crate::env::env`] is checked against over seeded units.

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

use blaze_hash::{HashMap, HashSet};

use crate::env::{HostOs, Version, below, cfg_of};
use crate::{Error, unit::Unit};

#[derive(Default)]
struct Explicit {
    vars: HashMap<OsString, Option<OsString>>,
}

impl Explicit {
    fn set(&mut self, name: impl Into<OsString>, value: impl Into<OsString>) {
        self.vars.insert(name.into(), Some(value.into()));
    }

    fn remove(&mut self, name: &str) {
        self.vars.insert(name.into(), None);
    }

    fn contains(&self, name: &str) -> bool {
        self.vars.contains_key(OsStr::new(name))
    }

    fn get(&self, name: &str) -> Option<&OsStr> {
        self.vars.get(OsStr::new(name)).and_then(|it| it.as_deref())
    }
}

fn envify(name: &str) -> String {
    name.chars().flat_map(char::to_uppercase).map(|c| if c == '-' { '_' } else { c }).collect()
}

/// The whole environment, and the names set from the unit.
pub fn env(
    unit: &Unit,
    inherited: &[(OsString, OsString)],
) -> Result<(BTreeMap<OsString, OsString>, HashSet<OsString>), Error> {
    unit.check()?;
    let host = HostOs::of(&unit.host);
    let inherited_var = |name: &str| {
        inherited.iter().find(|(it, _)| it == OsStr::new(name)).map(|(_, value)| value.as_os_str())
    };
    let mut cmd = Explicit::default();
    let package = &unit.package;

    let search = &unit.dylib_search;
    let mut search_path = Vec::new();
    if let Some(host_root) = &search.host_root {
        search_path.extend(below(&[], host_root));
        search_path.push(host_root.clone());
    }
    search_path.extend(search.host_deps.iter().cloned());
    if !search.sysroot_libdir.as_os_str().is_empty() {
        search_path.push(search.sysroot_libdir.clone());
    }
    let dylib_path =
        inherited_var(host.dylib_var).map(|it| host.split_paths(it)).unwrap_or_default();
    let dylib_path_is_empty = dylib_path.is_empty();
    if dylib_path.starts_with(&search_path) {
        search_path = dylib_path;
    } else {
        search_path.extend(dylib_path);
    }
    if host.macos && dylib_path_is_empty {
        if let Some(home) = inherited_var("HOME") {
            search_path.push(Path::new(home).join("lib"));
        }
        search_path.push(PathBuf::from("/usr/local/lib"));
        search_path.push(PathBuf::from("/usr/lib"));
    }
    cmd.set(host.dylib_var, host.join_paths(&search_path, host.dylib_var)?);
    cmd.set("CARGO", &unit.tools.cargo);
    let version = Version::of(package)?;
    cmd.set("CARGO_MANIFEST_DIR", &package.manifest_dir);
    cmd.set("CARGO_MANIFEST_PATH", &package.manifest_path);
    cmd.set("CARGO_PKG_VERSION_MAJOR", version.major);
    cmd.set("CARGO_PKG_VERSION_MINOR", version.minor);
    cmd.set("CARGO_PKG_VERSION_PATCH", version.patch);
    cmd.set("CARGO_PKG_VERSION_PRE", version.pre);
    cmd.set("CARGO_PKG_VERSION", &package.version);
    cmd.set("CARGO_PKG_NAME", &package.name);
    let field = |it: &Option<String>| it.clone().unwrap_or_default();
    cmd.set("CARGO_PKG_DESCRIPTION", field(&package.description));
    cmd.set("CARGO_PKG_HOMEPAGE", field(&package.homepage));
    cmd.set("CARGO_PKG_REPOSITORY", field(&package.repository));
    cmd.set("CARGO_PKG_LICENSE", field(&package.license));
    cmd.set("CARGO_PKG_LICENSE_FILE", field(&package.license_file));
    cmd.set("CARGO_PKG_AUTHORS", package.authors.join(":"));
    cmd.set("CARGO_PKG_RUST_VERSION", field(&package.rust_version));
    cmd.set("CARGO_PKG_README", field(&package.readme));
    for it in &unit.config_env {
        if (it.force || inherited_var(&it.name).is_none()) && !cmd.contains(&it.name) {
            cmd.set(&it.name, &it.value);
        }
    }

    cmd.set("OUT_DIR", &unit.out_dir);
    cmd.set("CARGO_MANIFEST_DIR", &package.manifest_dir);
    cmd.set("CARGO_MANIFEST_PATH", &package.manifest_path);
    cmd.set("NUM_JOBS", unit.jobs.to_string());
    cmd.set("TARGET", &unit.target);
    cmd.set("DEBUG", unit.profile.debuginfo.to_string());
    cmd.set("OPT_LEVEL", &unit.profile.opt_level);
    cmd.set("PROFILE", if unit.profile.release { "release" } else { "debug" });
    cmd.set("HOST", &unit.host);
    cmd.set("RUSTC", &unit.tools.rustc);
    cmd.set("RUSTDOC", &unit.tools.rustdoc);
    if let Some(linker) = &unit.tools.linker {
        cmd.set("RUSTC_LINKER", linker);
    }
    if let Some(links) = &package.links {
        cmd.set("CARGO_MANIFEST_LINKS", links);
    }
    for feature in &unit.features {
        cmd.set(format!("CARGO_FEATURE_{}", envify(feature)), "1");
    }
    let mut cfgs: Vec<(&str, Vec<&str>)> = Vec::new();
    fn insert<'a>(cfgs: &mut Vec<(&'a str, Vec<&'a str>)>, name: &'a str, values: Vec<&'a str>) {
        match cfgs.iter_mut().find(|(it, _)| *it == name) {
            Some((_, it)) => *it = values,
            None => cfgs.push((name, values)),
        }
    }
    insert(&mut cfgs, "feature", unit.features.iter().map(String::as_str).collect());
    if unit.profile.debug_assertions {
        insert(&mut cfgs, "debug_assertions", Vec::new());
    }
    for line in &unit.target_cfgs {
        match cfg_of(package, line)? {
            ("debug_assertions", None) => {}
            (name, None) => insert(&mut cfgs, name, Vec::new()),
            (name, Some(value)) => match cfgs.iter_mut().find(|(it, _)| *it == name) {
                Some((_, values)) => values.push(value),
                None => cfgs.push((name, vec![value])),
            },
        }
    }
    for (name, values) in cfgs {
        cmd.set(format!("CARGO_CFG_{}", envify(name)), values.join(","));
    }
    match &unit.tools.rustc_wrapper {
        Some(wrapper) => cmd.set("RUSTC_WRAPPER", wrapper),
        None => cmd.remove("RUSTC_WRAPPER"),
    }
    cmd.remove("RUSTC_WORKSPACE_WRAPPER");
    if unit.workspace_member
        && let Some(wrapper) = &unit.tools.rustc_workspace_wrapper
    {
        cmd.set("RUSTC_WORKSPACE_WRAPPER", wrapper);
    }
    cmd.set("CARGO_ENCODED_RUSTFLAGS", unit.rustflags.join("\x1f"));
    cmd.remove("RUSTFLAGS");
    for dependency in &unit.dependencies {
        let Some(links) = &dependency.links else { continue };
        for (key, value) in &dependency.metadata {
            cmd.set(format!("DEP_{}_{}", envify(links), envify(key)), value);
        }
    }
    if let Some(host_root) = &search.host_root {
        let mut search_path =
            cmd.get(host.dylib_var).map(|it| host.split_paths(it)).unwrap_or_default();
        search_path.extend(below(&search.plugin_search, host_root));
        cmd.set(host.dylib_var, host.join_paths(&search_path, host.dylib_var)?);
    }
    if let Some(auth) = &unit.jobserver {
        cmd.set("CARGO_MAKEFLAGS", format!("-j --jobserver-fds={auth} --jobserver-auth={auth}"));
    }

    let mut vars: BTreeMap<OsString, OsString> = inherited.iter().cloned().collect();
    let mut from_unit = HashSet::default();
    for (name, value) in cmd.vars {
        match value {
            Some(value) => {
                from_unit.insert(name.clone());
                vars.insert(name, value);
            }
            None => _ = vars.remove(&name),
        }
    }
    Ok((vars, from_unit))
}
