//! `cargo config get` as cargo 1.99 prints it (`src/ops/cargo_config.rs`): the oracle the
//! harness reads the configuration through.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::path::Path;
use std::str::FromStr;

use anyhow::{anyhow, bail};
use serde_json::json;

use super::{Config, ConfigKey, ConfigValue as CV, Definition};
use crate::files::Disk;
use crate::diagnostic::report;

#[derive(Debug, Clone, Copy)]
pub enum ConfigFormat {
    Toml,
    Json,
    JsonValue,
}

impl FromStr for ConfigFormat {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s {
            "toml" => Ok(ConfigFormat::Toml),
            "json" => Ok(ConfigFormat::Json),
            "json-value" => Ok(ConfigFormat::JsonValue),
            f => bail!("unknown config format `{f}`"),
        }
    }
}

impl std::fmt::Display for ConfigFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ConfigFormat::Toml => "toml",
            ConfigFormat::Json => "json",
            ConfigFormat::JsonValue => "json-value",
        })
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
            return (101, Output { stdout: String::new(), stderr: report(&err, cwd) + "\n" });
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
            if let Some(usage) = err.downcast_ref::<UsageError>() {
                stderr.push_str(&usage.0);
                return (1, Output { stdout: String::new(), stderr });
            }
            let _ = writeln!(stderr, "{}", report(&err, cwd));
            (101, Output { stdout: String::new(), stderr })
        }
    }
}

/// A command line cargo's argument parser refuses: printed as it is, exit code 1.
#[derive(Debug)]
struct UsageError(String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

fn invalid_value(value: &str, arg: &str, possible: &str) -> anyhow::Error {
    UsageError(format!(
        "error: invalid value '{value}' for '{arg}'\n  [possible values: {possible}]\n\n\
         For more information, try '--help'.\n"
    ))
    .into()
}

struct Args {
    config: Vec<String>,
    key: Option<String>,
    format: ConfigFormat,
    show_origin: bool,
    merged: bool,
}

fn parse_args(args: &[String]) -> anyhow::Result<Args> {
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
                .ok_or_else(|| anyhow!("a value is required for '{name} <VALUE>'"))
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
            _ => bail!("unexpected argument '{arg}' found"),
        }
    }
    match subcommand.as_slice() {
        [config, get] if config == "config" && get == "get" => {}
        [config, get, key] if config == "config" && get == "get" => parsed.key = Some(key.clone()),
        _ => bail!("expected `config get [KEY]`, found `{}`", subcommand.join(" ")),
    }
    Ok(parsed)
}

/// `cargo config get`. The command is unstable, so the cargo that runs it reads its `unstable`
/// table on start, which loads the files: an error in them is an error here even unmerged.
pub fn get(config: &Config<Disk>, opts: &GetOptions<'_>) -> anyhow::Result<Output> {
    config.ensure_loaded()?;
    if opts.show_origin && !matches!(opts.format, ConfigFormat::Toml) {
        bail!(
            "the `{}` format does not support --show-origin, try the `toml` format instead",
            opts.format
        );
    }
    let key = match opts.key {
        Some(key) => ConfigKey::parse(key),
        None => ConfigKey::new(),
    };
    let mut out = Output::default();
    if opts.merged {
        let cv = config
            .get_cv_with_env(&key)?
            .ok_or_else(|| anyhow!("config value `{key}` is not set"))?;
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
            format => bail!(
                "the `{format}` format does not support --merged=no, try the `toml` format instead"
            ),
        }
    }
    Ok(out)
}

fn maybe_env<'a>(config: &'a Config<Disk>, key: &ConfigKey, cv: &CV) -> Option<Vec<(&'a str, &'a str)>> {
    if !matches!(cv, CV::Table(..)) {
        return None;
    }
    let prefix = format!("{}_", key.as_env_key());
    let env: Vec<_> = config.env_vars().filter(|(k, _)| k.starts_with(&prefix)).collect();
    (!env.is_empty()).then_some(env)
}

fn cv_to_toml(cv: &CV) -> toml_edit::Value {
    match cv {
        CV::String(s, _) => toml_edit::Value::from(s.as_str()),
        CV::Integer(i, _) => toml_edit::Value::from(*i),
        CV::Boolean(b, _) => toml_edit::Value::from(*b),
        CV::List(l, _) => toml_edit::Value::from_iter(l.iter().map(cv_to_toml)),
        CV::Table(t, _) => toml_edit::Value::from_iter({
            let mut t: Vec<_> = t.iter().collect();
            t.sort_by_key(|t| t.0);
            t.into_iter().map(|(k, v)| (k, cv_to_toml(v)))
        }),
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
        CV::String(val, def) => {
            let _ = writeln!(out, "{key} = {}{}", toml_edit::Value::from(val), origin(def));
        }
        CV::List(vals, _) => {
            if opts.show_origin {
                let _ = writeln!(out, "{key} = [");
                for cv in vals {
                    let _ = writeln!(out, "    {}, # {}", cv_to_toml(cv), cv.definition());
                }
                out.push_str("]\n");
            } else {
                let vals: toml_edit::Array = vals.iter().map(cv_to_toml).collect();
                let _ = writeln!(out, "{key} = {vals}");
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
) -> anyhow::Result<()> {
    let print_table = |out: &mut String, cv: &CV| {
        let _ = writeln!(out, "# {}", cv.definition());
        print_toml(out, opts, &ConfigKey::new(), cv);
        out.push('\n');
    };

    fn trim_cv(mut cv: &mut CV, key: &ConfigKey) -> anyhow::Result<bool> {
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
                    bail!(
                        "expected table for configuration key `{key_so_far}`, but found {} in {}",
                        cv.desc(),
                        cv.definition()
                    )
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
