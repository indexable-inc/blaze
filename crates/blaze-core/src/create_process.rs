//! The command line and the environment block `CreateProcessW` is given, made as std's
//! `Command` makes them. The program is quoted, and each argument quoted when it is empty or
//! has a space or a tab, its backslashes doubled before a quote and the quote escaped, so the
//! C runtime splits the line back into the same arguments. The variables are sorted by their
//! names in upper case, as Windows wants the block, and the last given of a name is kept.
//!
//! The work is on UTF-16 code units, so the rules are tested on every platform.

use std::io;

const QUOTE: u16 = b'"' as u16;
const SLASH: u16 = b'\\' as u16;
const EQUALS: u16 = b'=' as u16;

/// The command line of `program` and `args`, ending in NUL.
#[cfg(windows)]
pub fn command_line(
    program: &std::path::Path,
    args: &[std::ffi::OsString],
) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    // alloc: once per process spawned, which a CreateProcessW call dwarfs
    let program: Vec<u16> = program.as_os_str().encode_wide().collect();
    // alloc: once per process spawned, which a CreateProcessW call dwarfs
    let args: Vec<Vec<u16>> = args.iter().map(|it| it.encode_wide().collect()).collect();
    wide_command_line(&program, &args)
}

/// The environment block of `env`, ending in two NULs.
#[cfg(windows)]
pub fn environment(env: &[(std::ffi::OsString, std::ffi::OsString)]) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    wide_environment(
        env.iter()
            // alloc: once per process spawned, which a CreateProcessW call dwarfs
            .map(|(name, value)| (name.encode_wide().collect(), value.encode_wide().collect()))
            // alloc: once per process spawned, which a CreateProcessW call dwarfs
            .collect(),
    )
}

/// [`command_line`] of a program and arguments in code units.
pub fn wide_command_line(program: &[u16], args: &[Vec<u16>]) -> io::Result<Vec<u16>> {
    if program.iter().any(|&it| it == 0 || it == QUOTE) {
        // alloc: the error's message
        return Err(invalid(format!(
            "the program's path has a quote or a NUL in it: {}",
            // alloc: the error's message
            String::from_utf16_lossy(program)
        )));
    }
    let mut line =
        Vec::with_capacity(program.len() + 3 + args.iter().map(|it| it.len() + 3).sum::<usize>());
    line.push(QUOTE);
    line.extend_from_slice(program);
    line.push(QUOTE);
    for arg in args {
        line.push(u16::from(b' '));
        push_arg(&mut line, arg)?;
    }
    line.push(0);
    Ok(line)
}

fn push_arg(line: &mut Vec<u16>, arg: &[u16]) -> io::Result<()> {
    if arg.contains(&0) {
        // alloc: the error's message
        return Err(invalid(format!(
            "an argument has a NUL in it: {}",
            // alloc: the error's message
            String::from_utf16_lossy(arg)
        )));
    }
    let quoted =
        arg.is_empty() || arg.iter().any(|&it| it == u16::from(b' ') || it == u16::from(b'\t'));
    if quoted {
        line.push(QUOTE);
    }
    let mut slashes = 0;
    for &unit in arg {
        if unit == SLASH {
            slashes += 1;
        } else {
            if unit == QUOTE {
                line.extend(std::iter::repeat_n(SLASH, slashes + 1));
            }
            slashes = 0;
        }
        line.push(unit);
    }
    if quoted {
        line.extend(std::iter::repeat_n(SLASH, slashes));
        line.push(QUOTE);
    }
    Ok(())
}

