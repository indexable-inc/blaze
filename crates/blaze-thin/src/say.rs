//! cargo's own lines that a build with nothing to do ends with: the status word and the
//! `Finished` line. blaze-cargo's `cli::say` writes every other line of cargo's and takes
//! these from here, so the thin `blaze` and the engine write them with one function.

use std::{fmt::Write as _, time::Duration};

/// cargo's `HEADER` and `GOOD` (`util/style.rs`): bright green, bold.
pub const GOOD: &str = "\x1b[1m\x1b[92m";
pub const RESET: &str = "\x1b[0m";

/// The start of a status line in `out`: `word` right-aligned in twelve columns, then a space.
pub fn status_word(out: &mut String, word: &str, color: bool) {
    if color {
        out.push_str(GOOD);
    }
    out.extend(std::iter::repeat_n(' ', 12usize.saturating_sub(word.len())));
    out.push_str(word);
    if color {
        out.push_str(RESET);
    }
    out.push(' ');
}

/// A time as cargo's `util::elapsed` writes it: `0.12s`, `1m 05s`, into `out`.
pub fn elapsed_into(out: &mut String, took: Duration) {
    let secs = took.as_secs();
    // Writing to a String cannot fail.
    let _ = match secs >= 60 {
        true => write!(out, "{}m {:02}s", secs / 60, secs % 60),
        false => write!(out, "{secs}.{:02}s", took.subsec_nanos() / 10_000_000),
    };
}

/// What follows `Finished`: `` `dev` profile [unoptimized + debuginfo] target(s) in 0.12s ``,
/// of the daemon's `profile` (`Response::Built`), into `out`.
pub fn finished_into(out: &mut String, profile: &str, took: Duration) {
    out.push_str(profile);
    out.push_str(" target(s) in ");
    elapsed_into(out, took);
}

/// `arg` as `shell-escape` leaves it unquoted on Unix: not empty, and only letters, digits
/// and `-_=/,.+`. cargo shows any other argument quoted (blaze-cargo's `cli::say::escape_unix`,
/// which asks this).
pub fn plain(arg: &str) -> bool {
    let plain =
        |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '=' | '/' | ',' | '.' | '+');
    !arg.is_empty() && arg.chars().all(plain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_word_is_right_aligned_in_twelve_columns() {
        let mut out = String::new();
        status_word(&mut out, "Finished", false);
        assert_eq!(out, "    Finished ");
        let mut out = String::new();
        status_word(&mut out, "Running", true);
        assert_eq!(out, "\x1b[1m\x1b[92m     Running\x1b[0m ");
    }

    #[test]
    fn the_finished_line_is_cargos() {
        let mut out = String::new();
        finished_into(&mut out, "`dev` profile [unoptimized + debuginfo]", Duration::from_millis(120));
        assert_eq!(out, "`dev` profile [unoptimized + debuginfo] target(s) in 0.12s");
        let mut out = String::new();
        elapsed_into(&mut out, Duration::from_secs(65));
        assert_eq!(out, "1m 05s");
    }

    #[test]
    fn only_what_the_shell_reads_as_itself_is_plain() {
        assert!(plain("--name=a/b,c.d+e_f-1"));
        for quoted in ["", "a b", "a'b", "$HOME", "a:b", "é"] {
            assert!(!plain(quoted), "{quoted:?}");
        }
    }
}
