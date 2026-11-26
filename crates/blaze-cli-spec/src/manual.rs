//! Sans-IO curated manual completion specs for popular CLIs & tools with non-standard help.

use blaze_own::{Alloc, String, Vec};

use crate::{CliSpec, FlagSpec, SpecSource, SubcommandSpec};

type FlagRow = (&'static str, &'static str, &'static str, &'static str);
type SubRow = (&'static str, &'static str);

const CLAUDE_FLAGS: &[FlagRow] = &[
    ("--dangerously-skip-permissions", "", "", "Bypass all permission checks (sandboxed use)"),
    ("--allow-dangerously-skip-permissions", "", "", "Enable bypassing permissions as an option"),
    ("--print", "-p", "", "Print response and exit (non-interactive)"),
    ("--continue", "-c", "", "Continue the most recent conversation in cwd"),
    ("--resume", "-r", "<session-id>", "Resume a conversation by session ID"),
    ("--model", "", "<model>", "Model for the current session (sonnet, opus, haiku)"),
    ("--effort", "", "<level>", "Effort level (low, medium, high, max)"),
    ("--add-dir", "", "<dirs...>", "Additional directories Claude can access"),
    ("--allowed-tools", "", "<tools...>", "Comma/space-separated allowed tool names"),
    ("--disallowed-tools", "", "<tools...>", "Comma/space-separated disallowed tool names"),
    ("--append-system-prompt", "", "<prompt>", "Append text to the default system prompt"),
    ("--system-prompt", "", "<prompt>", "Replace the system prompt"),
    ("--output-format", "", "<fmt>", "Output format (text, json, stream-json)"),
    ("--input-format", "", "<fmt>", "Input format (text, stream-json)"),
    ("--permission-mode", "", "<mode>", "Permission mode (default, plan, acceptEdits, bypassPermissions)"),
    ("--mcp-config", "", "<configs...>", "Load MCP servers from JSON files or strings"),
    ("--verbose", "", "", "Override verbose mode setting"),
    ("--debug", "-d", "<filter>", "Enable debug mode with optional category filter"),
    ("--bare", "", "", "Minimal mode: skip hooks, LSP, plugins, auto-memory"),
    ("--bg", "", "", "Start the session in the background"),
    ("--worktree", "-w", "<name>", "Create a new git worktree for this session"),
    ("--version", "-v", "", "Output the version number"),
    ("--help", "-h", "", "Display help for command"),
];

const CLAUDE_SUBS: &[SubRow] = &[
    ("mcp", "Configure and manage MCP servers"),
    ("doctor", "Check health of Claude Code installation"),
    ("update", "Check for updates and install if available"),
    ("config", "Manage configuration settings"),
    ("agents", "List background agents"),
    ("attach", "Attach to a background session"),
    ("logs", "Show logs for a background session"),
    ("stop", "Stop a background session"),
];

const GIT_FLAGS: &[FlagRow] = &[
    ("--version", "-v", "", "Print git version"),
    ("--help", "-h", "", "Display help"),
    ("", "-C", "<path>", "Run as if git was started in <path>"),
    ("--no-pager", "-P", "", "Do not pipe output into a pager"),
];

const GIT_SUBS: &[SubRow] = &[
    ("status", "Show working tree status"),
    ("diff", "Show changes between commits, index, and working tree"),
    ("add", "Add file contents to the index"),
    ("commit", "Record changes to the repository"),
    ("push", "Update remote refs along with associated objects"),
    ("pull", "Fetch from and integrate with another repository"),
    ("fetch", "Download objects and refs from another repository"),
    ("log", "Show commit logs"),
    ("switch", "Switch branches"),
    ("checkout", "Switch branches or restore working tree files"),
    ("branch", "List, create, or delete branches"),
    ("stash", "Stash changes in a dirty working directory"),
    ("rebase", "Reapply commits on top of another base tip"),
    ("merge", "Join two or more development histories together"),
    ("reset", "Reset current HEAD to the specified state"),
    ("restore", "Restore working tree files"),
    ("cherry-pick", "Apply changes introduced by existing commits"),
    ("worktree", "Manage multiple working trees"),
    ("show", "Show various types of objects"),
    ("blame", "Show what revision and author last modified each line"),
    ("bisect", "Use binary search to find the commit that introduced a bug"),
    ("clone", "Clone a repository into a new directory"),
];

