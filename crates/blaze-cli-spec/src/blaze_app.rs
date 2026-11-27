//! Single source of truth for the `blaze` CLI ([`BLAZE_APP`]): commands, aliases, renamed
//! commands, flags, help text, usage lines, and `blaze-shell` completions (Rule 30).

use crate::app::{AppSpec, ArgKind, ArgSpec, CommandSpec, RenamedCommand};

const fn flag_bool(id: &'static str, long: &'static str, short: Option<char>, about: &'static str, hidden: bool) -> ArgSpec {
    ArgSpec {
        id,
        long: Some(long),
        short,
        value_hint: None,
        kind: ArgKind::Bool,
        about,
        hidden,
    }
}

const fn flag_value(
    id: &'static str,
    long: &'static str,
    short: Option<char>,
    value_hint: &'static str,
    about: &'static str,
    hidden: bool,
) -> ArgSpec {
    ArgSpec {
        id,
        long: Some(long),
        short,
        value_hint: Some(value_hint),
        kind: ArgKind::Value,
        about,
        hidden,
    }
}

const fn flag_multi(
    id: &'static str,
    long: &'static str,
    short: Option<char>,
    value_hint: &'static str,
    about: &'static str,
    hidden: bool,
) -> ArgSpec {
    ArgSpec {
        id,
        long: Some(long),
        short,
        value_hint: Some(value_hint),
        kind: ArgKind::MultiValue,
        about,
        hidden,
    }
}

const fn flag_rest(
    id: &'static str,
    long: &'static str,
    value_hint: &'static str,
    about: &'static str,
    hidden: bool,
) -> ArgSpec {
    ArgSpec {
        id,
        long: Some(long),
        short: None,
        value_hint: Some(value_hint),
        kind: ArgKind::RestUntilFlag,
        about,
        hidden,
    }
}

const CHECK_FLAGS: &[ArgSpec] = &[
    flag_value("color", "color", None, "auto|always|never", "Colorize diagnostics", false),
    flag_value("view", "view", None, "person|agent|json", "Diagnostic output format", false),
    flag_rest("fix", "fix", "[selector...]", "Apply fixes (id, code, path, or all)", false),
    flag_bool("each", "each", None, "Report each body individually", false),
    flag_bool("verbose", "verbose", Some('v'), "Print verbose diagnostic notes (including cfg-excluded files)", false),
    flag_value("workers", "workers", None, "<N>", "Maximum concurrent workers", false),
    flag_value("status-every", "status-every", None, "<secs>", "Emit periodic progress lines", false),
    flag_bool("stats", "stats", None, "Print telemetry summary at exit", false),
    flag_value("origin", "origin", None, "<url|folder>", "Shared remote or folder record origin", false),
    flag_value("origin-key", "origin-key", None, "<hex>", "Trusted Ed25519 public key for origin", false),
    flag_bool("names-oracle", "names-oracle", None, "Run differential names verification oracle", true),
    flag_value("driver", "driver", None, "auto|ready|phases", "Incremental check driver", true),
    flag_value("block-scopes", "block-scopes", None, "bodies|modules", "Block scope resolution strategy", true),
];

const BUILD_FLAGS: &[ArgSpec] = &[
    flag_multi("package", "package", Some('p'), "<name>", "Package to build", false),
    flag_value("bin", "bin", None, "<name>", "Binary target to build", false),
    flag_value("target", "target", None, "<triple>", "Build target (native or wasm64)", false),
    flag_value("output", "output", Some('o'), "<path>", "Destination executable path", false),
    flag_value("workers", "workers", None, "<N>", "Maximum concurrent workers", false),
    flag_bool("stats", "stats", None, "Print telemetry summary at exit", false),
    flag_bool("verbose", "verbose", Some('v'), "Print verbose diagnostic notes (including cfg-excluded files)", true),
    flag_value("color", "color", None, "auto|always|never", "Colorize diagnostics", true),
    flag_value("view", "view", None, "person|agent|json", "Diagnostic output format", true),
    flag_value("origin", "origin", None, "<url|folder>", "Shared remote or folder record origin", true),
    flag_value("origin-key", "origin-key", None, "<hex>", "Trusted Ed25519 public key for origin", true),
    flag_bool("names-oracle", "names-oracle", None, "Run differential names verification oracle", true),
    flag_value("driver", "driver", None, "auto|ready|phases", "Incremental check driver", true),
    flag_value("block-scopes", "block-scopes", None, "bodies|modules", "Block scope resolution strategy", true),
];

