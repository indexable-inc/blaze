---
status: superseded
summary: End state for the command line, output and the agent interface: one ordered stream of typed rows that the terminal, the pipe format, the agent API and why all fold; a registry of homes so every daemon is visible and stoppable. Not yet reconciled with the shared page.
date: 2026-11-12
superseded_by: v2/plan/design/target.md
---
# End state: what people and agents see and call

Tree read: `/Volumes/Projects/blaze-int-1112` (2026-11-12). Status words: **measured** (with where from), **read** (from code or a doc of the tree, this session), **memory** (a system cited from what I know of it, its source not opened this session), **design** (not run).

## 1. What this area must do, and the invariant

1. Take a request from a person or an agent (a few verbs) and say what blaze did about it: nothing, or which work, each piece with the cause that made it necessary.
2. Show a person the right thing at each moment: nothing in the first 100 ms of a request that ends in them; the cause and the amount of work as soon as a request misses; the frontier of a long build in a region that is erased; one line that stays.
3. Give an agent the same facts as typed data: stable, versioned, complete, with no prose to parse, and a way to tell that nothing was lost.
4. Answer "why was that rebuilt, slow or big" from what the build already recorded, with no query language and no second process.
5. Let a user see and end every blaze daemon and home on the machine, find every output, and set every setting in one place.

**The invariant (one account, many drawings):** everything blaze shows or returns about a request is a pure function of that request's one ordered stream of typed rows, and that stream is the one the home keeps. The terminal view, the pipe lines, cargo's JSON, the agent API, `why` and `debug sql` are folds of it. Two readers of one request never disagree; nothing is said that is not a row; no row holds drawn text.

How it fits `end-state.md`: a unit of work is a record found or made, so a row is a record's key with the verdict of its reads (`Held`, or `Moved(read, was, now)`). `why` prints a record and the read that moved. The idea needs one addition here: folds are never records (nothing drawn is cached or keyed), and what a certificate keeps to say again (`said`) is rows, not bytes.

## 2. How the best systems do it

- **Buck2: superconsole and the event log** (memory: the `superconsole` crate; `buck2 log what-ran`, `what-failed`, `critical-path`, `replay`). The daemon emits one stream of typed events; the console is a component tree redrawn in a region under lines that scroll, erased at the end; every after-the-fact question is a reader of the saved stream, and a saved stream replays into the console. **Take:** one stream, the region, questions as readers, replay as the test. **Not:** the log as a format of its own beside what the daemon keeps for itself, and BXL (a script language to ask the graph; the owner ruled out script languages for the ECS on 2026-11-11, and rule 32 says library before language).
- **Bazel's build event protocol** (memory: `build_event_stream.proto`). Every event announces the events that will follow from it, so a consumer knows when it has seen everything. **Take:** completeness is checkable: the graph is announced before it moves, and the last row carries counts the reader's own fold must equal. **Not:** a second transport and a second graph vocabulary for it.
- **nix-output-monitor** (memory). Roots are the requested targets; the view is the forest of what runs under them, a totals row (running, done, planned), and time left estimated from earlier builds of the same thing; inline, not an alternate screen. **Take:** roots, totals, estimates from kept records. **Not:** it scrapes another program's log lines; and its tree does not fit (docs/terminal.md, read: nom's tree took 56 rows for ripgrep's 12 running units, the frontier 8).
- **git and jj, cargo, uv and ruff** (memory). Porcelain for people may change; plumbing is stable and typed (`git status --porcelain=v2`, `jj --template`). cargo keeps stdout for machine messages and the program, stderr for people. ruff has one `--output-format`; uv ends with one line that has a count and a time. **Take:** one format flag; stdout is never decorated; the pipe format is plumbing with a written stability rule. **Not:** cargo's `rendered` strings as the content, or `CARGO_*`-style settings by environment.
- **Postgres `pg_ctl stop`** (memory). Stop is a signal to the pid in a file the server holds, then a wait: it works on any version and on a server that answers nothing. **Take:** stop needs no protocol.
- **rust-analyzer's LSP** (memory). Request ids, cancellation, a versioned overlay of unsaved text. **Take:** those three. **Not:** JSON-RPC and a capability matrix: one schema instead.
- **Relax** (read: `compiler/src/lib.rs:548-626`, `run.rs:1012-1022`, `serve_rpc.rs:1`). Verbs `run`, `check`, `test`, `clean`; agent questions are plain subcommands (`schema`, `hover`, `definition`); `--agent` turns on events on stderr. **Take:** no daemon needed to ask. **Not:** JSON lines over loopback TCP (any local user can connect; rule 33 says a Unix socket).

