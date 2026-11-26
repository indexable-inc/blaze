//! Declarative `bstd::cli` command specification for `bgit` — single source of truth for
//! argument parsing, aliases, typo suggestions, `--version`, and ANSI-styled `--help`.

use bstd::cli::{CommandSpec, FlagSpec, OptSpec, PositionalSpec, SubcommandSpec};

const STATUS_SPEC: CommandSpec<'static> = CommandSpec {
    name: "status",
    about: "Show working tree status (native getattrlistbulk or O(dirty) VFS overlay)",
    flags: &[
        FlagSpec { long: "short", short: Some('s'), help: "Give output in short format", .. },
        FlagSpec {
            long: "branch",
            short: Some('b'),
            help: "Show branch and tracking info in short/porcelain format",
            ..
        },
        FlagSpec { long: "prompt", help: "Emit Starship-style prompt summary", .. },
        FlagSpec { long: "no-color", help: "Disable ANSI colors", .. },
        FlagSpec { long: "timing", help: "Print per-phase microsecond breakdown to stderr", .. },
    ],
    opts: &[
        OptSpec {
            long: "porcelain",
            value_name: "VER",
            default_missing_value: Some("v1"),
            help: "Give machine-readable output (v1)",
            ..
        },
        OptSpec {
            long: "untracked-files",
            short: Some('u'),
            value_name: "MODE",
            default_missing_value: Some("all"),
            help: "Show untracked files (no, normal, all)",
            ..
        },
        OptSpec {
            long: "color",
            value_name: "WHEN",
            default_missing_value: Some("always"),
            help: "Control ANSI colors (always, auto, never)",
            ..
        },
    ],
    positionals: &[PositionalSpec { name: "DIR", help: "Repository directory (default: .)", .. }],
    ..
};

const WT_ADD_SPEC: CommandSpec<'static> = CommandSpec {
    name: "add",
    about: "Create a worktree that auto-merges tracked branch updates and auto-deletes on merge",
    flags: &[
        FlagSpec { long: "vfs", help: "Mount worktree lazily via NFSv3 CoW VFS", .. },
        FlagSpec {
            long: "no-auto-delete",
            help: "Keep worktree folder after its branch is merged into the tracked branch",
            ..
        },
    ],
    opts: &[
        OptSpec { long: "from", value_name: "REV", help: "Base revision for a new branch", .. },
        OptSpec {
            long: "track",
            value_name: "BRANCH",
            help: "Upstream branch to auto-merge updates from (defaults to current HEAD branch)",
            ..
        },
    ],
    positionals: &[
        PositionalSpec {
            name: "REPO_OR_PATH",
            required: true,
            help: "Worktree path (or repo directory when 3 arguments are given)",
            ..
        },
        PositionalSpec {
            name: "BRANCH_OR_PATH",
            required: true,
            help: "Branch name (or worktree path when 3 arguments are given)",
            ..
        },
        PositionalSpec {
            name: "BRANCH",
            help: "Branch name (when repo directory is specified first)",
            ..
        },
    ],
    ..
};

const WT_LIST_SPEC: CommandSpec<'static> = CommandSpec {
    name: "list",
    about: "List registered bgit worktrees",
    positionals: &[PositionalSpec { name: "REPO", help: "Repository directory (default: .)", .. }],
    ..
};

const WT_SYNC_SPEC: CommandSpec<'static> = CommandSpec {
    name: "sync",
    about: "Sync worktrees against tracked branches (runs automatically via Git hooks)",
    flags: &[
        FlagSpec { long: "dry-run", help: "Show planned merges/deletions without modifying disk", .. },
        FlagSpec {
            long: "force",
            short: Some('f'),
            help: "Delete merged worktrees even with local changes",
            ..
        },
        FlagSpec { long: "from-hook", hidden: true, .. },
    ],
    positionals: &[PositionalSpec { name: "REPO", help: "Repository directory (default: .)", .. }],
    ..
};

const WT_WATCH_SPEC: CommandSpec<'static> = CommandSpec {
    name: "watch",
    about: "Continuously watch refs and auto-sync worktrees",
    opts: &[OptSpec {
        long: "interval-ms",
        value_name: "MS",
        default: Some("250"),
        help: "Polling interval in milliseconds (50..=10000)",
        ..
    }],
    positionals: &[PositionalSpec { name: "REPO", help: "Repository directory (default: .)", .. }],
    ..
};