const RUN_FLAGS: &[ArgSpec] = &[
    flag_multi("package", "package", Some('p'), "<name>", "Package to run", false),
    flag_value("bin", "bin", None, "<name>", "Binary target to run", false),
    flag_bool("quiet", "quiet", Some('q'), "Do not print status lines", false),
    flag_multi("grant", "grant", None, "<k=v>", "Grant an environment/host capability", false),
    flag_bool("open", "open", None, "Open Explore viewer in browser", false),
    flag_value("workers", "workers", None, "<N>", "Maximum concurrent workers", false),
    flag_bool("stats", "stats", None, "Print telemetry summary at exit", false),
    flag_bool("verbose", "verbose", Some('v'), "Print verbose diagnostic notes (including cfg-excluded files)", false),
    flag_value("color", "color", None, "auto|always|never", "Colorize diagnostics", true),
    flag_value("view", "view", None, "person|agent|json", "Diagnostic output format", true),
    flag_bool("each", "each", None, "Report each body individually", true),
    flag_value("status-every", "status-every", None, "<secs>", "Emit periodic progress lines", true),
    flag_value("origin", "origin", None, "<url|folder>", "Shared remote or folder record origin", true),
    flag_value("origin-key", "origin-key", None, "<hex>", "Trusted Ed25519 public key for origin", true),
    flag_bool("names-oracle", "names-oracle", None, "Run differential names verification oracle", true),
    flag_value("driver", "driver", None, "auto|ready|phases", "Incremental check driver", true),
    flag_value("block-scopes", "block-scopes", None, "bodies|modules", "Block scope resolution strategy", true),
];

const TEST_FLAGS: &[ArgSpec] = &[
    flag_multi("package", "package", Some('p'), "<name>", "Package to test", false),
    flag_value("bin", "bin", None, "<name>", "Binary target to test", false),
    flag_value("workers", "workers", None, "<N>", "Maximum concurrent workers", false),
    flag_bool("stats", "stats", None, "Print telemetry summary at exit", false),
    flag_bool("verbose", "verbose", Some('v'), "Print verbose diagnostic notes (including cfg-excluded files)", false),
    flag_multi("grant", "grant", None, "<k=v>", "Grant an environment/host capability", true),
    flag_value("color", "color", None, "auto|always|never", "Colorize diagnostics", true),
    flag_value("view", "view", None, "person|agent|json", "Diagnostic output format", true),
    flag_bool("each", "each", None, "Report each body individually", true),
    flag_value("status-every", "status-every", None, "<secs>", "Emit periodic progress lines", true),
    flag_value("origin", "origin", None, "<url|folder>", "Shared remote or folder record origin", true),
    flag_value("origin-key", "origin-key", None, "<hex>", "Trusted Ed25519 public key for origin", true),
    flag_bool("names-oracle", "names-oracle", None, "Run differential names verification oracle", true),
    flag_value("driver", "driver", None, "auto|ready|phases", "Incremental check driver", true),
    flag_value("block-scopes", "block-scopes", None, "bodies|modules", "Block scope resolution strategy", true),
];

const CI_FLAGS: &[ArgSpec] = &[
    flag_value("format", "format", None, "auto|plain|github", "Output format", false),
    flag_value("todo", "todo", None, "<path>", "Path for generated CI todo artifact", false),
    flag_value("todo-format", "todo-format", None, "btab|tab|json", "CI todo artifact format", false),
    flag_bool("no-test", "no-test", None, "Skip running tests after check", false),
    flag_bool("no-todo", "no-todo", None, "Do not write a CI todo file", false),
    flag_bool("fix", "fix", None, "Apply automatic fixes before checking", false),
    flag_bool("allow-incomplete", "allow-incomplete", None, "Allow unchecked/incomplete items", false),
    flag_value("color", "color", None, "auto|always|never", "Colorize output", false),
    flag_value("workers", "workers", None, "<N>", "Maximum concurrent workers", false),
];

const INSPECT_FLAGS: &[ArgSpec] = &[
    flag_value("lines", "lines", None, "<start-end>", "Inspect an inclusive 1-based line range (e.g. 10-80 or 42)", false),
    flag_bool("json", "json", None, "Emit deterministic JSON output", false),
    flag_bool("no-docs", "no-docs", None, "Omit inline LSP hover docs and Referenced Docs appendix", false),
    flag_bool("no-outline", "no-outline", None, "Omit item outline header", false),
    flag_bool("no-types", "no-types", None, "Skip front2 semantic type inlay hints (outline + docs only)", false),
    flag_bool("all-exprs", "all-exprs", None, "Include all sub-expressions", false),
    flag_bool("no-coercions", "no-coercions", None, "Omit coercion hints", false),
    flag_bool("no-defaults", "no-defaults", None, "Omit defaulted argument hints", false),
    flag_bool("no-try", "no-try", None, "Omit `?` try-site hints", false),
    flag_bool("no-bindings", "no-bindings", None, "Omit local binding type hints", false),
    flag_value("edition", "edition", None, "<ed>", "Override Rust edition (2015..2024, blaze-2026.11)", false),
];

