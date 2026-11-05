//! The names cargo gives the signals that end a process: what cargo-util's
//! `exit_status_to_string` writes after `signal: N`, which build-script failures and test
//! exits print the same as cargo.

/// `, SIGSEGV: invalid memory reference` for `SIGSEGV`: what cargo-util's
/// `exit_status_to_string` writes after `signal: {signal}`. Empty for a signal it does not
/// name, and off Unix, where no signal ends a process.
#[cfg(unix)]
pub fn described(signal: i32) -> &'static str {
    match signal {
        libc::SIGABRT => ", SIGABRT: process abort signal",
        libc::SIGALRM => ", SIGALRM: alarm clock",
        libc::SIGFPE => ", SIGFPE: erroneous arithmetic operation",
        libc::SIGHUP => ", SIGHUP: hangup",
        libc::SIGILL => ", SIGILL: illegal instruction",
        libc::SIGINT => ", SIGINT: terminal interrupt signal",
        libc::SIGKILL => ", SIGKILL: kill",
        libc::SIGPIPE => ", SIGPIPE: write on a pipe with no one to read",
        libc::SIGQUIT => ", SIGQUIT: terminal quit signal",
        libc::SIGSEGV => ", SIGSEGV: invalid memory reference",
        libc::SIGTERM => ", SIGTERM: termination signal",
        libc::SIGBUS => ", SIGBUS: access to undefined memory",
        libc::SIGSYS => ", SIGSYS: bad system call",
        libc::SIGTRAP => ", SIGTRAP: trace/breakpoint trap",
        _ => "",
    }
}

#[cfg(not(unix))]
pub fn described(_: i32) -> &'static str {
    ""
}

#[cfg(all(test, unix))]
mod tests {
    use super::described;

    /// The table as `blaze-cargo/src/cli/say.rs` and `blaze-build-script/src/run.rs` each had
    /// it, by the signals' names rather than `libc`'s numbers, so a renumbering shows.
    fn reference(signal: i32) -> &'static str {
        let names: [(i32, &str); 14] = [
            (libc::SIGABRT, "SIGABRT: process abort signal"),
            (libc::SIGALRM, "SIGALRM: alarm clock"),
            (libc::SIGFPE, "SIGFPE: erroneous arithmetic operation"),
            (libc::SIGHUP, "SIGHUP: hangup"),
            (libc::SIGILL, "SIGILL: illegal instruction"),
            (libc::SIGINT, "SIGINT: terminal interrupt signal"),
            (libc::SIGKILL, "SIGKILL: kill"),
            (libc::SIGPIPE, "SIGPIPE: write on a pipe with no one to read"),
            (libc::SIGQUIT, "SIGQUIT: terminal quit signal"),
            (libc::SIGSEGV, "SIGSEGV: invalid memory reference"),
            (libc::SIGTERM, "SIGTERM: termination signal"),
            (libc::SIGBUS, "SIGBUS: access to undefined memory"),
            (libc::SIGSYS, "SIGSYS: bad system call"),
            (libc::SIGTRAP, "SIGTRAP: trace/breakpoint trap"),
        ];
        names.iter().find(|it| it.0 == signal).map_or("", |it| it.1)
    }

    #[test]
    fn every_signal_reads_as_cargo_names_it() {
        for signal in -2..=130 {
            let expected = reference(signal);
            let got = described(signal);
            match expected {
                "" => assert_eq!(got, "", "signal {signal}"),
                name => assert_eq!(got.strip_prefix(", "), Some(name), "signal {signal}"),
            }
        }
    }

    #[test]
    fn a_segfault_is_an_invalid_memory_reference() {
        assert_eq!(
            format!("signal: {}{}", libc::SIGSEGV, described(libc::SIGSEGV)),
            format!("signal: {}, SIGSEGV: invalid memory reference", libc::SIGSEGV)
        );
    }
}