const GIT_COMMIT_FLAGS: &[FlagRow] = &[
    ("--message", "-m", "<msg>", "Use the given <msg> as the commit message"),
    ("--amend", "", "", "Replace the tip of the current branch"),
    ("--all", "-a", "", "Automatically stage modified and deleted files"),
    ("--no-edit", "", "", "Use selected commit message without launching editor"),
    ("--no-verify", "-n", "", "Bypass pre-commit and commit-msg hooks"),
    ("--signoff", "-s", "", "Add Signed-off-by trailer"),
    ("--allow-empty", "", "", "Allow creating a commit with no changes"),
    ("--fixup", "", "<commit>", "Construct a fixup! commit for autosquash"),
];

const GIT_DIFF_FLAGS: &[FlagRow] = &[
    ("--staged", "", "", "Show changes staged for the next commit"),
    ("--cached", "", "", "Synonym for --staged"),
    ("--stat", "", "", "Generate a diffstat summary"),
    ("--numstat", "", "", "Machine-friendly added/deleted line counts"),
    ("--name-only", "", "", "Show only names of changed files"),
    ("--name-status", "", "", "Show names and status of changed files"),
    ("--word-diff", "", "", "Show a word diff instead of line diff"),
    ("--ignore-all-space", "-w", "", "Ignore whitespace when comparing lines"),
];

const GIT_LOG_FLAGS: &[FlagRow] = &[
    ("--oneline", "", "", "Compact one-line commit format"),
    ("--graph", "", "", "Draw text-based graphical commit history"),
    ("--stat", "", "", "Show diffstat for each commit"),
    ("--patch", "-p", "", "Show patch diff for each commit"),
    ("--all", "", "", "Show commits across all refs"),
    ("--decorate", "", "", "Print ref names of commits"),
    ("--max-count", "-n", "<num>", "Limit the number of commits to output"),
    ("--author", "", "<pattern>", "Limit commits by author"),
    ("--grep", "", "<pattern>", "Limit commits matching message pattern"),
];

const GIT_PUSH_FLAGS: &[FlagRow] = &[
    ("--set-upstream", "-u", "", "Set upstream tracking reference"),
    ("--force-with-lease", "", "", "Safer force push verifying remote ref"),
    ("--force", "-f", "", "Force update remote refs"),
    ("--tags", "", "", "Push all tags under refs/tags"),
    ("--delete", "-d", "", "Delete all listed refs from the remote"),
    ("--dry-run", "-n", "", "Do everything except actually send updates"),
];

const GIT_STASH_FLAGS: &[FlagRow] = &[
    ("--include-untracked", "-u", "", "Include untracked files in stash"),
    ("--keep-index", "-k", "", "Leave staged changes intact in the index"),
    ("--message", "-m", "<msg>", "Attach a description to the stash"),
    ("--patch", "-p", "", "Interactively select hunks to stash"),
];

const GIT_STASH_SUBS: &[SubRow] = &[
    ("push", "Save local modifications to a new stash entry"),
    ("pop", "Remove a single stash state and apply it"),
    ("apply", "Apply stash state without removing it"),
    ("list", "List the stash entries you currently have"),
    ("show", "Show the changes recorded in the stash entry"),
    ("drop", "Remove a single stash entry"),
    ("clear", "Remove all stash entries"),
];

const CARGO_FLAGS: &[FlagRow] = &[
    ("--offline", "", "", "Run without accessing the network"),
    ("--locked", "", "", "Assert that Cargo.lock will remain unchanged"),
    ("--release", "-r", "", "Build artifacts in release mode"),
    ("--workspace", "", "", "Operate on all workspace members"),
    ("--package", "-p", "<spec>", "Package to operate on"),
    ("--jobs", "-j", "<n>", "Number of parallel jobs"),
    ("--quiet", "-q", "", "Do not print cargo log messages"),
    ("--verbose", "-v", "", "Use verbose output"),
];

