//! Declarative `bstd::cli` command specification for `bgit` — single source of truth for
//! argument parsing, aliases, typo suggestions, `--version`, ANSI-styled `--help`, and
//! `blaze-shell` Tab completions / zero-Tab live hints.

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

const ADD_SPEC: CommandSpec<'static> = CommandSpec {
    name: "add",
    about: "Stage working tree file changes into the index",
    flags: &[
        FlagSpec { long: "all", short: Some('A'), help: "Stage all changes including untracked & deletions", .. },
        FlagSpec { long: "update", short: Some('u'), help: "Stage modified and deleted tracked files only", .. },
        FlagSpec { long: "force", short: Some('f'), help: "Allow adding otherwise ignored files", .. },
    ],
    positionals: &[PositionalSpec {
        name: "PATH",
        multiple: true,
        help: "Files or directories to stage",
        ..
    }],
    ..
};

const COMMIT_SPEC: CommandSpec<'static> = CommandSpec {
    name: "commit",
    about: "Record staged changes to the repository",
    flags: &[
        FlagSpec { long: "all", short: Some('a'), help: "Automatically stage modified and deleted tracked files", .. },
        FlagSpec { long: "amend", help: "Amend the previous commit instead of creating a new one", .. },
        FlagSpec { long: "allow-empty", help: "Allow creating a commit with no tree changes", .. },
    ],
    opts: &[OptSpec {
        long: "message",
        short: Some('m'),
        value_name: "MSG",
        multiple: true,
        help: "Use the given message as the commit message",
        ..
    }],
    ..
};

const BRANCH_SPEC: CommandSpec<'static> = CommandSpec {
    name: "branch",
    about: "List, create, rename, or delete branches",
    flags: &[
        FlagSpec { long: "all", short: Some('a'), help: "List both local and remote-tracking branches", .. },
        FlagSpec { long: "remotes", short: Some('r'), help: "List remote-tracking branches", .. },
        FlagSpec { long: "verbose", short: Some('v'), help: "Show SHA-1 and commit subject for each branch", .. },
        FlagSpec { long: "list", short: Some('l'), help: "List branches", .. },
        FlagSpec { long: "show-current", help: "Print the name of the current branch", .. },
        FlagSpec { long: "delete", short: Some('d'), help: "Delete a fully merged branch", .. },
        FlagSpec { long: "force-delete", short: Some('D'), help: "Force-delete a branch regardless of merge state", .. },
        FlagSpec { long: "move", short: Some('m'), help: "Rename a branch", .. },
        FlagSpec { long: "force-move", short: Some('M'), help: "Force-rename a branch", .. },
        FlagSpec { long: "force", short: Some('f'), help: "Reset branch to start-point even if it exists", .. },
    ],
    positionals: &[PositionalSpec {
        name: "BRANCH",
        multiple: true,
        help: "Branch name (and optional start-point or rename target)",
        ..
    }],
    ..
};

const CHECKOUT_SPEC: CommandSpec<'static> = CommandSpec {
    name: "checkout",
    about: "Switch branches or restore working tree files",
    flags: &[
        FlagSpec { long: "force", short: Some('f'), help: "Proceed even if local changes would be overwritten", .. },
        FlagSpec { long: "detach", short: Some('d'), help: "Detach HEAD at named commit", .. },
    ],
    opts: &[
        OptSpec { long: "create", short: Some('b'), value_name: "BRANCH", help: "Create and checkout a new branch", .. },
        OptSpec {
            long: "force-create",
            short: Some('B'),
            value_name: "BRANCH",
            help: "Create/reset and checkout a branch",
            ..
        },
    ],
    positionals: &[PositionalSpec {
        name: "TARGET",
        multiple: true,
        help: "Branch, revision, or file path(s) to checkout",
        ..
    }],
    ..
};