/// [`environment`] of names and values in code units.
pub fn wide_environment(env: Vec<(Vec<u16>, Vec<u16>)>) -> io::Result<Vec<u16>> {
    let mut by_name = Vec::with_capacity(env.len());
    for (given, (name, value)) in env.into_iter().enumerate() {
        // A name may start with `=` (cmd's `=C:`), and has none after.
        if name.is_empty() || name.contains(&0) || name[1..].contains(&EQUALS) {
            // alloc: the error's message
            return Err(invalid(format!(
                "the environment has a variable that can't be named: {}",
                // alloc: the error's message
                String::from_utf16_lossy(&name)
            )));
        }
        if value.contains(&0) {
            // alloc: the error's message
            return Err(invalid(format!(
                "the variable {} has a NUL in it",
                // alloc: the error's message
                String::from_utf16_lossy(&name)
            )));
        }
        by_name.push((upper(&name), given, name, value));
    }
    by_name.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    by_name.dedup_by(|later, kept| later.0 == kept.0);
    let mut block =
        Vec::with_capacity(by_name.iter().map(|it| it.2.len() + it.3.len() + 2).sum::<usize>() + 2);
    for (_, _, name, value) in by_name {
        block.extend(name);
        block.push(EQUALS);
        block.extend(value);
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

/// Each code unit in upper case where that is one code unit, as Windows compares names.
fn upper(units: &[u16]) -> Vec<u16> {
    units
        .iter()
        .map(|&unit| {
            let Some(c) = char::from_u32(u32::from(unit)) else { return unit };
            let mut up = c.to_uppercase();
            match (up.next(), up.next()) {
                (Some(one), None) => u16::try_from(u32::from(one)).unwrap_or(unit),
                _ => unit,
            }
        })
        // alloc: once per variable of a process spawned, its name's sort key
        .collect()
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use hegel::generators as gs;
    use hegel::{PrintableGenerator, TestCase};

    use super::*;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    fn text(units: &[u16]) -> String {
        String::from_utf16(units).unwrap()
    }

    fn line(program: &str, args: &[&str]) -> io::Result<String> {
        let args: Vec<Vec<u16>> = args.iter().map(|it| wide(it)).collect();
        let line = wide_command_line(&wide(program), &args)?;
        assert_eq!(line.last(), Some(&0));
        Ok(text(&line[..line.len() - 1]))
    }

    fn environment(env: &[(&str, &str)]) -> io::Result<String> {
        wide_environment(env.iter().map(|(name, value)| (wide(name), wide(value))).collect())
            .map(|it| text(&it))
    }

    /// The arguments the C runtime splits the arguments of `line` (no NUL) into, after the
    /// quoted program: whitespace ends an argument outside quotes, `2n` backslashes and a quote
    /// are `n` backslashes and a quote that opens or closes, `2n + 1` are `n` and a literal
    /// quote, and other backslashes are themselves.
    fn split(line: &[u16]) -> Vec<Vec<u16>> {
        assert_eq!(line[0], QUOTE);
        let end = 2 + line[1..].iter().position(|&it| it == QUOTE).unwrap();
        let rest = &line[end..];
        let (mut args, mut arg, mut quoted, mut started, mut at) =
            (Vec::new(), Vec::new(), false, false, 0);
        while at < rest.len() {
            let unit = rest[at];
            if unit == SLASH {
                let n = rest[at..].iter().take_while(|&&it| it == SLASH).count();
                if rest.get(at + n) == Some(&QUOTE) {
                    arg.extend(std::iter::repeat_n(SLASH, n / 2));
                    if n % 2 == 1 {
                        arg.push(QUOTE);
                        at += 1;
                    }
                } else {
                    arg.extend(std::iter::repeat_n(SLASH, n));
                }
                at += n;
                started = true;
            } else if unit == QUOTE {
                quoted = !quoted;
                started = true;
                at += 1;
            } else if !quoted && (unit == u16::from(b' ') || unit == u16::from(b'\t')) {
                if started {
                    args.push(std::mem::take(&mut arg));
                    started = false;
                }
                at += 1;
            } else {
                arg.push(unit);
                started = true;
                at += 1;
            }
        }
        if started {
            args.push(arg);
        }
        args
    }

    /// blaze-core's driver's copy, kept as the reference: it quotes more (a newline, a
    /// vertical tab or a quote), which the runtime reads back the same.
    fn reference_line(program: &[u16], args: &[Vec<u16>]) -> Vec<u16> {
        fn quote(wide: &[u16], line: &mut Vec<u16>) {
            let special = |c: u16| matches!(c, 0x20 | 0x09 | 0x0A | 0x0B | 0x22);
            if !wide.is_empty() && !wide.iter().copied().any(special) {
                line.extend(wide);
                return;
            }
            line.push(QUOTE);
            let mut backslashes = 0;
            for &c in wide {
                if c == SLASH {
                    backslashes += 1;
                } else {
                    if c == QUOTE {
                        line.extend(std::iter::repeat_n(SLASH, backslashes + 1));
                    }
                    backslashes = 0;
                }
                line.push(c);
            }
            line.extend(std::iter::repeat_n(SLASH, backslashes));
            line.push(QUOTE);
        }
        let mut line = Vec::new();
        quote(program, &mut line);
        for arg in args {
            line.push(u16::from(b' '));
            quote(arg, &mut line);
        }
        line
    }

    /// blaze-core's driver's copy of the block, kept as the reference: it agrees on ASCII names.
    fn reference_environment(env: &[(Vec<u16>, Vec<u16>)]) -> Vec<u16> {
        let upper = |c: u16| match char::from_u32(u32::from(c)) {
            Some(it) if it.is_ascii_lowercase() => it.to_ascii_uppercase() as u16,
            _ => c,
        };
        let mut by_name: Vec<(Vec<u16>, &Vec<u16>, &Vec<u16>)> = Vec::new();
        for (name, value) in env {
            let key: Vec<u16> = name.iter().map(|&c| upper(c)).collect();
            by_name.retain(|(it, ..)| *it != key);
            by_name.push((key, name, value));
        }
        by_name.sort_by(|a, b| a.0.cmp(&b.0));
        let mut block = Vec::new();
        for (_, name, value) in by_name {
            block.extend(name);
            block.push(EQUALS);
            block.extend(value);
            block.push(0);
        }
        if block.is_empty() {
            block.push(0);
        }
        block.push(0);
        block
    }

    #[test]
    fn arguments_are_quoted_as_the_msvc_runtime_splits_them() {
        let args = [
            "",
            "plain",
            "a b",
            "tab\there",
            r#"say "hi""#,
            r"tail\",
            r"tail dir\",
            r#"a\\"b"#,
            r"\\host\share",
        ];
        let expected = [
            r#""C:\p q\a.exe""#,
            r#""""#,
            "plain",
            r#""a b""#,
            "\"tab\there\"",
            r#""say \"hi\"""#,
            r"tail\",
            r#""tail dir\\""#,
            r#"a\\\\\"b"#,
            r"\\host\share",
        ]
        .join(" ");
        assert_eq!(line(r"C:\p q\a.exe", &args).unwrap(), expected);
        assert!(line("a.exe", &["a\0b"]).is_err());
        assert!(line("a\".exe", &[]).is_err());
        assert_eq!(line("a.exe", &[]).unwrap(), "\"a.exe\"");
    }

    #[test]
    fn arguments_are_quoted_as_the_runtime_reads_them() {
        let program = r"C:\Program Files\x.exe";
        assert_eq!(line(program, &["plain", ""]).unwrap(), r#""C:\Program Files\x.exe" plain """#);
        assert_eq!(
            line(program, &["a b", r#"say "hi""#]).unwrap(),
            r#""C:\Program Files\x.exe" "a b" "say \"hi\"""#
        );
        assert_eq!(
            line(program, &[r"dir\", r"a\\b c\"]).unwrap(),
            r#""C:\Program Files\x.exe" dir\ "a\\b c\\""#
        );
        assert_eq!(line(program, &[r#"\"x"#]).unwrap(), r#""C:\Program Files\x.exe" \\\"x"#);
    }

    #[test]
    fn the_environment_block_is_sorted_without_regard_to_case_and_ends_in_two_nuls() {
        let env = [("b", "2"), ("Path", "x"), ("A", "1"), ("_z", ""), ("a", "3"), ("=C:", r"C:\w")];
        assert_eq!(environment(&env).unwrap(), "=C:=C:\\w\0a=3\0b=2\0Path=x\0_z=\0\0");
        assert_eq!(environment(&[]).unwrap(), "\0\0");
        assert!(environment(&[("A=B", "c")]).is_err());
        assert!(environment(&[("", "c")]).is_err());
        assert!(environment(&[("A", "c\0d")]).is_err());
    }

    #[test]
    fn the_environment_is_sorted_its_later_names_kept() {
        let env = [("Path", "a"), ("HOME", "h"), ("PATH", "b"), ("=C:", "C:\\")];
        assert_eq!(environment(&env).unwrap(), "=C:=C:\\\0HOME=h\0PATH=b\0\0");
        assert!(environment(&[("A=B", "")]).is_err());
    }

    #[test]
    fn names_compare_in_unicode_upper_case() {
        assert_eq!(environment(&[("é", "1"), ("É", "2")]).unwrap(), "É=2\0\0");
    }

    fn units(
        alphabet: &'static str,
        len: std::ops::Range<usize>,
    ) -> impl PrintableGenerator<Vec<u16>> {
        gs::vecs(gs::sampled_from(alphabet.encode_utf16().collect::<Vec<_>>()))
            .min_size(len.start)
            .max_size(len.end - 1)
    }

    #[hegel::test]
    fn the_runtime_reads_back_the_arguments_as_the_reference_does(tc: TestCase) {
        let args = tc.draw(gs::vecs(units("a \t\"\\\né", 0..8)).max_size(4));
        let program = wide(r"C:\p q\a.exe");
        let ours = wide_command_line(&program, &args).unwrap();
        assert_eq!(&split(&ours[..ours.len() - 1]), &args);
        assert_eq!(&split(&reference_line(&program, &args)), &args);
    }

    #[hegel::test]
    fn ascii_names_make_the_reference_block(tc: TestCase) {
        let env = tc.draw(
            gs::vecs(gs::tuples!(gs::booleans(), units("aAbB_", 1..3), units("a=\\", 0..3)))
                .max_size(5),
        );
        let env: Vec<(Vec<u16>, Vec<u16>)> = env
            .into_iter()
            .map(|(drive, name, value)| {
                let name = if drive { [vec![EQUALS], name].concat() } else { name };
                (name, value)
            })
            .collect();
        assert_eq!(wide_environment(env.clone()).unwrap(), reference_environment(&env));
    }
}