const WT_REMOVE_SPEC: CommandSpec<'static> = CommandSpec {
    name: "remove",
    about: "Remove a registered worktree folder",
    flags: &[FlagSpec {
        long: "force",
        short: Some('f'),
        help: "Remove even if the worktree has uncommitted changes",
        ..
    }],
    positionals: &[
        PositionalSpec {
            name: "REPO_OR_TARGET",
            required: true,
            help: "Worktree path/branch (or repo directory when 2 arguments are given)",
            ..
        },
        PositionalSpec {
            name: "TARGET",
            help: "Worktree path or branch (when repo directory is specified first)",
            ..
        },
    ],
    ..
};

const WORKTREE_SPEC: CommandSpec<'static> = CommandSpec {
    name: "worktree",
    about: "Manage auto-merging, auto-deleting bgit worktrees",
    subcommands: &[
        SubcommandSpec {
            name: "add",
            about: "Create a new tracked worktree",
            spec: &WT_ADD_SPEC,
            ..
        },
        SubcommandSpec {
            name: "list",
            aliases: &["ls"],
            about: "List registered worktrees",
            spec: &WT_LIST_SPEC,
            ..
        },
        SubcommandSpec {
            name: "sync",
            about: "Manually trigger worktree sync (automatic on git merge/push)",
            spec: &WT_SYNC_SPEC,
            ..
        },
        SubcommandSpec {
            name: "watch",
            about: "Poll and sync worktrees continuously",
            spec: &WT_WATCH_SPEC,
            ..
        },
        SubcommandSpec {
            name: "remove",
            aliases: &["rm"],
            about: "Remove a registered worktree",
            spec: &WT_REMOVE_SPEC,
            ..
        },
    ],
    ..
};

const MOUNT_SPEC: CommandSpec<'static> = CommandSpec {
    name: "mount",
    about: "Lazily mount a .bgit repo via macOS loopback NFSv3 with CoW overlay",
    flags: &[
        FlagSpec { long: "list", short: Some('l'), help: "List active lazy VFS mounts", .. },
        FlagSpec { long: "ro", help: "Mount read-only", .. },
        FlagSpec { long: "read-only", hidden: true, .. },
        FlagSpec {
            long: "foreground",
            short: Some('f'),
            help: "Run the NFSv3 server in the foreground",
            ..
        },
    ],
    positionals: &[
        PositionalSpec { name: "REPO", help: "Repository directory", .. },
        PositionalSpec { name: "MOUNTPOINT", help: "Directory to mount onto", .. },
        PositionalSpec { name: "REV", help: "Revision to mount (default: HEAD)", .. },
    ],
    ..
};

const UNMOUNT_SPEC: CommandSpec<'static> = CommandSpec {
    name: "unmount",
    about: "Unmount an active lazy VFS mount and stop its daemon",
    flags: &[FlagSpec { long: "force", short: Some('f'), help: "Force unmount (`umount -f`)", .. }],
    positionals: &[PositionalSpec {
        name: "MOUNTPOINT",
        required: true,
        help: "Mounted directory path",
        ..
    }],
    ..
};

const VFS_SERVE_SPEC: CommandSpec<'static> = CommandSpec {
    name: "__vfs-serve",
    about: "Internal background NFSv3 VFS daemon",
    flags: &[FlagSpec { long: "ro", .. }],
    opts: &[OptSpec { long: "ready-file", value_name: "PATH", .. }],
    positionals: &[
        PositionalSpec { name: "REPO", required: true, .. },
        PositionalSpec { name: "MOUNTPOINT", required: true, .. },
        PositionalSpec { name: "REV", required: true, .. },
    ],
    ..
};

const INIT_SPEC: CommandSpec<'static> = CommandSpec {
    name: "init",
    about: "Initialize an empty native .bgit repository",
    positionals: &[PositionalSpec { name: "DIR", help: "Target directory (default: .)", .. }],
    ..
};