const SWITCH_SPEC: CommandSpec<'static> = CommandSpec {
    name: "switch",
    about: "Switch branches cleanly without path checkout ambiguity",
    flags: &[
        FlagSpec { long: "force", short: Some('f'), help: "Discard local changes when switching", .. },
        FlagSpec { long: "detach", short: Some('d'), help: "Switch to a commit for inspection (detached HEAD)", .. },
    ],
    opts: &[
        OptSpec { long: "create", short: Some('c'), value_name: "BRANCH", help: "Create and switch to a new branch", .. },
        OptSpec {
            long: "force-create",
            short: Some('C'),
            value_name: "BRANCH",
            help: "Create/reset and switch to a branch",
            ..
        },
    ],
    positionals: &[PositionalSpec { name: "BRANCH", help: "Target branch or start-point", .. }],
    ..
};

const DIFF_SPEC: CommandSpec<'static> = CommandSpec {
    name: "diff",
    about: "Show unified diffs between working tree, index, and commits",
    flags: &[
        FlagSpec { long: "staged", help: "Show changes staged in the index vs HEAD", .. },
        FlagSpec { long: "cached", help: "Alias for --staged", .. },
        FlagSpec { long: "stat", help: "Show diffstat summary instead of full patch", .. },
        FlagSpec { long: "name-only", help: "Show only names of changed files", .. },
        FlagSpec { long: "name-status", help: "Show names and status (A/M/D) of changed files", .. },
        FlagSpec { long: "no-color", help: "Disable ANSI colors", .. },
    ],
    positionals: &[PositionalSpec {
        name: "REV_OR_PATH",
        multiple: true,
        help: "Optional revision(s) (`A`, `A B`, `A..B`, `A...B`) and/or path filters",
        ..
    }],
    ..
};

const SHOW_SPEC: CommandSpec<'static> = CommandSpec {
    name: "show",
    about: "Show commit metadata + diff, tree listing, or `<rev>:<path>` blob",
    flags: &[
        FlagSpec { long: "stat", help: "Show diffstat summary instead of full patch", .. },
        FlagSpec { long: "name-only", help: "Show only changed file names", .. },
        FlagSpec { long: "no-color", help: "Disable ANSI colors", .. },
    ],
    positionals: &[PositionalSpec {
        name: "OBJECT",
        help: "Commit, tag, tree, or `<rev>:<path>` (default: HEAD)",
        ..
    }],
    ..
};

const RESET_SPEC: CommandSpec<'static> = CommandSpec {
    name: "reset",
    about: "Reset HEAD, index, or working tree to a specified revision",
    flags: &[
        FlagSpec { long: "soft", help: "Move HEAD only; keep index and working tree", .. },
        FlagSpec { long: "mixed", help: "Move HEAD and reset index (default)", .. },
        FlagSpec { long: "hard", help: "Reset HEAD, index, and working tree (discards local edits)", .. },
    ],
    positionals: &[PositionalSpec {
        name: "REV_OR_PATH",
        multiple: true,
        help: "Target commit (default: HEAD) or file path(s) to unstage",
        ..
    }],
    ..
};

const RESTORE_SPEC: CommandSpec<'static> = CommandSpec {
    name: "restore",
    about: "Restore working tree or staged files from index or a revision",
    flags: &[
        FlagSpec { long: "staged", short: Some('S'), help: "Restore the index (unstage files)", .. },
        FlagSpec { long: "worktree", short: Some('W'), help: "Restore the working tree", .. },
    ],
    opts: &[OptSpec {
        long: "source",
        short: Some('s'),
        value_name: "REV",
        help: "Restore files from the given commit or branch",
        ..
    }],
    positionals: &[PositionalSpec {
        name: "PATH",
        required: true,
        multiple: true,
        help: "File or directory path(s) to restore",
        ..
    }],
    ..
};

const RM_SPEC: CommandSpec<'static> = CommandSpec {
    name: "rm",
    about: "Remove files from the working tree and index",
    flags: &[
        FlagSpec { long: "cached", help: "Unstage and remove from index only; keep on disk", .. },
        FlagSpec { long: "force", short: Some('f'), help: "Force removal", .. },
        FlagSpec { long: "recursive", short: Some('r'), help: "Allow recursive removal of directories", .. },
    ],
    positionals: &[PositionalSpec {
        name: "PATH",
        required: true,
        multiple: true,
        help: "Path(s) to remove",
        ..
    }],
    ..
};

