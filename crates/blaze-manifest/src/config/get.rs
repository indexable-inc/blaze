//! `cargo config get` as cargo 1.99 prints it (`src/ops/cargo_config.rs`): the oracle the
//! harness reads the configuration through.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::path::Path;
use std::str::FromStr;

use serde_json::json;

use super::error::{ConfigError, ConfigWhat};
use super::key::{push_toml_key, push_toml_string};
use super::{Config, ConfigKey, ConfigValue as CV, Definition};
use crate::files::Disk;

#[derive(Debug, Clone, Copy)]
pub enum ConfigFormat {
    Toml,
    Json,
    JsonValue,
}

impl FromStr for ConfigFormat {
    type Err = ConfigError;

    fn from_str(s: &str) -> Result<Self, ConfigError> {
        match s {
            "toml" => Ok(ConfigFormat::Toml),
            "json" => Ok(ConfigFormat::Json),
            "json-value" => Ok(ConfigFormat::JsonValue),
            f => Err(ConfigError::new(ConfigWhat::UnknownFormat { format: f.to_owned() })),
        }
    }
}

impl std::fmt::Display for ConfigFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(format_name(*self))
    }
}

pub struct GetOptions<'a> {
    pub key: Option<&'a str>,
    pub format: ConfigFormat,
    pub show_origin: bool,
    pub merged: bool,
}

/// What `cargo config get` writes: to stdout, and the note on variables JSON sends to stderr.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
}

/// `cargo [--config X]... config get [OPTIONS] [KEY]` run in `cwd` with the variables `env`,
/// `args` being what follows `cargo`: its exit code, stdout and stderr.
pub fn main(
    cwd: &Path,
    env: impl IntoIterator<Item = (String, String)>,
    args: &[String],
) -> (u8, Output) {
    let mut config = match Config::load(cwd, env, &[]) {
        Ok(config) => config,
        Err(err) => {
            let stderr = format!("error: {}\n", err.report());
            return (101, Output { stdout: String::new(), stderr });
        }
    };
    let result = parse_args(args).and_then(|args| {
        config.start("config", &args.config)?;
        get(
            &config,
            &GetOptions {
                key: args.key.as_deref(),
                format: args.format,
                show_origin: args.show_origin,
                merged: args.merged,
            },
        )
    });
    let mut stderr = String::new();
    for warning in config.take_warnings() {
        let _ = writeln!(stderr, "warning: {warning}");
    }
    match result {
        Ok(output) => {
            stderr.push_str(&output.stderr);
            (0, Output { stdout: output.stdout, stderr })
        }
        Err(err) => {
            if let ConfigWhat::Usage { text } = err.what() {
                stderr.push_str(text);
                return (1, Output { stdout: String::new(), stderr });
            }
            let _ = writeln!(stderr, "error: {}", err.report());
            (101, Output { stdout: String::new(), stderr })
        }
    }
}

fn invalid_value(value: &str, arg: &str, possible: &str) -> ConfigError {
    ConfigError::new(ConfigWhat::Usage {
        text: format!(
            "error: invalid value '{value}' for '{arg}'\n  [possible values: {possible}]\n\n\
             For more information, try '--help'.\n"
        ),
    })
}

struct Args {
    config: Vec<String>,
    key: Option<String>,
    format: ConfigFormat,
    show_origin: bool,
    merged: bool,
}

fn parse_args(args: &[String]) -> Result<Args, ConfigError> {
    let mut parsed = Args {
        config: Vec::new(),
        key: None,
        format: ConfigFormat::Toml,
        show_origin: false,
        merged: true,
    };
    let mut args = args.iter().cloned();
    let mut subcommand = Vec::new();
    while let Some(arg) = args.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_owned(), Some(v.to_owned())),
            _ => (arg.clone(), None),
        };
        let mut value = || {
            inline
                .clone()
                .or_else(|| args.next())
                .ok_or_else(|| {
                    ConfigError::new(ConfigWhat::ValueRequired { name: name.clone() })
                })
        };
        match name.as_str() {
            "-Zunstable-options" => {}
            "--config" => parsed.config.push(value()?),
            "--format" => {
                let format = value()?;
                parsed.format = format.parse().map_err(|_| {
                    invalid_value(&format, "--format <format>", "toml, json, json-value")
                })?;
            }
            "--show-origin" => parsed.show_origin = true,
            "--merged" => {
                let merged = value()?;
                parsed.merged = match merged.as_str() {
                    "yes" => true,
                    "no" => false,
                    _ => return Err(invalid_value(&merged, "--merged <merged>", "yes, no")),
                };
            }
            _ if !arg.starts_with('-') => subcommand.push(arg),
            _ => {
                return Err(ConfigError::new(ConfigWhat::UnexpectedArgument { arg: arg.clone() }));
            }
        }
    }
    match subcommand.as_slice() {
        [config, get] if config == "config" && get == "get" => {}
        [config, get, key] if config == "config" && get == "get" => parsed.key = Some(key.clone()),
        _ => {
            let found = subcommand.join(" ");
            return Err(ConfigError::new(ConfigWhat::ExpectedConfigGet { found }));
        }
    }
    Ok(parsed)
}