const INFECT_SPEC: CommandSpec<'static> = CommandSpec {
    name: "infect",
    about: "Convert an existing .git repo into native .bgit slabs and install auto-sync hooks",
    flags: &[FlagSpec { long: "raw", help: "Store uncompressed slabs instead of Zstd", .. }],
    positionals: &[PositionalSpec { name: "DIR", help: "Repository directory (default: .)", .. }],
    ..
};

const DISINFECT_SPEC: CommandSpec<'static> = CommandSpec {
    name: "disinfect",
    about: "Export a .bgit store back into a standard .git directory (parallel PACK v2 + idx)",
    positionals: &[
        PositionalSpec { name: "DIR", required: true, help: "Source .bgit repository", .. },
        PositionalSpec {
            name: "OUT_GIT_DIR",
            required: true,
            help: "Destination .git directory",
            ..
        },
    ],
    ..
};

const VERIFY_SPEC: CommandSpec<'static> = CommandSpec {
    name: "verify",
    about: "Verify BGIX CRC-32, every slab record CRC-32, and SHA-1 object integrity",
    positionals: &[PositionalSpec { name: "DIR", help: "Repository directory (default: .)", .. }],
    ..
};

const LOG_SPEC: CommandSpec<'static> = CommandSpec {
    name: "log",
    about: "Walk commit history directly from mmap .bgit slabs",
    opts: &[OptSpec {
        long: "max-count",
        short: Some('n'),
        value_name: "N",
        default: Some("20"),
        help: "Maximum number of commits to show",
        ..
    }],
    positionals: &[
        PositionalSpec { name: "DIR", help: "Repository directory (default: .)", .. },
        PositionalSpec { name: "REV", help: "Starting revision (default: HEAD)", .. },
    ],
    ..
};

const MERGE_BASE_SPEC: CommandSpec<'static> = CommandSpec {
    name: "merge-base",
    about: "Compute lowest common ancestor(s) via CGPH commit-graph",
    positionals: &[
        PositionalSpec { name: "DIR", required: true, help: "Repository directory", .. },
        PositionalSpec { name: "REV_A", required: true, help: "First revision", .. },
        PositionalSpec { name: "REV_B", required: true, help: "Second revision", .. },
    ],
    ..
};

const MERGE_SPEC: CommandSpec<'static> = CommandSpec {
    name: "merge",
    about: "Run O(changed) ORT 3-way merge and auto-update/clean tracking worktrees",
    flags: &[
        FlagSpec {
            long: "write-commit",
            help: "Write merge commit (or fast-forward) and update target branch ref",
            ..
        },
        FlagSpec {
            long: "keep-worktree",
            help: "Do not auto-delete the merged branch's worktree folder",
            ..
        },
        FlagSpec {
            long: "no-sync",
            help: "Do not auto-merge updates into downstream tracking worktrees",
            ..
        },
    ],
    positionals: &[
        PositionalSpec { name: "DIR", required: true, help: "Repository directory", .. },
        PositionalSpec { name: "OURS", required: true, help: "Target branch or revision", .. },
        PositionalSpec { name: "THEIRS", required: true, help: "Branch or revision to merge in", .. },
    ],
    ..
};

const SERVE_SPEC: CommandSpec<'static> = CommandSpec {
    name: "serve",
    about: "Serve a repository over Git Smart HTTP (Protocol v2) or stdio/SSH",
    opts: &[
        OptSpec { long: "http", value_name: "ADDR", help: "Listen address for Smart HTTP (e.g. 127.0.0.1:8080)", .. },
        OptSpec {
            long: "stdio",
            value_name: "SERVICE",
            help: "Serve one stdio session (`upload-pack` or `receive-pack`)",
            ..
        },
    ],
    positionals: &[PositionalSpec { name: "REPO", required: true, help: "Repository directory", .. }],
    ..
};

const LS_REMOTE_SPEC: CommandSpec<'static> = CommandSpec {
    name: "ls-remote",
    about: "List remote references via Git Protocol v2 `ls-refs`",
    positionals: &[PositionalSpec { name: "HTTP_URL", required: true, help: "Remote HTTP URL", .. }],
    ..
};