const MV_SPEC: CommandSpec<'static> = CommandSpec {
    name: "mv",
    about: "Move or rename a file in the working tree and index",
    flags: &[FlagSpec { long: "force", short: Some('f'), help: "Overwrite destination if it exists", .. }],
    positionals: &[
        PositionalSpec { name: "SRC", required: true, help: "Source file path", .. },
        PositionalSpec { name: "DST", required: true, help: "Destination path or directory", .. },
    ],
    ..
};

const TAG_SPEC: CommandSpec<'static> = CommandSpec {
    name: "tag",
    about: "List, create, or delete lightweight or annotated tags",
    flags: &[
        FlagSpec { long: "list", short: Some('l'), help: "List tags", .. },
        FlagSpec { long: "annotate", short: Some('a'), help: "Create an annotated tag object", .. },
        FlagSpec { long: "delete", short: Some('d'), help: "Delete existing tag(s)", .. },
        FlagSpec { long: "force", short: Some('f'), help: "Replace an existing tag", .. },
    ],
    opts: &[OptSpec {
        long: "message",
        short: Some('m'),
        value_name: "MSG",
        help: "Tag message (implies -a annotated tag)",
        ..
    }],
    positionals: &[PositionalSpec {
        name: "TAG",
        multiple: true,
        help: "Tag name (and optional target revision)",
        ..
    }],
    ..
};

const REV_PARSE_SPEC: CommandSpec<'static> = CommandSpec {
    name: "rev-parse",
    about: "Resolve revisions, HEAD branch name, or repository paths",
    flags: &[
        FlagSpec { long: "show-toplevel", help: "Print absolute path of repository root", .. },
        FlagSpec { long: "git-dir", help: "Print path to .git or .bgit directory", .. },
        FlagSpec { long: "is-inside-work-tree", help: "Print `true` when inside a working tree", .. },
        FlagSpec { long: "abbrev-ref", help: "Print short branch name for HEAD", .. },
    ],
    opts: &[OptSpec {
        long: "short",
        value_name: "N",
        default_missing_value: Some("7"),
        help: "Shorten object IDs to N hex characters (default: 7)",
        ..
    }],
    positionals: &[PositionalSpec {
        name: "REV",
        multiple: true,
        help: "Revision expression(s) (`HEAD`, `HEAD~1`, `main^2`, short OID, …)",
        ..
    }],
    ..
};

const SHOW_REF_SPEC: CommandSpec<'static> = CommandSpec {
    name: "show-ref",
    about: "List references in the local repository",
    flags: &[
        FlagSpec { long: "heads", help: "Show only refs/heads/*", .. },
        FlagSpec { long: "tags", help: "Show only refs/tags/*", .. },
        FlagSpec { long: "hash", short: Some('s'), help: "Show only the OID hash", .. },
    ],
    ..
};

const REMOTE_ADD_SPEC: CommandSpec<'static> = CommandSpec {
    name: "add",
    about: "Add a named remote repository URL",
    positionals: &[
        PositionalSpec { name: "NAME", required: true, help: "Remote name (e.g. origin)", .. },
        PositionalSpec { name: "URL", required: true, help: "Remote repository URL", .. },
    ],
    ..
};

const REMOTE_REMOVE_SPEC: CommandSpec<'static> = CommandSpec {
    name: "remove",
    about: "Remove a configured remote",
    positionals: &[PositionalSpec { name: "NAME", required: true, help: "Remote name", .. }],
    ..
};

const REMOTE_RENAME_SPEC: CommandSpec<'static> = CommandSpec {
    name: "rename",
    about: "Rename a configured remote",
    positionals: &[
        PositionalSpec { name: "OLD", required: true, help: "Existing remote name", .. },
        PositionalSpec { name: "NEW", required: true, help: "New remote name", .. },
    ],
    ..
};

const REMOTE_GET_URL_SPEC: CommandSpec<'static> = CommandSpec {
    name: "get-url",
    about: "Print the URL configured for a remote",
    positionals: &[PositionalSpec { name: "NAME", required: true, help: "Remote name", .. }],
    ..
};