## 3. blaze today (read from code unless marked)

**Shape.** `cli/args.rs` `parse` (`:780`) reads 33 command words (`Asked`, `:426`; `blaze_units::query::Command`), and `cli/observe.rs` `debug` (`:125`) 11 more. A build's answer is `blaze_api::Response` frames; `cli/print.rs` `Printer::on` (`:427`) turns each into a `blaze_screen::Event`; `blaze-screen` (`Screen::take`, sans-IO) makes a `Frame` for a terminal or `blaze: key=value` lines for a pipe (`screen.rs:1047-1089`); `cli/show.rs` writes bytes. `why` asks the daemon's ledger (`observe.rs:396` `rebuilt`); `slow` and `explain VIEW` run DuckDB over Parquet in the client (`observe.rs:567`, `:1320` `run`, `duckdb.rs`). Much of what the owner asked for on 2026-11-12 is already in the screen's model: typed `Text` with roles, the palette, OSC 8 and OSC 9;4, `~` paths, `Built::size_bytes`, `store_bytes`, `store_delta_bytes`, `Ran` (`blaze-screen/src/event.rs:62-110`, `blaze-report`).

**Smells under rule 66.**
- *Several mechanisms for one job: a build is described six times.* `blaze_api::Response` (`blaze-api/src/lib.rs:793`, 42 variants, `SCHEMA = 53` at `:110`); `blaze_protocol::events::Event` (`events.rs:27`, `SCHEMA = 2` at `lib.rs:45`, whose `Message` nothing outside its own tests sends); `blaze_screen::Event` (`event.rs:34`); `blaze_protocol::now::Now` (`now.rs:70`); cargo's JSON with blaze's own reasons added (`cli/json.rs:309`, `:358`); the profile's 29 tables (docs/profiling.md, by heading). Cause has two types: `events::Cause` (`events.rs:651`) and `blaze_api::RebuildCause` with `Reason` (`lib.rs:574`, `:634`). Each new fact (size, storage, why) was added to three of them.
- *Drawn text inside events.* `Event::Said(String)`: "whole lines, as they were drawn for this client" (`blaze-screen/src/event.rs:43`); `Waits(Option<String>)` "in the daemon's words" (`:47`); `Built::tier`, `Link::how` are sentences. The daemon draws for a terminal it cannot see. 5,129 messages still format code between backticks by hand in 99 crates (docs/terminal.md; `blaze-report/tests/backticks.txt`); about 320 std print macros remain in 40 crates (`tests/prints.txt`), and the thin client reports a closed stderr as a failure (`blaze-thin/src/main.rs:272` `wrote`).
- *The answer is reconstructed, by a second process.* The build's own "why" is a second round trip when the history thread was not done (`observe.rs:499` `summary_from`, `:522` `recomputed`). `Unexplained` has ten sub-cases (docs/v2/plan/design/why.md, model point 5). `slow` needs a DuckDB fetched by hash on first use and a flush asked of the daemon (`observe.rs:9-12`, `:1339` `history_source`). 53 named queries live in `blaze-profile/sql/examples.sql`. Measured (owner, brief): "why is it slow" has needed hand-written SQL each time; an unexplained recompute named nothing until an event was added.
- *A special-case list for who answers.* `blaze-thin/src/route.rs:3-9` reads only `build` and `run` with `-q` and `-r`; everything else execs the engine (`handover.rs`, `cli/resume.rs`).
- *A per-process key: the daemon's environment.* `serve/settings.rs:32` `STARTED_WITH` lists variables the daemon read at start; a client whose environment differs is refused (`cli/daemon.rs:128` `DaemonError::Drifted`, `:378` `this_build`: a `Status` round trip and a second connect per command). 91 `BLAZE_*` variables (docs/v2/plan/design/caches.md: 11 in the switches table, 80 listed after it); the API token is one of them (`serve.rs:1140`) and so is visible to every child.
- *A dead end stated as an error.* `cli/daemon.rs:1331` `other_schema`: "it is another build of blaze, which this one can ask nothing and can't tell to stop". A daemon is found only by its home's socket (`serve.rs:117`) and `serve.lock`; nothing lists homes. Measured (owner's Mac, brief): eight daemons of different builds and homes at once, seen by nobody.
- *`run` cannot say how the program ended.* `cli/run.rs:262` `exec` replaces the client, so `Event::Ran` is never sent (docs/terminal.md, "Not built").
- *Work nobody reads.* Lints of packages the user does not own are computed, then counted (`Built::dependency_warnings`); measured (owner, brief): 36 dependency warnings printed on every piped run before they were counted.
- *Numbers.* Hello-world no-op 50 ms only from the fifth run, 160-700 ms before; a first `run` of 11 ms the owner expected under 1 ms (measured, brief). No number exists for how long `why`, `slow` or `status` take, or for the bytes of events a build sends; step 0 below takes them.