const CLEAN_FLAGS: &[ArgSpec] = &[
    flag_bool("store", "store", None, "Also remove the home's shared store", false),
    flag_bool("all", "all", None, "Remove checkout state, shared store, and derived caches", false),
];

const GC_FLAGS: &[ArgSpec] = &[
    flag_value("max-gb", "max-gb", None, "<N>", "Store size budget in GiB (0 = unlimited)", false),
    flag_value("max-bytes", "max-bytes", None, "<N>", "Store size budget in bytes (0 = unlimited)", false),
    flag_bool("dry-run", "dry-run", None, "Report what would be compacted or evicted without deleting", false),
    flag_bool("all-formats", "all-formats", None, "Remove all other-format store directories immediately", false),
    flag_bool("background", "background", None, "Run at background priority with non-blocking lock", false),
];

const PEER_FLAGS: &[ArgSpec] = &[
    flag_value("bind", "bind", None, "<ip:port>", "TCP bind address (e.g. 0.0.0.0:0)", false),
    flag_value("udp-port", "udp-port", None, "<port>", "UDP discovery port (default 41729)", false),
    flag_value("udp-bind", "udp-bind", None, "<ip>", "UDP discovery bind IP address", false),
    flag_value("idle-secs", "idle-secs", None, "<secs>", "Exit after idle timeout in seconds (default 1800)", false),
    flag_value("segments", "segments", None, "<dir>", "Directory of BZS1/BLPS segments to serve", false),
];

const WARM_TOOLCHAIN_FLAGS: &[ArgSpec] = &[
    flag_value("workers", "workers", None, "<N>", "Maximum concurrent workers", false),
    flag_value("origin", "origin", None, "<url|folder>", "Shared remote or folder record origin", false),
    flag_value("origin-key", "origin-key", None, "<hex>", "Trusted Ed25519 public key for origin", false),
    flag_bool("fresh", "fresh", None, "Re-warm toolchain records from scratch", false),
    flag_bool("stats", "stats", None, "Print telemetry summary at exit", false),
];

const STORE_FLAGS: &[ArgSpec] = &[
    flag_value("signer", "signer", None, "<key file>", "File holding the signer's Ed25519 seed (64 hex digits)", false),
];

const COMMON_FLAGS: &[ArgSpec] = &[
    flag_multi("package", "package", Some('p'), "<name>", "Select package in workspace", false),
    flag_value("bin", "bin", None, "<name>", "Select binary target", false),
    flag_value("output", "output", Some('o'), "<path>", "Output path for `blaze build`", false),
    flag_value("view", "view", None, "<mode>", "Output view: person, agent, or json", false),
    flag_value("color", "color", None, "<when>", "Color output: auto, always, or never", false),
    flag_rest("fix", "fix", "[sel...]", "Apply automatic fixes (`blaze check`)", false),
    flag_value("workers", "workers", None, "<N>", "Limit concurrent worker threads", false),
    flag_bool("stats", "stats", None, "Print telemetry summary at exit", false),
    flag_bool("verbose", "verbose", Some('v'), "Print verbose diagnostic notes (including cfg-excluded files)", false),
    flag_bool("help", "help", Some('h'), "Show help for blaze or a command", false),
    flag_bool("version", "version", Some('V'), "Print version information", false),
];