const REMOTE_SET_URL_SPEC: CommandSpec<'static> = CommandSpec {
    name: "set-url",
    about: "Change the URL configured for a remote",
    positionals: &[
        PositionalSpec { name: "NAME", required: true, help: "Remote name", .. },
        PositionalSpec { name: "URL", required: true, help: "New URL", .. },
    ],
    ..
};

const REMOTE_SPEC: CommandSpec<'static> = CommandSpec {
    name: "remote",
    about: "Manage tracked remote repositories",
    flags: &[FlagSpec { long: "verbose", short: Some('v'), help: "Show remote URLs", .. }],
    subcommands: &[
        SubcommandSpec { name: "add", about: "Add a new remote", spec: &REMOTE_ADD_SPEC, .. },
        SubcommandSpec {
            name: "remove",
            aliases: &["rm"],
            about: "Remove a remote",
            spec: &REMOTE_REMOVE_SPEC,
            ..
        },
        SubcommandSpec { name: "rename", about: "Rename a remote", spec: &REMOTE_RENAME_SPEC, .. },
        SubcommandSpec {
            name: "get-url",
            about: "Print URL for a remote",
            spec: &REMOTE_GET_URL_SPEC,
            ..
        },
        SubcommandSpec {
            name: "set-url",
            about: "Update URL for a remote",
            spec: &REMOTE_SET_URL_SPEC,
            ..
        },
    ],
    ..
};

const CONFIG_SPEC: CommandSpec<'static> = CommandSpec {
    name: "config",
    about: "Get, set, unset, or list repository and global configuration",
    flags: &[
        FlagSpec { long: "list", short: Some('l'), help: "List all variables set in config", .. },
        FlagSpec { long: "get", help: "Get the value for a given key", .. },
        FlagSpec { long: "unset", help: "Remove the line matching the key", .. },
        FlagSpec { long: "local", help: "Use repository config file only", .. },
        FlagSpec { long: "global", help: "Use global ~/.gitconfig file", .. },
    ],
    positionals: &[
        PositionalSpec { name: "KEY", help: "Config key (e.g. user.name)", .. },
        PositionalSpec { name: "VALUE", help: "Value to assign", .. },
    ],
    ..
};

const STASH_PUSH_SPEC: CommandSpec<'static> = CommandSpec {
    name: "push",
    about: "Save local modifications to a new stash entry and clean working tree",
    flags: &[FlagSpec {
        long: "include-untracked",
        short: Some('u'),
        help: "Include untracked files in the stash",
        ..
    }],
    opts: &[OptSpec {
        long: "message",
        short: Some('m'),
        value_name: "MSG",
        help: "Description for the stash entry",
        ..
    }],
    ..
};

const STASH_LIST_SPEC: CommandSpec<'static> = CommandSpec {
    name: "list",
    about: "List saved stash entries",
    ..
};

const STASH_POP_SPEC: CommandSpec<'static> = CommandSpec {
    name: "pop",
    about: "Apply a stash entry and remove it from the stash stack",
    positionals: &[PositionalSpec { name: "STASH", help: "Stash index or stash@{n} (default: 0)", .. }],
    ..
};

const STASH_APPLY_SPEC: CommandSpec<'static> = CommandSpec {
    name: "apply",
    about: "Apply a stash entry without removing it",
    positionals: &[PositionalSpec { name: "STASH", help: "Stash index or stash@{n} (default: 0)", .. }],
    ..
};

const STASH_DROP_SPEC: CommandSpec<'static> = CommandSpec {
    name: "drop",
    about: "Remove a single stash entry",
    positionals: &[PositionalSpec { name: "STASH", help: "Stash index or stash@{n} (default: 0)", .. }],
    ..
};

const STASH_SHOW_SPEC: CommandSpec<'static> = CommandSpec {
    name: "show",
    about: "Show diff recorded in a stash entry",
    positionals: &[PositionalSpec { name: "STASH", help: "Stash index or stash@{n} (default: 0)", .. }],
    ..
};

const STASH_CLEAR_SPEC: CommandSpec<'static> = CommandSpec {
    name: "clear",
    about: "Remove all stash entries",
    ..
};