const CARGO_SUBS: &[SubRow] = &[
    ("build", "Compile the current package"),
    ("check", "Analyze the current package and report errors"),
    ("test", "Run the tests"),
    ("run", "Run a binary or example of the local package"),
    ("clippy", "Catch common mistakes with Clippy lints"),
    ("fmt", "Format all bin and lib files"),
    ("doc", "Build package documentation"),
    ("clean", "Remove the target directory"),
    ("update", "Update dependencies listed in Cargo.lock"),
    ("tree", "Display a tree visualization of dependency graph"),
];

const FD_FLAGS: &[FlagRow] = &[
    ("--hidden", "-H", "", "Search hidden files and directories"),
    ("--no-ignore", "-I", "", "Do not respect .(git|fd)ignore files"),
    ("--unrestricted", "-u", "", "Unrestricted search (include hidden & ignored)"),
    ("--type", "-t", "<type>", "Filter by type: f (file), d (dir), l (symlink), x (exec)"),
    ("--extension", "-e", "<ext>", "Filter by file extension"),
    ("--exclude", "-E", "<glob>", "Exclude entries matching the given glob"),
    ("--max-depth", "-d", "<depth>", "Set maximum search depth"),
    ("--full-path", "-p", "", "Search full abs/rel path instead of filename"),
    ("--fixed-strings", "-F", "", "Treat pattern as a literal string"),
    ("--glob", "-g", "", "Glob-based search instead of regex"),
    ("--exec", "-x", "<cmd>", "Execute a command for each search result"),
    ("--size", "-S", "<size>", "Limit results based on file size"),
];

const RG_FLAGS: &[FlagRow] = &[
    ("--ignore-case", "-i", "", "Search case insensitively"),
    ("--smart-case", "-S", "", "Smart case search"),
    ("--fixed-strings", "-F", "", "Treat pattern as a literal string"),
    ("--files-with-matches", "-l", "", "Print only paths with at least one match"),
    ("--files", "", "", "Print each file that would be searched"),
    ("--hidden", "-.", "", "Search hidden files and directories"),
    ("--no-ignore", "", "", "Don't respect .gitignore files"),
    ("--glob", "-g", "<glob>", "Include or exclude files matching glob"),
    ("--type", "-t", "<type>", "Only search files matching TYPE (e.g. rust, ts)"),
    ("--line-number", "-n", "", "Show line numbers"),
    ("--count", "-c", "", "Show count of matching lines per file"),
    ("--context", "-C", "<num>", "Show NUM lines before and after each match"),
    ("--after-context", "-A", "<num>", "Show NUM lines after each match"),
    ("--before-context", "-B", "<num>", "Show NUM lines before each match"),
    ("--multiline", "-U", "", "Enable matching across multiple lines"),
    ("--word-regexp", "-w", "", "Only show matches surrounded by word boundaries"),
];

const SSH_FLAGS: &[FlagRow] = &[
    ("", "-i", "<identity>", "Identity file (private key) for authentication"),
    ("", "-p", "<port>", "Port to connect to on the remote host"),
    ("", "-L", "<bind:host:port>", "Local port forwarding"),
    ("", "-R", "<bind:host:port>", "Remote port forwarding"),
    ("", "-D", "<port>", "Dynamic SOCKS application-level port forwarding"),
    ("", "-J", "<jump>", "Jump host(s) to proxy through"),
    ("", "-A", "", "Enable forwarding of the ssh-agent connection"),
    ("", "-N", "", "Do not execute a remote command (useful for tunnels)"),
    ("", "-t", "", "Force pseudo-terminal allocation"),
    ("", "-v", "", "Verbose mode (use -vvv for max debug)"),
    ("", "-o", "<option>", "Pass ssh_config option"),
];

