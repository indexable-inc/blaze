---
title: Rewriting Bun in Rust
description: "Why & how we rewrote Bun from Zig to Rust"
date: "2026-07-08T16:00:00.000Z"
author: jarred
---

Disclosure: Bun was acquired by Anthropic in December 2025. I and others on the Bun team work at Anthropic. I used a pre-release version of Claude Fable 5 for much of the Rust rewrite.

Bun started as a line-for-line port of esbuild's JavaScript & TypeScript transpiler from Go to Zig. I wrote my first line of Zig on [April 16, 2021](https://github.com/ziglang/zig/issues/8575). I bet on Zig after seeing the single-page [Zig Language Reference](https://ziglang.org/documentation/master/) on Hacker News and getting really excited about the low-level control and care for performance.

From the start, Bun's scope was massive:

- JavaScript, TypeScript, and CSS transpiler, minifier, and bundler
- npm-compatible package manager
- Jest-like test runner
- Node.js & TypeScript-compatible module resolution
- HTTP/1.1 & WebSocket client
- Node.js API implementations like `fs`, `net`, `tls`, and dozens of other modules

The initial version of Bun was written by me in 1 year, in a cramped Oakland apartment, pre-LLM, in Zig. The default outcome for ambitiously-scoped projects like Bun is joining the graveyard of dead side projects on a GitHub profile page. Zig made Bun possible. I would never have been able to build this much in 1 year if it wasn't for Zig.

Nowadays, Bun's CLI gets over 22 million monthly downloads. Popular tools like Claude Code and OpenCode bet on Bun as their runtime. Vercel, Railway, DigitalOcean and more have 1st-party support for Bun.

Bun's scope has also been a challenge for stability. Here's a small sample of bugs we fixed in Bun v1.3.14:

> - heap-use-after-free crash in `node:zlib` when calling `.reset()` on a zlib, Brotli, or Zstd stream while an async `.write()` is still in progress on the threadpool
> - use-after-free crash in `node:zlib` when an `onerror` callback issued a re-entrant `write()` followed by `close()` on native handles
> - use-after-free crashes in `node:http2` when re-entrant JS callbacks (e.g. `session.request()` inside a timeout listener, an options getter, or a write callback) triggered a hashmap rehash, invalidating internal stream pointers
> - use-after-free in `UDPSocket.send()` and `sendMany()` where user code in `valueOf()` or `toString()` callbacks could detach an `ArrayBuffer` between payload capture and the actual send
> - crash and out-of-bounds read in `Buffer#copy` and `Buffer#fill` when a `valueOf` callback detaches or resizes the underlying `ArrayBuffer` during argument coercion
> - heap out-of-bounds write in `UDPSocket.sendMany()` when the socket's connection state changed mid-iteration via user JS callbacks
> - memory leak in `crypto.scrypt` where the callback and protected password/salt buffers were never released when the output buffer allocation failed
> - `SSLWrapper.init` leaked the strdup'd passphrase on error paths
> - memory leak in `tlsSocket.setSession()` where each call leaked one `SSL_SESSION` (~6.5 KB per call) due to a missing `SSL_SESSION_free` after `d2i_SSL_SESSION`
> - memory leak where `fs.watch()` watchers were never garbage collected after `.close()`, caused by a reference count underflow that permanently pinned each watcher as a GC root
> - double-free crash in the CSS parser when `background-clip` had vendor prefixes and multi-layer backgrounds
> - `DuplexUpgradeContext` was never freed — a full leak per `tls.connect({ socket: duplex })`
> - race condition crash in `MessageEvent` where the GC marker thread could observe a torn variant in `m_data` during concurrent access from a `BroadcastChannel` or `MessagePort`

We could have kept fixing these kinds of bugs one-off in perpetuity, but we owe it to our users counting on us to do better than that, and systematically prevent these kinds of bugs from recurring.

### What we were already doing

- We patched the Zig compiler to add Address Sanitizer support. We run our test suite with ASAN on every commit.
- We ship Zig safety-checked ReleaseSafe builds on Windows
- We fuzz Bun's runtime APIs 24/7 using [Fuzzilli](https://github.com/googleprojectzero/fuzzilli), the JavaScript engine fuzzer used by V8 & JavaScriptCore
- We have a whole lot of end-to-end memory leak tests

This is more than many projects do.

## Just be really smart and don't make mistakes?