const STASH_SPEC: CommandSpec<'static> = CommandSpec {
    name: "stash",
    about: "Stash changes in a dirty working directory away",
    flags: &[FlagSpec {
        long: "include-untracked",
        short: Some('u'),
        help: "Include untracked files when pushing a stash",
        ..
    }],
    opts: &[OptSpec {
        long: "message",
        short: Some('m'),
        value_name: "MSG",
        help: "Description for the stash entry",
        ..
    }],
    subcommands: &[
        SubcommandSpec {
            name: "push",
            aliases: &["save"],
            about: "Save dirty changes to stash",
            spec: &STASH_PUSH_SPEC,
            ..
        },
        SubcommandSpec { name: "list", about: "List stash entries", spec: &STASH_LIST_SPEC, .. },
        SubcommandSpec { name: "pop", about: "Apply and drop stash", spec: &STASH_POP_SPEC, .. },
        SubcommandSpec { name: "apply", about: "Apply stash without dropping", spec: &STASH_APPLY_SPEC, .. },
        SubcommandSpec { name: "drop", about: "Delete a stash entry", spec: &STASH_DROP_SPEC, .. },
        SubcommandSpec { name: "show", about: "Show diff of a stash entry", spec: &STASH_SHOW_SPEC, .. },
        SubcommandSpec { name: "clear", about: "Delete all stash entries", spec: &STASH_CLEAR_SPEC, .. },
    ],
    ..
};

const CHERRY_PICK_SPEC: CommandSpec<'static> = CommandSpec {
    name: "cherry-pick",
    about: "Apply changes introduced by an existing commit via ORT 3-way merge",
    flags: &[FlagSpec {
        long: "no-commit",
        short: Some('n'),
        help: "Apply changes to index and working tree without committing",
        ..
    }],
    positionals: &[PositionalSpec { name: "REV", required: true, help: "Commit to cherry-pick", .. }],
    ..
};

const REVERT_SPEC: CommandSpec<'static> = CommandSpec {
    name: "revert",
    about: "Create a new commit reversing an existing commit via ORT 3-way merge",
    flags: &[FlagSpec {
        long: "no-commit",
        short: Some('n'),
        help: "Apply reverse diff to index and working tree without committing",
        ..
    }],
    positionals: &[PositionalSpec { name: "REV", required: true, help: "Commit to revert", .. }],
    ..
};

const CLEAN_SPEC: CommandSpec<'static> = CommandSpec {
    name: "clean",
    about: "Remove untracked files from the working tree",
    flags: &[
        FlagSpec { long: "dry-run", short: Some('n'), help: "Show what would be removed without deleting", .. },
        FlagSpec { long: "force", short: Some('f'), help: "Actually remove untracked files", .. },
        FlagSpec { long: "directories", short: Some('d'), help: "Prune empty untracked directories", .. },
        FlagSpec { long: "ignored", short: Some('x'), help: "Also remove .gitignore-ignored files", .. },
    ],
    ..
};

const FETCH_SPEC: CommandSpec<'static> = CommandSpec {
    name: "fetch",
    about: "Download objects and refs via Git Protocol v2 (HTTP/2+TLS1.3 or SSH)",
    opts: &[
        OptSpec { long: "depth", value_name: "N", help: "Limit fetching to N commits (shallow)", .. },
        OptSpec {
            long: "filter",
            value_name: "SPEC",
            help: "Partial clone filter (e.g. blob:none or tree:0)",
            ..
        },
    ],
    positionals: &[PositionalSpec {
        name: "REMOTE",
        help: "Remote name or URL (https://, git@host:org/repo, github:org/repo; default: origin)",
        ..
    }],
    ..
};