const CLONE_SPEC: CommandSpec<'static> = CommandSpec {
    name: "clone",
    about: "Clone a remote repository via Git Protocol v2 into .bgit (or lazily with --vfs)",
    flags: &[FlagSpec {
        long: "vfs",
        help: "Mount cloned repo lazily via NFSv3 with 0 checked-out files",
        ..
    }],
    positionals: &[
        PositionalSpec { name: "HTTP_URL", required: true, help: "Remote HTTP URL", .. },
        PositionalSpec { name: "DEST_DIR", required: true, help: "Destination directory", .. },
    ],
    ..
};

const PUSH_SPEC: CommandSpec<'static> = CommandSpec {
    name: "push",
    about: "Push a local ref to a remote Smart HTTP `receive-pack` endpoint",
    positionals: &[
        PositionalSpec { name: "HTTP_URL", required: true, help: "Remote HTTP URL", .. },
        PositionalSpec { name: "DIR", required: true, help: "Local repository directory", .. },
        PositionalSpec { name: "REF", required: true, help: "Ref to push (e.g. refs/heads/main)", .. },
    ],
    ..
};

const BENCH_SPEC: CommandSpec<'static> = CommandSpec {
    name: "bench",
    about: "Run head-to-head .bgit vs git floor benchmarks on a repository",
    positionals: &[PositionalSpec { name: "DIR", help: "Repository directory (default: .)", .. }],
    ..
};

/// Top-level `bgit` CLI specification.
pub(crate) const BGIT_CLI: CommandSpec<'static> = CommandSpec {
    name: "bgit",
    about: "bgit — sans-IO, Git-wire-compatible VCS engine (macOS / APFS native .bgit store)",
    version: Some(env!("CARGO_PKG_VERSION")),
    subcommands: &[
        SubcommandSpec {
            name: "status",
            aliases: &["st"],
            about: "Show working tree status",
            spec: &STATUS_SPEC,
            ..
        },
        SubcommandSpec {
            name: "worktree",
            aliases: &["wt"],
            about: "Manage auto-merging, auto-deleting worktrees",
            spec: &WORKTREE_SPEC,
            ..
        },
        SubcommandSpec {
            name: "mount",
            about: "Lazily mount a repo via NFSv3 CoW VFS",
            spec: &MOUNT_SPEC,
            ..
        },
        SubcommandSpec {
            name: "unmount",
            aliases: &["umount"],
            about: "Unmount a lazy VFS mount",
            spec: &UNMOUNT_SPEC,
            ..
        },
        SubcommandSpec { name: "__vfs-serve", spec: &VFS_SERVE_SPEC, hidden: true, .. },
        SubcommandSpec { name: "init", about: "Initialize an empty .bgit repo", spec: &INIT_SPEC, .. },
        SubcommandSpec {
            name: "infect",
            about: "Convert .git into native .bgit slabs + hooks",
            spec: &INFECT_SPEC,
            ..
        },
        SubcommandSpec {
            name: "disinfect",
            about: "Export .bgit back to standard .git pack+idx",
            spec: &DISINFECT_SPEC,
            ..
        },
        SubcommandSpec {
            name: "verify",
            about: "Verify BGIX + slab CRC-32 + SHA-1 integrity",
            spec: &VERIFY_SPEC,
            ..
        },
        SubcommandSpec { name: "log", about: "Walk commit history", spec: &LOG_SPEC, .. },
        SubcommandSpec {
            name: "merge-base",
            about: "Find LCA via CGPH commit-graph",
            spec: &MERGE_BASE_SPEC,
            ..
        },
        SubcommandSpec {
            name: "merge",
            about: "Run ORT 3-way merge",
            spec: &MERGE_SPEC,
            ..
        },
        SubcommandSpec {
            name: "serve",
            about: "Serve Smart HTTP or stdio Git protocol",
            spec: &SERVE_SPEC,
            ..
        },
        SubcommandSpec {
            name: "ls-remote",
            about: "List remote refs via Protocol v2",
            spec: &LS_REMOTE_SPEC,
            ..
        },
        SubcommandSpec {
            name: "clone",
            about: "Clone a remote repo (or --vfs lazily)",
            spec: &CLONE_SPEC,
            ..
        },
        SubcommandSpec { name: "push", about: "Push a ref via receive-pack", spec: &PUSH_SPEC, .. },
        SubcommandSpec {
            name: "bench",
            about: "Head-to-head .bgit vs git benchmark",
            spec: &BENCH_SPEC,
            ..
        },
    ],
    ..
};