## 4. The end state

### 4.1 One stream of rows
One crate, `blaze-events` (`no_std`, no IO, no compiler), holds the only description of a request. A row is `(request, seq, at_ns, body)`, fixed width, with names in a side table (as `daemon.md` 4.6 has the profile's rows). Bodies:

- `Asked{shape, checkout, blaze id}`, then `Verdict{Held | Moved{read, was, now, readers}}`: the certificate's answer and, on a miss, its named input.
- `Nodes{..}`: the work under the miss (kind, name, edges, and each node's time when it was last made, from its record), said before any of it moves.
- `Began{node, step, cause}` and `Ended{node, step, how: Held | Made | Error | Panic | Cancelled, wall_ns, cpu_ns, wrote_bytes, peak_bytes}`. `cause` is `daemon.md` 4.6's `Cause` (`First | Moved(read) | Evicted(by) | Forced | Left`); `Began` has no constructor without one.
- `Waits{node, what, holder}`; `Memory{footprint, ceiling, top owners}` at start, at peak and at the end (rule 50).
- `Diag{node, owner: Workspace | Dependency, level, code, message, spans, the source lines they point into, fixes}`; `Report` for blaze's own words; `Output{path under the out directory, key, bytes on disk, kind}`; `Test{..}`; `Ran{code, signal, wall, cpu, peak}`.
- `Account{..}` (4.4) and `End{ok, rows}`.

Rules of the stream. A message is a `blaze_report::Text` (spans with roles: code, command, path, package, count, time, hash); a `String` is only a name or a user program's own output. The daemon appends rows to the home's log (the profile's segments) and sends a subscriber the same bytes; a subscriber that falls behind reads the segment from disk at its offset, so the daemon keeps one bounded ring a subscriber and never waits for one (rule 18). `Follow{grain: Unit | Item}`: at unit grain the items of a unit are sums in its `Ended`. `End.rows` and `Account`'s counts must equal the reader's own fold; a reader that finds they do not says "rows lost" and fails (rule 3).

### 4.2 Folds
Each is a sans-IO machine `take(&Row)`, tested by replaying the same recorded logs (rule 14, rule 25: same rows, same bytes):

| Fold | For | Output |
|---|---|---|
| `Screen` | a terminal | `Frame`s (as `blaze-screen` today), encoded for the terminal's capabilities |
| `Lines` | a pipe, the default off a terminal | `blaze: <word> key=value ..` lines |
| `Rows` | `--format events`, the socket, HTTP | the rows themselves: CBOR on a socket, a JSON object a line on stdout, first line `{"schema":N,"blaze":..}` |
| `CargoJson` | `--message-format=json` | cargo's messages exactly, for the tools that read them (rule 5); nothing of blaze's added |
| `Why` | `blaze why` | `Report`s |

One flag picks: `--format human | lines | events`, default by whether stderr is a terminal; `--message-format` is cargo's spelling of the fourth. stdout carries only the program's output, cargo's JSON or `events`. Every write goes through `blaze_report::out`; a closed reader ends the command with status 141 and no words, in every program.

### 4.3 What a person sees
- **First 100 ms.** Nothing is drawn. A request that ends in that time prints its end only: for `build` and `check`, one dim line (`up to date · 6 ms · ~/.cache/blaze/build/hello-1a2b/debug/hello · 412 KB`); for `run` and `test`, nothing, then the program.
- **On a miss**, the region opens with the cause as its first row, from `Verdict` and `Nodes`: `src/main.rs changed · 1 function, 1 link · about 0.7 s`. A build that does work always says why before it says what; this is the answer to "why did `run` compile this time".
- **A long build.** The region, inline, a third of the rows, erased row by row at the end, never an alternate screen: one row a requested root with its closure's done of total; the frontier (docs/terminal.md's rows: stage, time, how many wait on it), each with its chain to its root folded into the row (`▶ syn · check 6.4s ← serde_derive ← 12 more ← reth (bin)`); a graph strip, one row for the whole graph by depth, each cell a layer coloured by done, running, waiting, with the critical path named; why something waits, only while it does; the status row with the time left from the nodes' last times, shown only when every running node has one. On a terminal of 40 rows or more the strip becomes the layers, a row each. OSC 9;4 progress, OSC 8 links on paths and codes, synchronized output: as detected today.
- **The end.** One line stays: the verdict; what was made at the grain that fits (functions for an edit, units for more) and what was held; the time; the cause; the output with `~`, its size on disk; the store's total and what this build added. A hint names `blaze why` only when the account alarms (work with no change, work out of proportion, a wait over a tenth of the wall time).
- **`run`.** The client starts the program as a child in the terminal's foreground group, waits (`wait4`; macOS `proc_pid_rusage`), exits with its status, and writes `Ran`. Footer as docs/terminal.md has it: always after a failure, a signal or a second of running; `--stats` adds `build 0.4 ms (held) · start 3 ms · program 7 ms`, the split the owner asked about.
- **Lints of code the user does not own are not asked for** (cargo's cap, as a rule of demand): a dependency's lint check is no node of the request. `-p dep` makes it owned for that request.

### 4.4 The account and `why`
`Account` is the last row before `End`, made by the daemon's own fold as rows pass (not by a later thread): what changed (inputs by kind, each named up to a bound, then a count); work by step and by cause; held; wall time split by step and by wait class, with the critical path; footprint at start, peak and end with the top four owners at peak; bytes written by kind, the store's total against its budget, each output with its size. A hit writes `Asked`, `Verdict::Held`, `Account`, `End` and nothing else.

`blaze why [ITEM] [--time | --memory | --disk] [--build N] [--format ..]` reads the log from the home's files, with no daemon and no DuckDB:
- no argument: the account in about eight lines (changed, redone because, time, memory, disk), each line a sum of rows and each naming the command that opens it;
- `ITEM` (a function, a unit, a file, an output path): its record: the key, each read with `was` and `now`, the one that moved, and the chain of `Moved` up to an input; or "held since build N: nothing it read moved"; for an output, its bytes by crate and section;
- `--time`: `slow`'s findings (serial, starved, wait, queued, redone), computed in Rust from the rows; `--memory`, `--disk`: the holders, largest first.
Each `requests` row names the blaze that ran it, and `why` says so when it is not the one asking.

### 4.5 Commands

| Kept | Why it earns its place |
|---|---|
| `build`, `check`, `run`, `test` (`bench`, `doc`, `fmt`; `check --fix`) | the verbs; cargo's words where people's hands and tools know them |
| `new` (`init` is `new .`) | a project in one command |
| `why` | the one question command (4.4) |
| `explain RULE` | a rule's or an error code's long text, and nothing else |
| `status` | every home and daemon of this user on the machine, this checkout's out directory and sizes, and every setting with where its value came from |
| `stop [HOME or PID] [--all] [--now]` | 4.6 |
| `clean [--all]` | drops this checkout's placed outputs and its root; the collector does the rest |
| `store` (`gc`, `budget`, `export`, `import`, `verify`) | the disk area's verbs, one noun |
| `daemon install` and `uninstall` | the service units; the only use of the word |
| `metadata`, `tree` | cargo's, read by tools (rust-analyzer) |
| `debug sql [VIEW or QUERY]`, `debug log [BUILD] [--follow]`, `debug schema` | plumbing, said to be unstable: Parquet for a question nobody foresaw; the raw rows; the schema as CDDL |

### 4.6 Daemons, homes and versions
- **Registry.** One fixed directory a user, not under any home (`$XDG_STATE_HOME/blaze/homes`; macOS `~/Library/Application Support/blaze/homes`). A daemon holds an exclusive lock on its entry for its life: `{home, socket, pid, start instant, blaze id, schema}`. An entry whose lock is free is a dead daemon's and is removed by whoever reads it. `status` reads the directory and sends each socket `Hello`; it prints one table (home with `~`, pid, blaze, up since, footprint of ceiling, store of budget, requests, checkouts watched).
- **Stop is a signal.** `stop` sends SIGTERM to the entry's pid (checked against the held lock), then waits on the lock with a deadline and a span naming it; `--now` sends SIGKILL. No message is involved, so any blaze stops any daemon, and one that answers nothing. It is sound because of `daemon.md`'s invariant: killing a daemon at any instant changes cost, never an answer. `Stop` leaves the schema.
- **One schema, one rule.** `Message{schema, id, body}` in `blaze-protocol`, deterministic CBOR, the socket checked by peer uid; HTTP carries the same bytes with a token read from a 0600 file in the home. Keys are only added; a reader skips unknown keys in rows and refuses a request with an unknown key, naming it; `schema` moves only when a meaning changes. `Hello{blaze id, schema, home, pid}` is frozen: the tree keeps the bytes of every released `Hello` and the gate decodes them. A client sends work only to a daemon whose blaze id equals its own; otherwise it starts its own engine and the old daemon hands the socket over (`daemon.md` 4.5). Readers (`why`, `status`, `debug log`) read files and need no equal version: a segment's header names its layout, and a layout this blaze cannot read is an error naming the blaze that wrote it.
- **Requests** (the 36 of today become): `Hello`, `Since`, `Build{shape, overlay, follow}`, `Cancel`, `Follow{request or home, from, grain}`, `Status`. An agent's unsaved text is `overlay`. Everything else today is a field of `Build`, a fold of the log, or a `store` verb.

### 4.7 Outputs and settings
- **Where outputs are.** In the home, `build/<checkout id>/` (`disk.md` 4.1), placed by clone. The checkout gets one entry: `.blaze/out`, a symbolic link to that directory, which is also the collector's root for the checkout (Nix's `result` link, memory). So `./.blaze/out/debug/hello` is stable, the end line shows the real path, `Output` rows carry it, and deleting the checkout frees its outputs. `./target` exists only where the project says so (`--target-dir`, `build.target-dir`), for tools that need it (rule 5).
- **Settings.** One frozen type `Settings` in `blaze-settings`, the only module allowed to read the environment (a `.clippy.toml` ban elsewhere, rule 13). Sources, in order: defaults, `<home>/config.toml`, `<checkout>/.blaze/config.toml`, one `--config '<json>'`, and one variable `BLAZE_CONFIG` holding the same JSON value (rule 42). An unknown key is an error naming it. Each setting has one class, and the class says where it lives: **request** (anything that changes an answer: backend, optimisation, sandbox, verify modes) is part of the request shape and so of the certificate's key (rule 21); **home** (ceiling, cores, profile size, huge pages, tool paths) is read by the daemon from the home's file, which it watches; **client** (colour, background, notify, footer) is `[terminal]`. The daemon reads no request setting from its own environment, so `Drifted` cannot be built.
- **The 91 variables** (classes by name; each is checked when ported): 2 survive as variables (`BLAZE_HOME`, because it says where the settings are, and `BLAZE_CONFIG`); about 17 become request settings (`BLAZE_BACKEND`, `OPT`, `MIR_OPT`, `INLINE*`, `REGALLOC`, `SANDBOX`, `DEPS`, the six verify switches as one `verify` list); about 22 become home settings (`CORES`, `DAEMON_MEMORY_GB`, `MEMORY_BUDGET`, `HUGE_PAGES`, `PROFILE_*`, `TOOLCHAINS`, `RUST_DIST`, `LIBRARY*`); 2 become client settings and flags (`UI`, `UI_TRANSCRIPT`, which is `debug log`); about 13 dumps and traces become `debug` fields of a request or subscriptions to rows (`DUMP_*`, `ALLOC_TRACE`, `FN_CACHE_TRACE`, `TRACE_HOLDERS`, `STATS`, `PROFILE`); about 15 test and harness knobs become values a test gives in process (`FUZZ_RUNS`, `ORACLE_*`, `REVISIONS_*`, the planted-bug switches); 6 build stamps are compile-time (`GIT_SHA`, `VERSION*`, `PINNED_BY`, `RELEASE`); about 6 parent-to-child marks become an argument or an inherited descriptor (`LAUNCHED`, `INPROC`, `WARM_SOCKET`); `API_TOKEN` becomes a file; the rest go with what they select (`DEF_MAP_PAGES`, `ITEM_STORE`, `RESTORE_WITHOUT`, `SCRIPT_RUSTC`, `DUCKDB` outside `debug sql`).
- **Errors.** `Report::new(rule: Rule, about: Subject, headline: Text)` is the only constructor: no report without the rule that refused and the item it is about (rule 19). `Rule` is an enum with its long text (`explain`). `ApiError{what: String}` and `anyhow` leave the client's path; an error nobody classified is `Rule::Bug` with its site, said as a blaze bug (rule 3). A panic in a request becomes that request's `Report` (rule 20).
- **Documentation.** `docs/usage.md` (commands, the view, the lines format with its stability rule: words and keys are only added), `docs/api.md` (generated from `blaze-events` and `blaze-protocol`, with a golden CDDL the gate compares), `docs/settings.md` (generated from `Settings`: key, class, default). Three current docs; `terminal.md`, `client.md`, `structured-output.md` and `why.md` fold into them.

Rules met: 3 (lost rows, unknown keys and unclassified errors stop loudly), 11 (rows fixed width in rings; folds allocate per frame), 14 (every fold and the settings merge are sans-IO), 19 (by constructor), 21 (request settings in the shape), 25 (replay gives the same bytes), 33 (one schema, socket or HTTP), 42 (one JSON value), 43 (the registry's lock and stop, and the lagging subscriber, under simulation: an oracle that a stopped home has no holder and that a reader's fold equals the daemon's `Account`), 50 (`Memory` rows are footprint), 51 (the log is the store's segments), 65 (a row a unit of work, never a crate's total alone).

## 5. What is deleted, and what survives

**Deleted:** `blaze_api::Response`'s build variants, `Envelope`, `SCHEMA = 53`, `ApiError`, `RebuildCause`, `Reason`, `Request::{Stop, Rebuilt, Status's settings, Flush, ClientRows, Tables, Trace, CodegenInputs, Hot, Memory, Live, Profile, Explore}`; `blaze_protocol::events::{Event, Cause}` and `now::Now` as wire types; `blaze_screen::Event::{Said, Waits, Now, Why}` and `Built`'s sentences; `cli/print.rs`'s translation, `cli/resume.rs`, `blaze-thin/src/{route.rs, handover.rs}`; `serve/settings.rs` and `DaemonError::Drifted`, `other_schema`, `cli/which.rs`'s warning; `observe.rs`'s `summary_from`, `recomputed`, `slowed`, `explained`, the 53 queries as the answer path (they stay as `debug sql` views), DuckDB anywhere but `debug sql`; the words `slow`, `cache`, `gc`, `explore`, `reach`, `doctor`, `pprof`, `perf`, `fetch-url`, `toolchain`, `codegen-inputs`, `trace`, `tables`, `lock`, and `debug`'s other nine (each becomes a view, a `status` section, a `store` verb or a `run` flag); `dependency_warnings`; 89 environment variables.

**Survives:** `blaze-screen`'s core, frames, encoder and golden transcripts; `blaze-report`'s `Text`, `say!`, palette, terminal detection, `out`; the `blaze: ok`, `failed` and `progress` lines and their keys; cargo's JSON; the ledger's parts and chains as the content of `Moved`; the account's columns; `slow`'s findings; the profile's Parquet and views for `debug sql`; `blaze_protocol`'s CBOR and frame.

## 6. Order of building

Step 0 (hours): on a named commit, time `blaze why`, `slow` and `status` on reth's last build; count rows and bytes a reth body edit and a cold build would send at unit and at item grain (from `spans` and `query_counts`); list which keys of the `blaze:` lines `harness/v2/integrate.py` reads.

| Step | What | Judged by (measured how) |
|---|---|---|
| 1 | The registry, stop by signal, `status` over every home; `other_schema` deleted; simulation of crash, stale entry, two starters | a test starts 3 daemons of 2 blaze builds in 2 homes: `status` lists 3 of 3 and `stop --all` leaves 0, from either build (today: 1 seen, 0 stoppable across builds); on the owner's Mac, 8 of 8 |
| 2 | `blaze-events` compiled alone; `Screen`, `Lines`, `CargoJson` ported onto it; the daemon emits rows; the five other vocabularies deleted | every golden transcript replays to the same bytes; vocabularies 6 to 1; drawn strings in rows 0 (a lint count) |
| 3 | `Account` made in line; `Why` in Rust from the log; `slow` and the second round trip deleted | `blaze why` after a reth body edit: under 50 ms, 0 child processes, 0 daemon requests; the owner's standing questions (a list of ten kept as a test) each answered by a `why` form with 0 SQL; unexplained 0 on reth and tempo |
| 4 | The client is whole (with `daemon.md` step 3): `run` waits and writes `Ran`; one writer | `blaze --version | head -1` and every command into a closed pipe: status 141, empty stderr; `prints.txt` total 0; `run` of `exit 3` shows the footer and exits 3 |
| 5 | `blaze-settings`, the classes, the lint ban; request settings in the shape | `BLAZE_` names read outside `blaze-settings`: 91 to 2 (the `domains` script); two clients with different request settings share one daemon, each a hit on its second run |
| 6 | The command cut; `Report::new` as the only constructor; lints of unowned code not asked | command words 33 to 18 and `debug` 11 to 3; `backticks.txt` total 5,129 to 0; reth `check`: 0 lint nodes for registry packages |
| 7 | The live view's first row, roots, chains, graph strip and time left | on recorded reth and tempo logs at 80x24 and 200x60: the region within its rows, the cause visible in the first frame; time left within 25% at the halfway row on a warm rebuild |

## 7. Unsound, unknown, or to be measured first

1. **How many rows is a build?** tempo has 7,909 units and 4.2 M bodies. Answer: step 0's count; if item grain is over the log's byte bound, item rows are kept only for work made (not held) and `Account` says what was folded.
2. **Can a second process read a segment while the daemon appends?** Needs a committed-length word and a CRC a block. Answer: the disk area's segment format; a simulation with a reader at every append point.
3. **Is Rust fast enough where DuckDB was used?** `--time` walks spans for the critical path. Answer: step 0's timing of `slow` against a prototype fold on reth's cold build log.
4. **Stop under a service manager.** launchd or systemd starts the daemon again on the next connect. Stated in `stop`'s output; is that what the owner expects, or should `stop` also disable the unit until the next command?
5. **Is `.blaze/out` in the checkout acceptable?** Today's rule is no build file in the checkout (docs/cargo.md:1281); `disk.md` already needs a root file there. The owner's decision.
6. **Does anything rely on dependency warnings being counted?** Search the harness for `dependency_warnings`; future-incompatibility reports need a decision of their own.
7. **Time left.** Honest only when the last times are of this machine and shape. Measure the error on reth before showing it; if over 25%, it is not shown.
8. **Windows.** The registry's lock and stop need a named event in place of a signal. Not designed.
9. **Classes of the 91.** Assigned here by name. Each needs its reader read; one that changes an answer and is classed `home` would be a key that misses an input (rule 21).

## 8. How to cut it

- **One mind, compiled before anyone builds on it:** `blaze-events` (rows, `Cause`, `Subject`, `Account`; shared with `daemon.md` 4.6, so the same person as that step), the additive-key rule with its golden gate, and the settings class table.
- **Whole in a few files each:** (a) the registry and stop: a new small crate plus hunks in `cli/daemon.rs` at `stop`, `status`, `other_schema` and in `serve.rs` at `hold`; (b) `Screen` on rows: `blaze-screen` only, judged by its golden transcripts; (c) `Lines` and `CargoJson`: the line functions of `blaze-screen/src/screen.rs` (`:1047-1089`, `:1388-1446`) and `cli/json.rs`; (d) `Why`: a new sans-IO crate over an iterator of rows with recorded logs as fixtures, no daemon needed to test; (e) `run`'s wait and footer: `cli/run.rs` and the client's `main.rs`; (f) the live view's new rows: `blaze-screen/src/screen.rs` by function (`frame`, the status row), after (b).
- **Mechanical, for a literal worker, a crate at a time with the list given:** the 91 variables onto `Settings` (one row of the class table each); `backticks.txt`'s 99 crates onto `say!` and `Report::new`; `prints.txt`'s crates onto `out`.
- **Not to be split:** the deletion of `blaze_api::Response` and `print.rs`'s translation lands with step 2 as one whole port (rule 24); nobody builds a fold on today's `Response`.