const PULL_SPEC: CommandSpec<'static> = CommandSpec {
    name: "pull",
    about: "Fetch from a remote and merge into the current branch",
    positionals: &[
        PositionalSpec { name: "REMOTE", help: "Remote name (default: origin)", .. },
        PositionalSpec { name: "BRANCH", help: "Remote branch name (default: current branch)", .. },
    ],
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
    flags: &[
        FlagSpec { long: "oneline", help: "Format each commit as `<short-sha> <subject>`", .. },
        FlagSpec { long: "no-color", help: "Disable ANSI colors", .. },
    ],
    opts: &[OptSpec {
        long: "max-count",
        short: Some('n'),
        value_name: "N",
        default: Some("20"),
        help: "Maximum number of commits to show",
        ..
    }],
    positionals: &[
        PositionalSpec { name: "REV_OR_DIR", help: "Revision or repository directory (default: HEAD)", .. },
        PositionalSpec { name: "REV", help: "Starting revision when DIR is specified first", .. },
    ],
    ..
};

const MERGE_BASE_SPEC: CommandSpec<'static> = CommandSpec {
    name: "merge-base",
    about: "Compute lowest common ancestor(s) via CGPH commit-graph",
    positionals: &[
        PositionalSpec { name: "REV_A_OR_DIR", required: true, help: "First revision (or repo directory)", .. },
        PositionalSpec { name: "REV_B_OR_REV_A", required: true, help: "Second revision (or REV_A with DIR)", .. },
        PositionalSpec { name: "REV_B", help: "Second revision (when DIR is specified first)", .. },
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
        PositionalSpec {
            name: "BRANCH_OR_DIR",
            required: true,
            help: "Branch to merge into HEAD (or repo directory when 3 args given)",
            ..
        },
        PositionalSpec { name: "OURS", help: "Target branch or revision (when DIR is specified first)", .. },
        PositionalSpec { name: "THEIRS", help: "Branch or revision to merge in (when DIR is specified first)", .. },
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
    positionals: &[PositionalSpec { name: "REMOTE_OR_URL", help: "Remote name or HTTP URL (default: origin)", .. }],
    ..
};

const CLONE_SPEC: CommandSpec<'static> = CommandSpec {
    name: "clone",
    about: "Clone a remote repo via Protocol v2 (HTTP/2+TLS1.3 or SSH) into .bgit",
    flags: &[FlagSpec {
        long: "vfs",
        help: "Mount cloned repo lazily via NFSv3 with 0 checked-out files",
        ..
    }],
    opts: &[
        OptSpec { long: "depth", value_name: "N", help: "Create a shallow clone truncated to N commits", .. },
        OptSpec {
            long: "filter",
            value_name: "SPEC",
            help: "Partial clone filter (e.g. blob:none for instant treeless/blobless clone)",
            ..
        },
    ],
    positionals: &[
        PositionalSpec {
            name: "URL",
            required: true,
            help: "Remote URL (https://github.com/org/repo, git@github.com:org/repo, github:org/repo, http://...)",
            ..
        },
        PositionalSpec {
            name: "DEST_DIR",
            help: "Destination directory (defaults to repository name)",
            ..
        },
    ],
    ..
};

const PUSH_SPEC: CommandSpec<'static> = CommandSpec {
    name: "push",
    about: "Push a local ref to a remote Smart HTTP `receive-pack` endpoint",
    positionals: &[
        PositionalSpec { name: "REMOTE_OR_URL", help: "Remote name or HTTP URL (default: origin)", .. },
        PositionalSpec { name: "REF_OR_DIR", help: "Branch/ref to push (or DIR when 3 args given)", .. },
        PositionalSpec { name: "REF", help: "Ref to push (when URL + DIR + REF are specified)", .. },
    ],
    ..
};

const BENCH_SPEC: CommandSpec<'static> = CommandSpec {
    name: "bench",
    about: "Run head-to-head .bgit vs git floor benchmarks on a repository",
    positionals: &[PositionalSpec { name: "DIR", help: "Repository directory (default: .)", .. }],
    ..
};