const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "check",
        aliases: &["engine-check"],
        hidden: false,
        about: "Check and lint a workspace incrementally",
        usage_args: "[dir] [options]",
        flags: CHECK_FLAGS,
        accepts_passthrough: false,
    },
    CommandSpec {
        name: "build",
        aliases: &[],
        hidden: false,
        about: "Build a standalone PGO-optimized native binary",
        usage_args: "[dir] [options]",
        flags: BUILD_FLAGS,
        accepts_passthrough: false,
    },
    CommandSpec {
        name: "run",
        aliases: &[],
        hidden: false,
        about: "Compile what is reached and run a binary",
        usage_args: "[dir] [options] [-- <program args>...]",
        flags: RUN_FLAGS,
        accepts_passthrough: true,
    },
    CommandSpec {
        name: "test",
        aliases: &[],
        hidden: false,
        about: "Run workspace tests incrementally",
        usage_args: "[dir] [options] [-- <test args>...]",
        flags: TEST_FLAGS,
        accepts_passthrough: true,
    },
    CommandSpec {
        name: "ci",
        aliases: &[],
        hidden: false,
        about: "Run CI checks with structured reporting",
        usage_args: "[dir] [options]",
        flags: CI_FLAGS,
        accepts_passthrough: false,
    },
    CommandSpec {
        name: "inspect",
        aliases: &[],
        hidden: false,
        about: "View a file, line slice, or item with inline types and LSP-style docs",
        usage_args: "<file.rs[:start-end|::item]> [options]",
        flags: INSPECT_FLAGS,
        accepts_passthrough: false,
    },
    CommandSpec {
        name: "why",
        aliases: &[],
        hidden: false,
        about: "Explain why a file or item was rechecked",
        usage_args: "<file[::item]> [options]",
        flags: CHECK_FLAGS,
        accepts_passthrough: false,
    },
    CommandSpec {
        name: "explain",
        aliases: &[],
        hidden: false,
        about: "Explain a diagnostic, lint, or exit status code",
        usage_args: "<code>",
        flags: &[],
        accepts_passthrough: false,
    },
    CommandSpec {
        name: "clean",
        aliases: &[],
        hidden: false,
        about: "Remove checkout state (and --store for home store)",
        usage_args: "[dir] [options]",
        flags: CLEAN_FLAGS,
        accepts_passthrough: false,
    },
    CommandSpec {
        name: "gc",
        aliases: &[],
        hidden: false,
        about: "Compact store and evict stale records",
        usage_args: "[options]",
        flags: GC_FLAGS,
        accepts_passthrough: false,
    },
    CommandSpec {
        name: "peer",
        aliases: &[],
        hidden: false,
        about: "Serve crate and segment records to LAN peers",
        usage_args: "[options]",
        flags: PEER_FLAGS,
        accepts_passthrough: false,
    },
    CommandSpec {
        name: "toolchain",
        aliases: &[],
        hidden: false,
        about: "Print the pinned Rust toolchain",
        usage_args: "",
        flags: &[],
        accepts_passthrough: false,
    },
    CommandSpec {
        name: "warm-toolchain",
        aliases: &[],
        hidden: false,
        about: "Pre-warm toolchain records into the store",
        usage_args: "[options]",
        flags: WARM_TOOLCHAIN_FLAGS,
        accepts_passthrough: false,
    },
    CommandSpec {
        name: "store",
        aliases: &[],
        hidden: false,
        about: "Publish signed toolchain records to a folder origin",
        usage_args: "publish <folder> --signer <key file>",
        flags: STORE_FLAGS,
        accepts_passthrough: false,
    },
    CommandSpec {
        name: "version",
        aliases: &[],
        hidden: false,
        about: "Print blaze and toolchain versions",
        usage_args: "",
        flags: &[],
        accepts_passthrough: false,
    },
    CommandSpec {
        name: "help",
        aliases: &[],
        hidden: false,
        about: "Show this help message",
        usage_args: "[command]",
        flags: &[],
        accepts_passthrough: false,
    },
    CommandSpec {
        name: "engine-dry",
        aliases: &[],
        hidden: true,
        about: "Dry-run the edit forward wake over the stored answer",
        usage_args: "<file> [options]",
        flags: CHECK_FLAGS,
        accepts_passthrough: false,
    },
    CommandSpec {
        name: "engine-keys",
        aliases: &[],
        hidden: true,
        about: "Print the stored answer's root and page keys",
        usage_args: "[options]",
        flags: CHECK_FLAGS,
        accepts_passthrough: false,
    },
    CommandSpec {
        name: "journal-helper",
        aliases: &[],
        hidden: true,
        about: "Run the checkout filesystem journal helper",
        usage_args: "[dir]",
        flags: &[],
        accepts_passthrough: false,
    },
];

const EXAMPLES: &[&str] = &[
    "blaze check",
    "blaze build -p app -o ./app",
    "blaze run -- --arg",
    "blaze test",
];

const RENAMED: &[RenamedCommand] = &[RenamedCommand {
    old: "export",
    new_cmd: "build",
    message: "use `blaze build` to build a standalone native executable",
}];

/// The ONE single source of truth for the `blaze` CLI (Rule 30).
pub const BLAZE_APP: AppSpec = AppSpec {
    name: "blaze",
    about: "incremental Rust compiler, builder, runner, and tester",
    commands: COMMANDS,
    common_flags: COMMON_FLAGS,
    examples: EXAMPLES,
    renamed: RENAMED,
};
