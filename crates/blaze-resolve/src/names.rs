//! Which package and feature names cargo accepts: `cargo-util-schemas`' `restricted_names.rs`
//! of cargo 1.99.0 (5f94df4789).

use anyhow::{Result, bail};

fn invalid(ch: char, what: &str, name: &str, reason: &str) -> anyhow::Error {
    anyhow::anyhow!("invalid character `{ch}` in {what}: `{name}`, {reason}")
}

/// A package name: each `::`-separated part a Unicode identifier that may also have `-`, and
/// does not start with a digit.
pub fn validate_package_name(name: &str) -> Result<()> {
    for part in name.split("::") {
        validate_name(part, "package name")?;
    }
    Ok(())
}

pub fn validate_registry_name(name: &str) -> Result<()> {
    validate_name(name, "registry name")
}

fn validate_name(name: &str, what: &str) -> Result<()> {
    if name.is_empty() {
        bail!("{what} cannot be empty");
    }
    let mut chars = name.chars();
    if let Some(ch) = chars.next() {
        if ch.is_ascii_digit() {
            return Err(invalid(ch, what, name, "the name cannot start with a digit"));
        }
        if !(unicode_ident::is_xid_start(ch) || ch == '_') {
            return Err(invalid(
                ch,
                what,
                name,
                "the first character must be a Unicode XID start character (most letters or `_`)",
            ));
        }
    }
    for ch in chars {
        if !(unicode_ident::is_xid_continue(ch) || ch == '-') {
            return Err(invalid(
                ch,
                what,
                name,
                "characters must be Unicode XID characters (numbers, `-`, `_`, or most letters)",
            ));
        }
    }
    Ok(())
}

/// A feature name: may start with a digit and have `-`, `+` and `.`, never `/` or `dep:`.
pub fn validate_feature_name(name: &str) -> Result<()> {
    let what = "feature name";
    if name.is_empty() {
        bail!("{what} cannot be empty");
    }
    if name.starts_with("dep:") {
        bail!("feature named `{name}` is not allowed to start with `dep:`");
    }
    if name.contains('/') {
        return Err(invalid('/', what, name, "feature name is not allowed to contain slashes"));
    }
    let mut chars = name.chars();
    if let Some(ch) = chars.next()
        && !(unicode_ident::is_xid_start(ch) || ch == '_' || ch.is_ascii_digit())
    {
        return Err(invalid(
            ch,
            what,
            name,
            "the first character must be a Unicode XID start character or digit \
             (most letters or `_` or `0` to `9`)",
        ));
    }
    for ch in chars {
        if !(unicode_ident::is_xid_continue(ch) || ch == '-' || ch == '+' || ch == '.') {
            return Err(invalid(
                ch,
                what,
                name,
                "characters must be Unicode XID characters, '-', `+`, or `.` \
                 (numbers, `+`, `-`, `_`, `.`, or most letters)",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_names() {
        for ok in ["serde", "serde_json", "tokio-util", "_x", "é", "a::b"] {
            validate_package_name(ok).unwrap();
        }
        for bad in ["", "1x", "-x", "a.b", "a b", "a::", "a+b"] {
            assert!(validate_package_name(bad).is_err(), "{bad}");
        }
        assert_eq!(
            validate_package_name("1x").unwrap_err().to_string(),
            "invalid character `1` in package name: `1x`, the name cannot start with a digit"
        );
    }

    #[test]
    fn feature_names() {
        for ok in ["default", "std", "1.0", "c++", "a-b.c+d", "_x"] {
            validate_feature_name(ok).unwrap();
        }
        for bad in ["", "dep:x", "a/b", "-x", "+x", ".x", "a b"] {
            assert!(validate_feature_name(bad).is_err(), "{bad}");
        }
    }
}