/// The name a format is spelled with on the command line.
fn format_name(format: ConfigFormat) -> &'static str {
    match format {
        ConfigFormat::Toml => "toml",
        ConfigFormat::Json => "json",
        ConfigFormat::JsonValue => "json-value",
    }
}

/// `cargo config get`. The command is unstable, so the cargo that runs it reads its `unstable`
/// table on start, which loads the files: an error in them is an error here even unmerged.
pub fn get(config: &Config<Disk>, opts: &GetOptions<'_>) -> Result<Output, ConfigError> {
    config.ensure_loaded()?;
    if opts.show_origin && !matches!(opts.format, ConfigFormat::Toml) {
        let format = format_name(opts.format);
        return Err(ConfigError::new(ConfigWhat::NoShowOrigin { format }));
    }
    let key = match opts.key {
        Some(key) => ConfigKey::parse(key),
        None => ConfigKey::new(),
    };
    let mut out = Output::default();
    if opts.merged {
        let cv = config.get_cv_with_env(&key)?.ok_or_else(|| {
            ConfigError::new(ConfigWhat::NotSet { key: key.to_string() })
        })?;
        match opts.format {
            ConfigFormat::Toml => print_toml(&mut out.stdout, opts, &key, &cv),
            ConfigFormat::Json => print_json(&mut out.stdout, &key, &cv, true),
            ConfigFormat::JsonValue => print_json(&mut out.stdout, &key, &cv, false),
        }
        if let Some(env) = maybe_env(config, &key, &cv) {
            match opts.format {
                ConfigFormat::Toml => {
                    out.stdout.push_str(
                        "# The following environment variables may affect the loaded values.\n",
                    );
                    for (k, v) in env {
                        let _ = writeln!(
                            out.stdout,
                            "# {k}={}",
                            shell_escape::escape(Cow::Borrowed(v))
                        );
                    }
                }
                ConfigFormat::Json | ConfigFormat::JsonValue => {
                    out.stderr.push_str(
                        "note: The following environment variables may affect the loaded values.\n",
                    );
                    for (k, v) in env {
                        let _ =
                            writeln!(out.stderr, "{k}={}", shell_escape::escape(Cow::Borrowed(v)));
                    }
                }
            }
        }
    } else {
        match opts.format {
            ConfigFormat::Toml => print_toml_unmerged(config, &mut out.stdout, opts, &key)?,
            format => {
                let format = format_name(format);
                return Err(ConfigError::new(ConfigWhat::NoMergedNo { format }));
            }
        }
    }
    Ok(out)
}

fn maybe_env<'a>(
    config: &'a Config<Disk>,
    key: &ConfigKey,
    cv: &CV,
) -> Option<Vec<(&'a str, &'a str)>> {
    if !matches!(cv, CV::Table(..)) {
        return None;
    }
    let prefix = format!("{}_", key.as_env_key());
    let env: Vec<_> = config.env_vars().filter(|(k, _)| k.starts_with(&prefix)).collect();
    (!env.is_empty()).then_some(env)
}

/// `cv` as an inline TOML value, as `toml_edit` writes one by default: `[a, b]`, `{ k = v }`,
/// `{}`, a table's keys in order.
fn write_toml(cv: &CV, out: &mut String) {
    match cv {
        CV::String(s, _) => push_toml_string(s, out),
        CV::Integer(i, _) => {
            let _ = write!(out, "{i}");
        }
        CV::Boolean(b, _) => {
            let _ = write!(out, "{b}");
        }
        CV::List(l, _) => {
            out.push('[');
            for (i, cv) in l.iter().enumerate() {
                if i != 0 {
                    out.push_str(", ");
                }
                write_toml(cv, out);
            }
            out.push(']');
        }
        CV::Table(t, _) => {
            if t.is_empty() {
                out.push_str("{}");
                return;
            }
            let mut t: Vec<_> = t.iter().collect();
            t.sort_by_key(|t| t.0);
            out.push_str("{ ");
            for (i, (k, v)) in t.into_iter().enumerate() {
                if i != 0 {
                    out.push_str(", ");
                }
                push_toml_key(k, out);
                out.push_str(" = ");
                write_toml(v, out);
            }
            out.push_str(" }");
        }
    }
}