/// Top-level `bgit` CLI specification (ordered by everyday Git workflow frequency).
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
            name: "diff",
            aliases: &["df"],
            about: "Show changes between working tree, index, and commits",
            spec: &DIFF_SPEC,
            ..
        },
        SubcommandSpec {
            name: "add",
            about: "Stage file contents into the index",
            spec: &ADD_SPEC,
            ..
        },
        SubcommandSpec {
            name: "commit",
            aliases: &["ci"],
            about: "Record staged changes to the repository",
            spec: &COMMIT_SPEC,
            ..
        },
        SubcommandSpec {
            name: "branch",
            aliases: &["br"],
            about: "List, create, rename, or delete branches",
            spec: &BRANCH_SPEC,
            ..
        },
        SubcommandSpec {
            name: "checkout",
            aliases: &["co"],
            about: "Switch branches or restore working tree files",
            spec: &CHECKOUT_SPEC,
            ..
        },
        SubcommandSpec {
            name: "switch",
            aliases: &["sw"],
            about: "Switch branches cleanly",
            spec: &SWITCH_SPEC,
            ..
        },
        SubcommandSpec { name: "log", about: "Walk commit history", spec: &LOG_SPEC, .. },
        SubcommandSpec {
            name: "show",
            about: "Show commit metadata + diff or `<rev>:<path>` blob",
            spec: &SHOW_SPEC,
            ..
        },
        SubcommandSpec {
            name: "merge",
            about: "Run ORT 3-way merge and auto-update tracking worktrees",
            spec: &MERGE_SPEC,
            ..
        },
        SubcommandSpec {
            name: "reset",
            about: "Reset HEAD, index, or working tree to a revision",
            spec: &RESET_SPEC,
            ..
        },
        SubcommandSpec {
            name: "restore",
            about: "Restore working tree or staged files",
            spec: &RESTORE_SPEC,
            ..
        },
        SubcommandSpec {
            name: "stash",
            about: "Stash changes in a dirty working directory",
            spec: &STASH_SPEC,
            ..
        },
        SubcommandSpec {
            name: "tag",
            about: "List, create, or delete tags",
            spec: &TAG_SPEC,
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
            name: "remote",
            about: "Manage tracked remote repositories",
            spec: &REMOTE_SPEC,
            ..
        },
        SubcommandSpec {
            name: "fetch",
            about: "Fetch objects and refs via Git Protocol v2",
            spec: &FETCH_SPEC,
            ..
        },
        SubcommandSpec {
            name: "pull",
            about: "Fetch and merge from a remote repository",
            spec: &PULL_SPEC,
            ..
        },
        SubcommandSpec { name: "push", about: "Push a ref via receive-pack", spec: &PUSH_SPEC, .. },
        SubcommandSpec {
            name: "clone",
            about: "Clone a remote repo (or --vfs lazily)",
            spec: &CLONE_SPEC,
            ..
        },
        SubcommandSpec {
            name: "cherry-pick",
            about: "Apply changes from an existing commit via ORT merge",
            spec: &CHERRY_PICK_SPEC,
            ..
        },
        SubcommandSpec {
            name: "revert",
            about: "Create a commit reversing an existing commit",
            spec: &REVERT_SPEC,
            ..
        },
        SubcommandSpec {
            name: "rm",
            about: "Remove files from working tree and index",
            spec: &RM_SPEC,
            ..
        },
        SubcommandSpec {
            name: "mv",
            about: "Move or rename a file in working tree and index",
            spec: &MV_SPEC,
            ..
        },
        SubcommandSpec {
            name: "clean",
            about: "Remove untracked files from working tree",
            spec: &CLEAN_SPEC,
            ..
        },
        SubcommandSpec {
            name: "config",
            about: "Get, set, unset, or list repository/global config",
            spec: &CONFIG_SPEC,
            ..
        },
        SubcommandSpec {
            name: "rev-parse",
            about: "Resolve revisions, HEAD branch, or repo paths",
            spec: &REV_PARSE_SPEC,
            ..
        },
        SubcommandSpec {
            name: "show-ref",
            about: "List references in the local repository",
            spec: &SHOW_REF_SPEC,
            ..
        },
        SubcommandSpec {
            name: "merge-base",
            about: "Find LCA via CGPH commit-graph",
            spec: &MERGE_BASE_SPEC,
            ..
        },
        SubcommandSpec { name: "init", about: "Initialize an empty .bgit repo", spec: &INIT_SPEC, .. },
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
            name: "bench",
            about: "Head-to-head .bgit vs git benchmark",
            spec: &BENCH_SPEC,
            ..
        },
    ],
    ..
};
