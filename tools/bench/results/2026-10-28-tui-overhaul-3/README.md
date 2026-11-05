# Build UI overhaul, piece 3a: piped output says only what took time; `blaze status` is the live view (lane tui)

Andrew's brief: output off a terminal is plain, stable lines, and `blaze status` and `blaze why`
render the same model as the terminal. This piece does two things.

- **Piped output says a step's opening line only once it has run half a second.** This uses
  `Plain::begun_by`, which runs before each record is applied and on every tick. Quick steps
  used to print a "Building …", "Compiling …" or "Linking …" line that the next line made
  obsolete. Now a 0.11 s edit prints the one line that matters. A slow step still says it
  began: at the first tick past 0.5 s, or just before its done-line if no tick came between.
- **`blaze status` draws `Live::snapshot`.** While a build is under way, that is the live view
  as of the build's last record: header, steps strip, cores and crate graph. Once it has
  finished, it is the steps strip and the end lines. Before, `blaze status` replayed the
  piped lines.

## Before / after (ripgrep 15.1.0, edit in `crates/core/main.rs`)

Piped edit, `before/edit.piped`:

```
    Building build scripts and proc macros
   Compiling rg
     Linking rg
     Rebuilt 1 fn: search (crates/core/main.rs edited); 0.11s
```

`after/edit.piped`:

```
     Rebuilt 1 fn: search (crates/core/main.rs edited); 0.11s
```

`blaze status` after that edit, `before/status.txt`:

```
  build of …/ripgrep (last):
    Building build scripts and proc macros
   Compiling rg
     Linking rg
     Rebuilt 1 fn: search (crates/core/main.rs edited); 0.11s
```

`after/status.txt`:

```
  build of …/ripgrep (last):
    ✓ build scripts 14/14 steps 0.07s
         Rebuilt 1 fn: search (crates/core/main.rs edited); 0.11s
```

## Numbers (ix VM tui-r2d, arm64, 16 cpus; 8 cores via taskset)

Piece 2 (738bff2f09) and this branch alternated run by run, piped, each with a warm daemon of
its own (`bench-piped.sh` → `piped.jsonl`):

| ripgrep, median wall | piece 2 | this branch |
|---|---|---|
| no-op, piped (30 each) | 19.26 ms | 19.20 ms |
| edit, piped (20 each) | 130.11 ms | 129.99 ms |

The worker is unchanged; only the client's rendering of piped output and `blaze status` moved.
`captures.sh` produced `before/` and `after/`.