const CURL_FLAGS: &[FlagRow] = &[
    ("--request", "-X", "<method>", "Specify HTTP request method (GET, POST, PUT, DELETE)"),
    ("--header", "-H", "<header>", "Pass custom header(s) to server"),
    ("--data", "-d", "<data>", "HTTP POST data"),
    ("--json", "", "<data>", "HTTP POST JSON with Content-Type/Accept headers"),
    ("--location", "-L", "", "Follow HTTP 3xx redirects"),
    ("--silent", "-s", "", "Silent mode (hide progress meter)"),
    ("--show-error", "-S", "", "Show error even when -s is used"),
    ("--fail", "-f", "", "Fail fast with no output on HTTP errors"),
    ("--include", "-i", "", "Include response headers in output"),
    ("--head", "-I", "", "Fetch HTTP headers only"),
    ("--output", "-o", "<file>", "Write output to <file> instead of stdout"),
    ("--verbose", "-v", "", "Make the operation more talkative"),
];

const VIM_FLAGS: &[FlagRow] = &[
    ("--clean", "", "", "Start with defaults, skipping user config/plugins"),
    ("--headless", "", "", "Start without UI (for scripting)"),
    ("", "-R", "", "Read-only mode"),
    ("", "-d", "", "Diff mode (compare two or more files)"),
    ("", "-O", "", "Open files in vertical splits"),
    ("", "-o", "", "Open files in horizontal splits"),
    ("", "-p", "", "Open files in tab pages"),
    ("", "-c", "<cmd>", "Execute Ex command after loading first file"),
    ("", "-u", "<vimrc>", "Use specified config file instead of default"),
];

/// Return a curated manual spec for `(cmd, subcmd)` allocated in `alloc`, if one exists.
pub fn manual_spec_in<'a>(cmd: &str, subcmd: Option<&str>, alloc: Alloc<'a>) -> Option<CliSpec<'a>> {
    if cmd == "blaze" {
        if let Some(sub) = subcmd {
            crate::app::find_command(&crate::blaze_app::BLAZE_APP, sub)?;
        }
        return Some(crate::app::to_cli_spec(&crate::blaze_app::BLAZE_APP, subcmd, alloc));
    }

    let (flag_rows, sub_rows): (&[FlagRow], &[SubRow]) = match (cmd, subcmd) {
        ("claude", None) => (CLAUDE_FLAGS, CLAUDE_SUBS),
        ("git", None) => (GIT_FLAGS, GIT_SUBS),
        ("git", Some("commit")) => (GIT_COMMIT_FLAGS, &[]),
        ("git", Some("diff")) => (GIT_DIFF_FLAGS, &[]),
        ("git", Some("log")) => (GIT_LOG_FLAGS, &[]),
        ("git", Some("push")) => (GIT_PUSH_FLAGS, &[]),
        ("git", Some("stash")) => (GIT_STASH_FLAGS, GIT_STASH_SUBS),
        ("cargo", None) => (CARGO_FLAGS, CARGO_SUBS),
        ("fd", None) => (FD_FLAGS, &[]),
        ("rg", None) => (RG_FLAGS, &[]),
        ("ssh", None) => (SSH_FLAGS, &[]),
        ("curl", None) => (CURL_FLAGS, &[]),
        ("vim" | "nvim", None) => (VIM_FLAGS, &[]),
        _ => return None,
    };

    let mut command = String::from_str_in(cmd, alloc);
    if let Some(s) = subcmd {
        command.push(' ');
        command.push_str(s);
    }
    let mut flags = Vec::new_in(alloc);
    for &(l, s, h, d) in flag_rows {
        flags.push(FlagSpec {
            long: (!l.is_empty()).then(|| String::from_str_in(l, alloc)),
            short: (!s.is_empty()).then(|| String::from_str_in(s, alloc)),
            value_hint: (!h.is_empty()).then(|| String::from_str_in(h, alloc)),
            description: String::from_str_in(d, alloc),
        });
    }
    let mut subcommands = Vec::new_in(alloc);
    for &(n, d) in sub_rows {
        subcommands.push(SubcommandSpec {
            name: String::from_str_in(n, alloc),
            description: String::from_str_in(d, alloc),
        });
    }
    Some(CliSpec {
        command,
        flags,
        subcommands,
        source: SpecSource::Manual,
    })
}