fn print_toml(out: &mut String, opts: &GetOptions<'_>, key: &ConfigKey, cv: &CV) {
    let origin = |def: &Definition| {
        if opts.show_origin { format!(" # {def}") } else { String::new() }
    };
    match cv {
        CV::Boolean(val, def) => {
            let _ = writeln!(out, "{key} = {val}{}", origin(def));
        }
        CV::Integer(val, def) => {
            let _ = writeln!(out, "{key} = {val}{}", origin(def));
        }
        CV::String(_, def) => {
            let _ = write!(out, "{key} = ");
            write_toml(cv, out);
            let _ = writeln!(out, "{}", origin(def));
        }
        CV::List(vals, _) => {
            if opts.show_origin {
                let _ = writeln!(out, "{key} = [");
                for cv in vals {
                    out.push_str("    ");
                    write_toml(cv, out);
                    let _ = writeln!(out, ", # {}", cv.definition());
                }
                out.push_str("]\n");
            } else {
                let _ = write!(out, "{key} = ");
                write_toml(cv, out);
                out.push('\n');
            }
        }
        CV::Table(table, _) => {
            let mut key_vals: Vec<_> = table.iter().collect();
            key_vals.sort_by(|a, b| a.0.cmp(b.0));
            for (table_key, val) in key_vals {
                let mut subkey = key.clone();
                subkey.push(table_key);
                print_toml(out, opts, &subkey, val);
            }
        }
    }
}

fn print_json(out: &mut String, key: &ConfigKey, cv: &CV, include_key: bool) {
    fn cv_to_json(cv: &CV) -> serde_json::Value {
        match cv {
            CV::Boolean(val, _) => json!(val),
            CV::Integer(val, _) => json!(val),
            CV::String(val, _) => json!(val),
            CV::List(vals, _) => json!(vals.iter().map(cv_to_json).collect::<Vec<_>>()),
            CV::Table(map, _) => {
                let mut map: Vec<_> = map.iter().collect();
                map.sort_by_key(|(k, _)| *k);
                let mut table = json!({});
                for (key, val) in map {
                    table[key] = cv_to_json(val);
                }
                table
            }
        }
    }

    let json_value = if key.is_root() || !include_key {
        cv_to_json(cv)
    } else {
        let mut parts: Vec<_> = key.parts().collect();
        let last_part = parts.pop().unwrap();
        let mut root_table = json!({});
        let mut table = &mut root_table;
        for part in parts {
            table[part] = json!({});
            table = table.get_mut(part).unwrap();
        }
        table[last_part] = cv_to_json(cv);
        root_table
    };
    let _ = writeln!(out, "{}", serde_json::to_string(&json_value).unwrap());
}

fn print_toml_unmerged(
    config: &Config<Disk>,
    out: &mut String,
    opts: &GetOptions<'_>,
    key: &ConfigKey,
) -> Result<(), ConfigError> {
    let print_table = |out: &mut String, cv: &CV| {
        let _ = writeln!(out, "# {}", cv.definition());
        print_toml(out, opts, &ConfigKey::new(), cv);
        out.push('\n');
    };

    fn trim_cv(mut cv: &mut CV, key: &ConfigKey) -> Result<bool, ConfigError> {
        for (i, part) in key.parts().enumerate() {
            match cv {
                CV::Table(map, _) => {
                    map.retain(|key, _| key == part);
                    match map.get_mut(part) {
                        Some(val) => cv = val,
                        None => return Ok(false),
                    }
                }
                _ => {
                    let mut key_so_far = ConfigKey::new();
                    for part in key.parts().take(i) {
                        key_so_far.push(part);
                    }
                    return Err(ConfigError::new(ConfigWhat::NotATable {
                        key: key_so_far.to_string(),
                        found: cv.desc(),
                        definition: cv.definition().clone(),
                    }));
                }
            }
        }
        Ok(match cv {
            CV::Table(map, _) => !map.is_empty(),
            _ => true,
        })
    }

    let mut cli_args = config.cli_args_as_table()?;
    if trim_cv(&mut cli_args, key)? {
        print_table(out, &cli_args);
    }

    let env: Vec<_> = config.env_vars().filter(|(k, _)| k.starts_with(key.as_env_key())).collect();
    if !env.is_empty() {
        out.push_str("# Environment variables\n");
        for (k, v) in env {
            let _ = writeln!(out, "# {k}={}", shell_escape::escape(Cow::Borrowed(v)));
        }
        out.push('\n');
    }

    for mut cv in config.load_values_unmerged()? {
        if trim_cv(&mut cv, key)? {
            print_table(out, &cv);
        }
    }
    Ok(())
}
