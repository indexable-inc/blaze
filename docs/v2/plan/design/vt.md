---
status: plan
summary: "`crates/blaze-vt` (`bstd::vt`): a terminal emulator core in Rust, sans-IO, `no_std`, on bstd's owning types, that accepts every sequence, mode and protocol ghostty's terminal core does (vt-inventory.md, 433 rows) and encodes keys, mouse, focus and paste as ghostty does. Bytes in; a screen, replies and effects out as data. For the native shell app's full-screen programs (vim, htop, less) and anything else that hosts a PTY."
date: 2026-11-24
---
# vt: a terminal core in Rust, on bstd

Why: the native shell (nushell's engine in a blaze-ui window, not a terminal) still runs programs
that drive a terminal. harness does that through libghostty-vt (zig, C ABI, `harness-vt`); blaze's
own code takes no third-party or C code (AGENTS.md, scope). This is that piece, owned.

**The bar.** Everything ghostty's terminal core accepts, this accepts with the same effect, and
everything ghostty deliberately refuses is refused the same way, unless a row below says
otherwise and why. The list is [vt-inventory.md](vt-inventory.md) (ghostty 73534c468, every row
with its source location). Each row becomes a test (below), so "supports what ghostty does" is
a count, not a claim.

## Shape

```text
bytes ──► Parser ──► Action ──► Terminal::apply ──► Screen (pages, styles, graphemes, links)
                                      │                  └► Damage (rows changed since taken)
                                      ├► Replies  (bytes for the PTY: DA, CPR, DECRPM, OSC queries, kitty)
                                      └► Effects  (title, pwd, bell, clipboard, notify, prompt marks, progress, …)
KeyEvent / MouseEvent / Paste / Focus ──► input::encode(&Modes) ──► bytes for the PTY
```

- **sans-IO** (rule 14, `#![blaze::sans_io]`): no PTY, no clock, no file. A driver owns the PTY
  and writes `Replies` back; `Effects` are data the host acts on. Things ghostty does by IO
  become requests in `Effects` that the host answers through a method: kitty graphics from a
  file, temp file or shared memory (`Effect::LoadImage { medium, path }` →
  `Terminal::image_loaded`), OSC 52 reads (`Effect::ClipboardRead` → `Terminal::clipboard`), the
  synchronized-output timeout (the host passes `now` to `Terminal::tick`, rule 75).
- **Memory** (rules 11, 18): every buffer is in the allocator the host passes
  (`Terminal::new_in(Opts, alloc)`), through blaze-own; every table has a cap in `Opts`
  (scrollback bytes, OSC capture, image storage 320 MB as ghostty's app, title stack). A refusal
  is a typed `Refused` naming the cap, never a silent drop, except where ghostty drops by design
  (inventory rows say which), and those are counted in `Stats` so a test can see them.
- **Re-export**: `bstd::vt`, as `bstd::ecs` is blaze-ecs.

## Modules (one lane each, disjoint files)

| module | what | inventory § |
|---|---|---|
| `parser` | the vt100.net state machine as ghostty runs it (14 states, 24 params, 4 intermediates, colon sub-params, C1 only mid-sequence), the UTF-8 decoder (maximal-subpart U+FFFD), a ground fast path over ASCII runs | 1, 2 |
| `page`, `screen` | rows in fixed pages (no per-row allocation), cells of 8 bytes (codepoint, style id, flags: wide, spacer head/tail, protected, hyperlink, grapheme), interned styles, graphemes and hyperlinks out of line, scrollback by bytes, reflow on resize, damage | 11 |
| `terminal` | print (pending wrap, wide at the margin, IRM, charsets, REP), cursor, erase (ED/EL/ECH, selective, the ED 2 prompt heuristic), IL/DL/ICH/DCH, SU/SD, margins, tabs, origin, protection, DECALN, alt screens 47/1047/1049, DECSC/DECRC, RIS, XTSAVE/XTRESTORE, title stack, kitty keyboard stack | 2–5, 11 |
| `sgr` | the SGR parser and its application | 6 |
| `osc`, `color` | every OSC row, colour specs (rgb:, rgbi:, #, X11 names), palette with modified mask, dynamic colours | 7 |
| `dcs` | DECRQSS, XTGETTCAP (the terminfo table ghostty answers from), tmux control mode | 8 |
| `kitty` | APC graphics (transmit, put, delete, query; zlib through bstd's inflate, PNG through bstd::image; placements, virtual placements, U+10EEEE), the glyph protocol | 9 |
| `input` | key encoding (legacy, modifyOtherKeys, kitty flags 1–16), mouse (X10, 1005, 1006, 1015, 1016; 1000/1002/1003), focus, paste (bracketed, unsafe bytes) | 10 |
| `reply` | every reply's exact bytes and terminator | 12 |

Known ghostty quirks are kept and each has a test with ghostty's line in its name (inventory
"Things a port could get wrong"): 0x10–0x1F and DEL print in ground; ANSI DECRQM is silent; CAN,
SUB and ESC inside an OSC still dispatch it; bare `CSI g` is ignored. Two are ghostty bugs, not
behaviour, and are fixed here, each with a test saying so: HPA/VPA/HPR/VPR adding the origin
offset twice (libghostty handler), and OSC 105 being unreachable.

## Tests (rules 53, 64, 23)

- **The inventory as tests.** `tests/inventory.rs` holds one case per inventory row: bytes in,
  expected screen text, cursor, style of named cells, replies and effects. A row without a case
  is a failing test (`every_row_has_a_case`), so coverage is counted from the doc.
- **ghostty's own tests, ported as data.** Terminal.zig (392), Screen.zig (203), PageList.zig
  (258), stream.zig (44), stream_terminal.zig (69), sgr.zig, osc parsers, key_encode.zig (90),
  mouse_encode.zig (23): each ported as `(name, bytes, expect)` with its ghostty name, so a
  failure points at the original.
- **A reference model.** `model.rs`: the screen as a plain `rows × cols` grid of `(char, Style)`
  with scrollback a list, written for obviousness. `blaze_sim::check` drives both the model and
  the real `Terminal` with drawn streams (prints, wide chars, combining marks, every CSI the
  model knows, resizes, alt-screen switches) and compares the visible screen, cursor and
  scrollback after each step. Planted-bug controls (rule 78 seam, `VtPlant`): pending wrap
  cleared on the wrong ops, a reflow that drops a soft-wrapped row's tail, a style id freed
  while a cell still names it, a page boundary off by one. Each must fail the check.
- **Fuzz corpus.** ghostty's `test/fuzz-libghostty/corpus/` replayed: no panic, every cap held.

## Floor (rule 67)

ghostty is the floor. Throughput of `feed` on ASCII-heavy output (`cat` of a large file), on
dense SGR (htop), and on scrolling (`yes`), in MB/s/core on the Mac and on Linux arm64, against
libghostty-vt built ReleaseFast on the same machine and the same bytes. The target is within 10%.
The parser's ground path is where it is won: one pass over a run of printable ASCII writing cells
directly, no per-byte dispatch.

## Not now

Sixel, ReGIS, DECUDK, rectangular ops, double-width lines: ghostty refuses them too (inventory,
"Deliberately not supported"). Rendering is not here: the shell's view reads `Screen` and draws
with blaze-ui.