Our bugfix list felt bad and I was tired of going to sleep worrying about crashes in Bun. I don't blame Zig for that - other users of Zig don't have the bugs we had, and mixing GC with manually-managed memory is an uncommon enough thing for software to need that no language really designs for it. We wouldn't have gotten this far if not for Zig, and I'll always be grateful. Until very recently, programming language choice was a one-way decision for a project like Bun.

JavaScript is a garbage-collected language and modern JavaScript engines like JavaScriptCore (and V8) have strict rules around exception handling and the garbage collector. Zig, like C, doesn't manage memory for you and this is a tradeoff that for many projects is a great reason to use Zig. Zig does not have constructors/destructors, and most cleanup is expected to be written out explicitly at each call site with `defer`.

For Bun, correctly handling the lifetimes of garbage-collected values and manually-managed values has been a major source of stability issues - most often small memory leaks and occasionally, crashes. Every memory allocation has to be meticulously reviewed. Where do these bytes get freed? How do we ensure it only gets freed once? Did we check for JavaScript exceptions properly? Is this garbage-collected pointer visible to the conservative stack scanner? Is this garbage collected memory or manually managed memory?

For stability issues, knowing as early as possible is best. Fuzzing happens after code is merged. CI happens when code is pushed. Runtime safety checks & address sanitizer happens when code is run (hopefully in development, before CI).

One common way to reduce this class of issue is to ensure cleanup code is always run exactly once for code that needs it. Zig is designed to be a simple language with no hidden control flow, and so it prefers the explicit `defer` keyword to run code at the end of a scope over C++'s implicit ~Destructor or Rust's implicit `Drop`.

| Language | Cleanup             |
| -------- | ------------------- |
| Zig      | `defer`, `errdefer` |
| C++      | ~Destructor, &&Move |
| Rust     | Drop                |

For Zig code, when exactly should we be running the cleanup code? If we're passing the same `*T` to many different functions, how do we know when it's no longer accessible and can be cleaned up? How does it work when some functions need to continue to reference the memory after the function is called? Our current approach is a mix of:

- arena lifetimes, where the scope of when it's accessible is clear (parser state doesn't escape the calling function and so AST nodes are a good choice there)
- reference-counting
- pay really close attention

Many projects opt to answer these kinds of questions through a style guide. TigerBeetle's [TigerStyle](https://tigerstyle.dev/) is an example in Zig and Google's 31,000 word [C++ style guide](https://google.github.io/styleguide/cppguide.html) is another. The challenge with style guides is enforcement. How do you make sure the style guide is followed? Historically, code review was the answer with best-effort enforcement via linters & static analyzers.

Having a rigid style guide with clear ownership expectations explicitly spelled out in the type system was a real option for Bun. Since Zig has no operator overloading, we would likely end up with a lot of code looking something like this:

```zig
fn foo(a_ptr: SharedPtr(TCPSocket)) !void {
  const a: *TCPSocket = a_ptr.get();
  defer a_ptr.deref();

  const b = try do_something_with_a(a);
  defer b.deref();

  // ...
}
```

This is less ergonomic than the Zig we expect:

```zig
fn foo(a: *TCPSocket) !void {
  const b = try do_something_with_a(a);
  // ...
}
```

## What about C/C++?

About 20% of Bun's code is written in C++ and Bun embeds several C/C++ libraries:

- JavaScriptCore, the JavaScript engine that powers Safari
- uWebSockets & usockets - our HTTP/WebSocket server, and event loop
- lshpack & lsquic - `HPACK` and HTTP/3 libraries
- BoringSSL, Google's OpenSSL fork
- SQLite

C++ instead of Zig would be a reasonable choice for Bun. We would get constructors & destructors. We could delete lots of `extern "C"` wrapper code.

But, we would still be reliant on style guides enforced through code review, and even with ASAN, memory corruption and memory leaks would still happen.

## Why Rust?

A large percentage of bugs from that list are use-after-free, double-free, and "forgot to free" in an error path. In safe Rust, these are compiler errors and RAII-like automatic cleanup with `Drop`. Compiler errors are a better feedback loop than a style guide.

Historically, rewrites are a terrible idea. Excluding comments, Bun is 535,496 lines of Zig. A rewrite in another language would take a small team of engineers a full year. It would mean freezing bugfixes, security fixes or feature development for that time. The least risky approach to getting something shippable would be a mechanical port from Zig to Rust, with the minimal number of behavioral changes, using the exact same test suite we already use for testing Bun.

Fortunately, Bun's own test suite is written in TypeScript which means it doesn't depend on the runtime's programming language.

A year of zero user-facing impact is not a realistic option we could consider. So, enforcement through code-style to fix stability issues was our best bet, and was our plan when we added Rust-inspired [smart pointers](https://github.com/oven-sh/bun/blob/3a79bd746b11601c9db970b608c73f0b9f96ac81/src/ptr/shared.zig#L569) to Bun's codebase.

But honestly, I didn't want to do it. Homegrown smart pointers offer worse ergonomics than Rust, with none of the guarantees.

What if, instead, I spend a week testing if Anthropic's new model can rewrite Bun in Rust?

At first, I didn't expect it to work. A few days in, a high % of the test suite started passing and I saw how much the new Rust code matched up with the original Zig codebase. My opinion went from "this is worth trying" to "I'm going to merge this".

## Claude, rewrite Bun in Rust.

There are a lot of ways to do a terrible job of this. For example, prompting Claude "Rewrite Bun in Rust. Don't make any mistakes." and then praying it would work is not what I did.

Think about how a person would do this. The first big question is:

> Incremental rewrite? Or, everything all at once?

In my experience porting esbuild's transpiler from Go to Zig for the initial version of Bun (without LLMs), everything all at once is better. An incremental rewrite adds temporary code that you hope gets deleted eventually, and would be painful in the short-medium term.

The second big question: how?

How do we keep Bun in Rust the same Bun as before, with the same architecture, performance, and feature-set while also getting the language features of Rust like the borrow checker? How do we ensure the team can still maintain it after the rewrite?

Do the rewrite that looks like we transpiled our Zig code to Rust. We can gradually refactor it to reduce `unsafe` usage and look more like idiomatic Rust after Bun v1.4 ships.

Those are the only two big questions. Everything else is tactics.

## Loops that write & review code

A lot of day-to-day engineering work as software engineers can be over-simplified into loops.

```js
// Pseudocode, not real code:
let task;
while ((task = todoList.pop())) {
  const result = task();
  const feedback = await Promise.all([review(result), review(result)]);
  await apply(feedback, result);
}
```

A `task` has some context associated with it (a Jira ticket, a GitHub issue, etc). The `result` is the code you wrote to fix it. Code reviewer(s) `review` the changes to check for regressions & correctness. And then you address the feedback.

I rewrote Bun in Rust using about 50 dynamic workflows in Claude Code run continuously over the course of 11 days.

Each dynamic workflow was a loop like this - a workflow for:

- Generate a porting guide mapping Zig patterns & types to Rust patterns & types
- Mechanically port every `.zig` file to a `.rs` file, matching the PORTING.md and LIFETIMES.tsv
- Fix every crate's compiler errors
- Get subcommands like `bun test` or `bun build` to work
- Get every test in Bun's entire test suite to pass
- Several large refactors and cleanup passes

For most of those 11 days (and after), I monitored workflows - manually reading the outputs to check for issues and bugs, and prompting Claude to edit the loop to fix things.

How do you review a PR with +1 million lines added? How do you start to build the confidence needed to responsibly merge large quantities of LLM-authored code?

A language-independent test suite with a million assertions, adversarial code review and when something does go wrong, fixing the process that generates the code instead of hand-fixing the code.

### Adversarial review

Adversarial review asks Claude (in a separate context window) to exhaustively come up with reasons why the changes create bugs or do not work.

#### Split context windows

Usually with humans, the person reviewing the code is not the person who authored the code. The person writing the code wants to merge the code, which can bias their actions to ship before it's ready.

Claude is the same way. The Claude that wrote the code wants the code to get accepted. The Claude that reviews wants to find issues in the code.

1 implementer, 2 or more adversarial reviewers per implementer. The reviewer's only job: find bugs & reasons why the code does not work. The implementer doesn't review. The reviewer doesn't implement.

{% reworkLeaderboard /%}

## What does this look like?

If you're about to do something big and expensive, it saves time and money to de-risk it first.

### Prep work

Before writing any code, I spent about 3 hours talking to Claude about how to map patterns from our Zig codebase closely to Rust. Claude serialized this discussion into a `PORTING.md` document, which ended up on [Hacker News](https://news.ycombinator.com/item?id=48016880).

The next question: how do you add Rust lifetimes to code that manually manages memory?

That's where I prompted Claude something like this:

> Me: Let's kick off a dynamic workflow to analyze the proper lifetimes of every struct field in the codebase. This workflow should read every struct field within every single file and trace the control flow. First, look for struct fields with complex lifetimes to express in Rust, then propose a lifetime for that field, then use 2 adversarial review agents to review that lifetime, then apply any feedback and serialize into a LIFETIMES.tsv for other claudes to look at.

Then a round of adversarial reviews on the `PORTING.md` and the `LIFETIMES.tsv` together to fix any conflicting suggestions and double check everything. I also manually read over it.

### Trial run

Before asking Claude to translate all 1,448 .zig files to .rs files, I started with just 3. For each of the 3 files, 1 implementer wrote the new `.rs` file, 2 adversarial reviewers checked the `.rs` file matched the behavior of the `.zig` file and that it followed the `PORTING.md` & `LIFETIMES.tsv`. After that, 1 fixer applied any suggestions.

### False starts

I asked Claude to loop the workflow on all 1,448 .zig files, and about 2 minutes in, one Claude ran `git stash` before committing. Another ran `git stash pop`. And then `git reset HEAD --hard`. They were stepping on each other! And if I put each Claude into a separate worktree, I would run out of disk space because Bun's git repository is too big and eventually the changes will need to be compiled and seen together.

So, I asked Claude to edit the workflow to instruct Claude to never run `git stash` or `git reset` or any `git` command that doesn't commit a specific file at once. No `cargo` either. No slow commands at all.

Then, Claude resumed the workflows. And it was working! Too slowly, so I split it into just 4 workflow shards each with their own worktree (4 worktrees total), each running 16 claudes committing and pushing files.

### Finally writing the code

Thanks to all the parallelization & this prep work, at peak Claude wrote about 1,300 lines of code per minute. Every line of code was reviewed by two separate adversarial reviewers (also Claude) and went through a round of fixes before committing. Absolutely none of it worked yet.

{% commitPunchcard /%}

Notice the inconsistent timing? I forgot to increase the default IOPS on the EC2 instance this ran on. One slow `grep` command was all it took to freeze disk reads & writes for minutes.

### Compiler errors as a work queue

After writing all the code, I asked Claude to write a workflow fixing every compiler error. We went crate-by-crate.

{% errorWorkQueue /%}

The trickiest class of error was cyclical dependencies.

Our Zig codebase was one compilation unit (effectively one crate). I wanted to split the new Rust codebase into ~100 crates so the Rust would compile faster, but this needed to avoid cyclical dependencies while minimizing changes compared to the original Zig implementation. [My PR](https://github.com/oven-sh/bun/pull/30224) to do this immediately before starting the Rust rewrite was insufficient. Instead of starting over, I ran another workflow to classify where the code with cyclical dependencies should go and write it all down - and then another workflow to do the refactor.

Fixing the cyclical dependencies revealed about 16,000 compiler errors. A massive number for 1 human, but not a crazy number for 64 claudes at once.

To maximize parallelism, the workflow looped over each crate.

- For each crate, run `cargo check`, group the output by file and save the errors to a file
- Fix all the compiler errors within that crate
- 2 adversarial reviewers for the crate's changes
- 1 fixer applies the fixes

To prevent claudes from stepping on each other, `cargo check` only ran at the very start and like the other runs, no `git` until the end.

#### Another false start

Claude interpreted "let's get all the crates to compile" as "stub out the functions with compilation errors". Claude also started adding suspiciously long explanatory comments to document workarounds, so I added this rule for the adversarial reviewers to reject:

> If you need a paragraph-long comment to justify why the workaround is OK, the code is wrong — fix the code.

One prompt edit and a few hours later, these things stopped happening.

### Smoke tests

Models love saying "smoke tests"

Once `cargo check` passed, getting it to compile and run `bun --version` was next. It had linker errors. Then, it panicked immediately on start.

The next goal was to get it to run `bun test <file>`. Once that worked, we could start running tests! Time for another workflow, looping over bun CLI subcommands:

- Save each failing stacktrace to a file along with its subcommand
- For each failing stacktrace grouped by subcommand, have 1 Claude fix
- 2 adversarial reviewers
- 1 fixer applies the suggestions

### Get the test suite passing locally

This workflow looped on test files.

Run about 100 random test files sharded to one of 4 worktrees by folder in the codebase. For each failing test, save the stacktrace & errors to a file, 1 implementer proposes a fix, 2 adversarial reviewers, then 1 fixer applies.

#### Even more false starts

Our test suite has lots of memory leak tests and a handful of integration tests that can take more than a minute - for example: a test that runs `next dev` and checks hot module reloading can pick up on changes 100 times. Several of these tests timeout in debug builds.

We also have stress tests that exhaust the max number of TCP sockets on the machine, tests that read & write gigabytes to disk, and tests that spawn ~10k processes.

This needed stronger isolation than "please", so we used `systemd-run` (cgroups) to limit memory & CPU usage and isolate pid namespaces. The machine ran out of disk space and crashed several times anyway.

### Get the test suite passing in CI

Two days after the first CI run, the failing list was down from 972 test files to 23. A day and a half after that, Linux went fully green — and for the first time, it felt like this Rust rewrite was actually going to work.

{% ciBurndown /%}

The rest of the time leading up to merging it was straightforward. A workflow that looped on fixing CI test failures for each platform until there were no more test failures. Several workflows for Windows-related cleanup, to deduplicate code, to reduce unsafe usage, and to generally clean up some code.

### Merging the Rust rewrite

Once 100% of Bun's test suite passed in CI on all platforms (and I manually verified the tests were in fact running and not being skipped), I ran a bunch of commands locally to test things - and then I pressed the merge button.

Merging into `main` isn't a versioned release. At this point, I was confident enough to move forward and commit to the rewrite, but not yet confident enough to release it.

### Stats

At peak, we were running 4 of these workflows at once each in a separate worktree, each with 16 Claudes per workflow. About 64 Claudes at a time.

{% portReplay /%}

#### 0 tests skipped or deleted

11 days (May 3 → merged May 14) · 6,778 commits

| Platform         | expect() calls | Tests  | Files |
| ---------------- | -------------- | ------ | ----- |
| Debian 13 x64    | 1,386,826      | 60,624 | 4,174 |
| macOS 14 arm64   | 1,259,953      | 58,850 | 4,175 |
| Windows 2019 x64 | 1,007,544      | 57,337 | 4,173 |

Pre-merge, this took 5.9 billion uncached input tokens, 690 million output tokens, and 72 billion cached input token reads — around $165,000 at API pricing. By hand, I think this would've taken 3 engineers with full context on the codebase about a year, during which time we wouldn't be able to improve Node.js compatibility, fix bugs, fix security issues or implement new features. We never would've done that. The realistic alternative was to do nothing and keep fixing the bugs at the top of this post forever.

This is the bleeding edge of what's possible today. I used a pre-release version of [Claude Fable 5](https://www.anthropic.com/news/claude-fable-5-mythos-5), a Mythos-class model. Claude Code's dynamic workflows kept 64 Claudes running for 11 days (I would've had to write my own harness to pull this off otherwise).

### The work continues

Since merging the Rust port, we've completed 11 rounds of security review from [Claude Code Security](https://claude.com/product/claude-security) and addressed the findings.

We've also added 24/7 coverage-guided fuzzing of every parser in Bun — JavaScript, TypeScript, JSX, CSS, JSON5, JSONC, TOML, YAML, Markdown, INI, Bun Shell scripts, semver ranges, .patch files, and CSS colors. The fuzzer automatically sends the bugs it finds to Claude to submit a PR reproducing & fixing, and humans review the PRs. So far, it's executed our parsers 100 billion times which has led to around 15 PRs.

At the time of writing, about 4% of Bun's Rust code sits inside an `unsafe` block (~13,000 `unsafe` keywords across ~27,000 lines / ~780,000 lines), and 78% of those blocks are a single line — a pointer that came from C++, or one call into a C library. I expect this number to go down over time as we refactor from a faithful Zig port (which had no greppable `unsafe` keyword) to idiomatic Rust, but we are going to continue using C & C++ libraries like JavaScriptCore so it will always have more `unsafe` than pure Rust projects.

### Porting mistakes

The focus of the Rust rewrite is stability, but it would be impossible to ship a massive change like this and introduce zero regressions.

This rewrite introduced 19 known regressions, each of which has been fixed.

Most of the regressions came from code that's syntactically identical in both languages but semantically different.

#### Side effect inside `debug_assert!`

These two snippets look similar but behave differently. Zig's `assert` is a function, so its argument runs in every build. Rust's `debug_assert!` is a macro, so in release builds the whole expression is erased, including the `insert_stale` call.

```zig
// Zig:
if (dev.framework.react_fast_refresh) |rfr| {
    assert(try dev.client_graph.insertStale(rfr.import_source, false) == IncrementalGraph(.client).react_refresh_index);
}

// Rust:
if let Some(rfr) = &dev.framework.react_fast_refresh {
    debug_assert!(dev.client_graph.insert_stale(&rfr.import_source, false)? == react_refresh_index);
}
```

`insert_stale` adds a file to the frontend dev server's hot reload graph. In release builds it stopped running, and HMR broke in certain cases for projects with HTML routes that use React while a hot reloaded file gets invalidated: `Cannot destructure property 'isLikelyComponentType' of 'k'`. Debug builds worked. [#30678](https://github.com/oven-sh/bun/issues/30678)

#### Slices of odd length

Bun's Zig helper `reinterpretSlice(u16, bytes)` (predating builtin casts supporting slices) used `@divTrunc` and ignored a trailing odd byte. `bytemuck::cast_slice` panics on it instead. `Blob.text()` on a UTF-16 byte order mark followed by an odd number of bytes stopped returning a string and panicked the process. We went back to ignoring the odd byte: `&buf[..buf.len() & !1]`. [#31188](https://github.com/oven-sh/bun/issues/31188)

#### Bounds checks

On macOS & Linux, we compiled Bun's Zig code with `ReleaseFast`, which removes bounds checks. Rust's release builds keep them.

Bun's module resolver interns long filenames into a global list that spills into overflow blocks. The original Zig code sized each block at `count / 4`, or 2048. The port left a placeholder:

```rust
/// ... so use a nonzero stand-in until Phase B threads the
/// per-instantiation value through.
pub const BSS_OVERFLOW_BLOCK_SIZE: usize = 64;
```

That lowered the ceiling from 8.4 million interned filenames to 270,272, which real projects hit, and made a `ptrs[4095]` off-by-one we ported from Zig reachable. Rust panicked instead of writing past the end. Zig would also panic in this case, if we used `ReleaseSafe` (we only did on Windows). [#31503](https://github.com/oven-sh/bun/issues/31503)

#### `comptime` format strings

`Output.pretty` rewrites `<r>` and `<d>` color markers into ANSI escapes. In Zig, `fmt` is `comptime`, so the markers are gone before the arguments are substituted. Rust functions don't have comptime parameters, so `Output::pretty` only ever saw the finished string, and rewrote markers over the arguments too.

```zig
// Zig:
pub inline fn pretty(comptime fmt: string, args: anytype) void;
Output.pretty("<r>{f}<r>", .{hyperlink});

// Rust:
pub fn pretty(payload: impl PrettyFmtInput);
Output::pretty(format_args!("<r>{}<r>", hyperlink));
```

`bun update -i` prints package names as [OSC 8](https://gist.github.com/egmontkob/eb114294efbcd5adb1944c9f3cb5feda) hyperlinks, terminated by `ESC \`. That backslash sits right before the `<` of the trailing `<r>`, the marker parser eats it, and the `r` prints as text.

{% image src="/images/update-interactive-r.png" caption="it should say oxfmt, not oxfmtr" /%}

In Rust it has to be a macro: `bun_core::pretty!("<r>{}<r>", hyperlink)`. [#30693](https://github.com/oven-sh/bun/issues/30693)

## Bun is better in Rust

So far, Bun v1.4.0 fixes 128 bugs that reproduce in v1.3.14. These range from memory leaks to crashes to miscolored help text.

### Reduced memory usage

Rust has a powerful language-level tool for cleaning up memory: `Drop`. When `Drop` is implemented, the `drop` function is automatically called every time the value goes out of scope.

```rust
impl Drop for Bytes {
    fn drop(&mut self) {
        if !self.pinned.is_empty() {
            JSC__JSValue__unpinArrayBuffer(self.pinned);
        }
    }
}
```

In Zig, `defer` can be used to run code at the end of a scope:

```zig
const bytes: ArrayBuffer = try .fromPinned(global, value);
defer bytes.unpin();
```

In Zig, `defer` needs to be added to every individual call site that might need cleanup. It's easy to end up forgetting to clean up (a memory leak), or to run cleanup code twice in rarely-reached error handling code (a double-free). In Rust, `Drop` runs automatically when the value is no longer accessible - trading "no hidden control flow" for preventing a common footgun.

`Drop` fixed several memory leaks in Bun related to file paths in error handling code.

#### We fixed every instrumentable memory leak

We improved Bun's [LeakSanitizer](https://clang.llvm.org/docs/LeakSanitizer.html) integration to track all [native code memory allocations](https://github.com/oven-sh/bun/pull/30875).

Here's an example: every in-process `Bun.build()` call leaked several megabytes of memory — parsed source text and AST symbol tables that outlived the build they belonged to.

```js
// Bundle the same 60-module project 2,000 times in one process
for (let i = 0; i < 2_000; i++) {
  await Bun.build({
    entrypoints: ["./index.js"],
    minify: true,
    sourcemap: "external",
  });
}
```

In Bun v1.3.14, every build leaks about 3 MB, forever — tools like dev servers that bundle on every request eventually run out of memory. In Bun v1.4.0, memory levels off:

| Builds | Bun v1.3.14 | Bun v1.4.0 |
| ------ | ----------- | ---------- |
| 500    | 1,914 MB    | 526 MB     |
| 1,000  | 3,506 MB    | 586 MB     |
| 1,500  | 5,097 MB    | 608 MB     |
| 2,000  | 6,745 MB    | 609 MB     |

A [previous attempt](https://github.com/oven-sh/bun/pull/24741) to do this in Zig was not merged because the lack of an equivalent of Drop made it more difficult to feel confident merging.

### Smaller binary size

The initial changes in the Rust rewrite reduced binary size by 3.8 MB on Windows, 5.5 MB on macOS, and 6.8 MB on Linux. This is largely because we used too much `comptime` in our Zig code.

{% raw %}

<blockquote class="twitter-tweet"><p lang="zxx" dir="ltr"><a href="https://t.co/RQiMNMNo8C">pic.twitter.com/RQiMNMNo8C</a></p>&mdash; Bun (@bunjavascript) <a href="https://x.com/bunjavascript/status/2056510566018203971?ref_src=twsrc%5Etfw">May 18, 2026</a></blockquote> <script async src="https://platform.x.com/widgets.js" charset="utf-8"></script>

{% /raw %}

After that initial shrinkage, the team explored more opportunities for binary size reduction using linker optimizations like Identical Code Folding, removing unused data from ICU, and lazily decompressing small parts of libicu with a zstd dictionary on-demand.

Combined with the Rust rewrite, ICU changes, and identical code folding, **Bun's binary size shrinks by ~20%** on Linux & Windows.

| Version             | Platform | Size  |
| ------------------- | -------- | ----- |
| Bun v1.4.0 (canary) | Windows  | 76 MB |
| Bun v1.3.14         | Windows  | 94 MB |
| Bun v1.4.0 (canary) | Linux    | 70 MB |
| Bun v1.3.14         | Linux    | 88 MB |

### Reduced stack space usage

The TOML parser, and all of the other recursive-descent parsers in Bun (JSON, YAML, JavaScript, TypeScript, and more) now use less stack space.

This caused some test failures before merging the Rust rewrite:

```js
bun test v1.3.14-canary.1 (e99311e58)
.......

105 | });
106 |
107 | it("Bun.TOML.parse throws on deeply nested inline tables instead of crashing", () => {
108 |   const depth = 25_000;
109 |   const deepToml = "a = " + "{ b = ".repeat(depth) + "1" + " }".repeat(depth);
110 |   expect(() => Bun.TOML.parse(deepToml)).toThrow(RangeError);
                                               ^
error: expect(received).toThrow(expected)

Expected constructor: RangeError

Received function did not throw
Received value: {
  a: {
    b: {
      b: {
        b: {
          b: {
            b: {
              b: {
                b: {
                  b: [Object ...],
                },
              },
            },
          },
        },
      },
    },
  },
}

      at <anonymous> (/var/lib/buildkite-agent/build/test/js/bun/resolve/toml/toml.test.js:110:42)

✗ Bun.TOML.parse throws on deeply nested inline tables instead of crashing [2907.64ms]
```

Rust's LLVM IR codegen emits LLVM's [`llvm.lifetime.start`](https://llvm.org/docs/LangRef.html#llvm-lifetime-start-intrinsic) and [`llvm.lifetime.end`](https://llvm.org/docs/LangRef.html#llvm-lifetime-end-intrinsic) intrinsics for stack variables when they are no longer in use, which lets LLVM reuse stack space slots. This lets large functions with nested scopes use significantly less stack space.

Previously, we manually worked around [an open issue](https://github.com/ziglang/zig/issues/23475) by [refactoring particularly large functions](https://github.com/oven-sh/bun/pull/15993) into many smaller functions.

### 2% - 5% faster

Rust supports cross-language link-time optimization between C/C++ and Rust, which enables inlining across programming languages (how cool is that!!).

We benchmarked Bun v1.3.14 against Bun v1.4.0 on Linux x64 (EC2, Xeon Platinum 8488C). HTTP throughput measured with [oha](https://github.com/hatoo/oha) against hello-world servers, app workloads measured with [hyperfine](https://github.com/sharkdp/hyperfine).

**HTTP throughput (req/s, avg of 3 rounds)**

| server    | Bun v1.3.14 | Bun v1.4.0 | Δ     |
| --------- | ----------- | ---------- | ----- |
| Bun.serve | 169.6k      | 177.7k     | +4.8% |
| node:http | 103.8k      | 108.5k     | +4.5% |
| Elysia    | 158.9k      | 163.3k     | +2.8% |
| express   | 64.5k       | 66.6k      | +3.2% |
| fastify   | 91.5k       | 95.9k      | +4.8% |

**Apps / CLI (hyperfine)**

| workload                | Bun v1.3.14 | Bun v1.4.0 | Δ     |
| ----------------------- | ----------- | ---------- | ----- |
| next build              | 13.62 s     | 13.03 s    | +4.5% |
| vite build (tsc + vite) | 1.69 s      | 1.65 s     | +2.2% |
| tsc -b --force          | 0.94 s      | 0.89 s     | +4.7% |

## Production

Prisma launched the [Prisma Compute](https://www.prisma.io/blog/bun-rust-rewrite-prisma-compute) public beta on Bun's Rust rewrite.

> "We ran into memory leaks and a connection pool that couldn't recover after a VM was paused and resumed. When the Rust rewrite appeared, we tested it against the same failure modes. It handled them perfectly." - Alexey Orlenko

Claude Code v2.1.181 (released June 17th) and later use the Rust port of Bun. Startup got 10% faster on Linux but otherwise, barely anyone noticed. Boring is good.

![Claude Code startup time from production telemetry (Linux p50): v2.1.179 at 517ms vs v2.1.181, the first release on Rust Bun, at 464ms — 10% faster](/images/claude-code-rust-bun-startup.webp)

## Shipping

Bun v1.3.14 was the last version of Bun written in Zig. Bun v1.4.0 will be the first version of Bun written in Rust. It's available in canary now - please report any issues you find:

```sh
$ bun upgrade --canary
```

## Maintainability

For myself and the team, our new Rust codebase feels very similar to the old Zig codebase. For example, here's a snippet of the original Zig code and the new Rust code:

```zig
pub fn canMergeSymbols(
    scope: *Scope,
    existing: Symbol.Kind,
    new: Symbol.Kind,
    comptime is_typescript_enabled: bool,
) SymbolMergeResult {
    if (existing == .unbound) {
        return .replace_with_new;
    }

    if (comptime is_typescript_enabled) {
        // In TypeScript, imports are allowed to silently collide with symbols within
        // the module. Presumably this is because the imports may be type-only:
        //
        //   import {Foo} from 'bar'
        //   class Foo {}
        //
        if (existing == .import) {
            return .replace_with_new;
        }

        // ...
    }

    // ...
}
```

```rust
pub fn can_merge_symbol_kinds<const IS_TYPESCRIPT_ENABLED: bool>(
    scope_kind: Kind,
    existing: symbol::Kind,
    new: symbol::Kind,
) -> SymbolMergeResult {

    if existing == symbol::Kind::Unbound {
        return SymbolMergeResult::ReplaceWithNew;
    }

    if IS_TYPESCRIPT_ENABLED {
        // In TypeScript, imports are allowed to silently collide with symbols within
        // the module. Presumably this is because the imports may be type-only:
        //
        //   import {Foo} from 'bar'
        //   class Foo {}
        //
        if existing == symbol::Kind::Import {
            return SymbolMergeResult::ReplaceWithNew;
        }

        // ...
    }

    // ...
}
```

Anyone who understands the original Zig code understands the mechanically translated Rust code. I reviewed the original Rust rewrite PR by checking the adversarial code review agents were correctly catching discrepancies between the Zig code and the Rust code, that they were ensuring the porting guide and lifetime guide were being followed, and also manually reading a lot of the code myself side-by-side with the Zig vs Rust.

## What's next

Bun v1.4 makes Bun faster, smaller, use less memory and gives the team incredibly powerful tools for systematically improving stability going forward: Rust's borrow checker, Miri (which runs for a growing chunk of code in CI), LeakSanitizer, and 24/7 coverage-guided fuzzing for parsers. There's still more to refactor, but things are off to a great start.

This Rust rewrite would've taken a team of engineers with full-context on the codebase a year of work. With 1 engineer using Fable & closely monitoring Claude Code, we went from start to 100% of the test suite passing on all platforms in 11 days.

One engineer can do a lot more today than a year ago.
