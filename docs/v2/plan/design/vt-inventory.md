---
status: plan
summary: "Every sequence, mode, protocol and encoding ghostty's terminal core accepts (433 rows in 12 sections, each with ghostty's file:line), the bar crates/blaze-vt is held to: each row has a test (vt.md)."
date: 2026-11-24
---
# Ghostty VT coverage inventory

**Path aliases.** All are under `/Volumes/Projects/ghostty/src/`:

| Alias | File |
|---|---|
| P | `terminal/Parser.zig` |
| PT | `terminal/parse_table.zig` |
| S | `terminal/stream.zig` |
| T | `terminal/Terminal.zig` |
| SC | `terminal/Screen.zig` |
| ST | `terminal/stream_terminal.zig` (libghostty handler) |
| SH | `termio/stream_handler.zig` (app handler) |
| M | `terminal/modes.zig` |
| G | `terminal/sgr.zig` |
| O | `terminal/osc.zig` |
| OP | `terminal/osc/parsers/*.zig` |
| D | `terminal/dcs.zig` |
| A | `terminal/apc.zig` |
| KG | `terminal/kitty/graphics_*.zig` |
| KE | `input/key_encode.zig` |
| FK | `input/function_keys.zig` |
| KT | `input/kitty.zig` |
| ME | `input/mouse_encode.zig` |
| PS | `input/paste.zig` |

## 1. Parser

| Item | Effect | Location | Notes |
|---|---|---|---|
| State machine | vt100.net DEC ANSI parser | P, PT | 14 states: ground, escape, escape_intermediate, csi_entry, csi_intermediate, csi_param, csi_ignore, dcs_entry, dcs_param, dcs_intermediate, dcs_passthrough, dcs_ignore, osc_string, sos_pm_apc_string |
| Return value of `next()` | Up to 3 actions: exit, transition, entry | P:251-311 | |
| MAX_INTERMEDIATE = 4 | Extra intermediates are dropped | P:190 | Logs a warning; the sequence still dispatches |
| MAX_PARAMS = 24 | The whole CSI or DCS is dropped | P:203, 293, 369 | Triggers when params_idx ≥ 24 |
| Parameter value | u16, saturating `*\|=10 +\|=` | P | The digit counter is a u8; when it overflows, the digit is ignored (test "csi param too long") |
| `:` separator | Sets the params_sep bit on the preceding param | P | |
| `:` on a final other than `m` | The CSI is dropped | P:386-394 | |
| `:` right after CSI | Goes to csi_ignore | PT | In DCS it goes to dcs_ignore |
| Private markers 0x3C–0x3F | Collected as intermediates in csi_entry | PT | In csi_param they go to csi_ignore |
| 0x30–0x3F in csi_intermediate | Goes to csi_ignore | PT | |
| Anywhere: CAN 0x18 / SUB 0x1A | Execute, then ground | PT | |
| Anywhere: ESC | Goes to escape | PT | |
| Anywhere: 0x80–0x8F, 0x91–0x97, 0x99, 0x9A | Execute, then ground | PT | Execute maps byte c to ESC (c−0x40) |
| Anywhere: 0x9C | Goes to ground | PT | |
| Anywhere: 0x90 / 0x9B / 0x9D | Enter DCS / CSI / OSC | PT | |
| Anywhere: 0x98 / 0x9E / 0x9F | Enter sos_pm_apc_string | PT | SOS and PM are fed to the APC handler, the same as APC |
| ESC `X` `^` `_` | Enter sos_pm_apc_string | PT | |
| osc_string | 0x20–0xFF is OSC data; BEL ends; other C0 bytes are ignored | PT | 0x9C is DATA, not ST. Only BEL, ESC, CAN and SUB end an OSC |
| Leaving osc_string | `osc_parser.end(c)` dispatches | P | Fires on ANY exit, including CAN, SUB and ESC |
| dcs_passthrough | 0x00–0x7E are put, 0x7F is ignored; exiting emits unhook | PT, P | Entry emits dcs_hook with the final byte |
| sos_pm_apc_string | 0x00–0x7F are put except 0x18, 0x1A, 0x1B; 0xA0–0xFF are ignored | PT | Exit emits apc_end |
| Ground fast path | SIMD UTF-8 decode until ESC; 4096-codepoint buffer | S `nextSlice` | Invalid UTF-8 becomes U+FFFD (maximal subpart) |
| Ground codepoint routing | cp ≤ 0x0F executes, 0x1B goes to escape, everything else prints | S:870-889 | So 0x10–0x1F and 0x7F PRINT |
| Fast-path csi_param | Executes 0x00–0x0F, ignores 0x10–0x17/0x19/0x1C–0x1F, CAN/SUB go to ground | S:911-957 | Differs from the table |
| APC bulk slice | Bulk feed of the APC payload | S:791-823 | |

## 2. C0 / C1

Handler: S `execute` at 1057-1087.

| Seq | Name | Effect | Notes |
|---|---|---|---|
| 0x00 NUL | | Ignored | |
| 0x01 SOH, 0x02 STX | | Ignored | |
| 0x03 ETX, 0x04 EOT, 0x06 ACK, 0x0E… | | Logged as unsupported | Only for cp ≤ 0x0F |
| 0x05 ENQ | ENQ | Reply with `enquiry_response` | Config option. In ST it is an effect callback |
| 0x07 BEL | BEL | Bell | |
| 0x08 BS | BS | backspace → cursorLeft(1) | T:1495 |
| 0x09 HT | HT | horizontalTab(1) | T:1910 |
| 0x0A LF | LF | linefeed | T:1489; adds CR when LNM is set |
| 0x0B VT | VT | Same as LF | |
| 0x0C FF | FF | Same as LF | |
| 0x0D CR | CR | carriageReturn | T:1475 |
| 0x0E SO | LS1 | GL = G1 | T:1456 |
| 0x0F SI | LS0 | GL = G0 | |
| 0x10–0x17, 0x19, 0x1C–0x1F in ground | | PRINTED as a cell | Quirk |
| 0x18 CAN | CAN | Cancels the sequence | In ground it is printed (cp > 0x0F) |
| 0x1A SUB | SUB | Cancels the sequence | Same as CAN |
| 0x1B ESC | ESC | Starts an escape sequence | |
| 0x7F DEL | DEL | Printed in ground, ignored inside sequences | Quirk |
| 0x84 | IND | Same as ESC D | C1 bytes only work mid-sequence |
| 0x85 | NEL | Same as ESC E | |
| 0x88 | HTS | Same as ESC H | |
| 0x8D | RI | Same as ESC M | |
| 0x8E | SS2 | Same as ESC N | |
| 0x8F | SS3 | Same as ESC O | |
| 0x90 | DCS | | |
| 0x96 | SPA | Same as ESC V | |
| 0x97 | EPA | Same as ESC W | |
| 0x98 | SOS | Treated as APC | |
| 0x9A | DECID | Same as ESC Z, i.e. DA1 | |
| 0x9B | CSI | | |
| 0x9C | ST | Goes to ground | Not an ST inside OSC |
| 0x9D | OSC | | |
| 0x9E | PM | Treated as APC | |
| 0x9F | APC | | |
| Other 0x80–0x9F | | ESC Fe with no handler, logged as unsupported | |
| UTF-8 U+0080–U+009F in ground | | Printed | |

## 3. ESC

Handler: S `escDispatch` at 2423-2701.

| Seq | Name | Effect | Line | Notes |
|---|---|---|---|---|
| ESC ( F | SCS G0 | Designate G0 | S:2452-2462, T:1450 | F is `B` ASCII, `A` British (0x23 → £), or `0` DEC Special Graphics (0x60–0x7E). Exactly 1 intermediate |
| ESC ) F | SCS G1 | Designate G1 | same | |
| ESC * F | SCS G2 | Designate G2 | same | |
| ESC + F | SCS G3 | Designate G3 | same | `- . /` (96-character sets) are NOT supported |
| ESC 7 | DECSC | saveCursor | S:2463, T:1655 | Saves x, y, style, protected, pending_wrap, origin and charset. One slot per screen |
| ESC 8 | DECRC | restoreCursor | S:2475, T:1671 | If nothing was saved, resets to the origin |
| ESC # 8 | DECALN | Fill the screen with E | S:2475, T:3277 | Resets margins, turns origin off, keeps only fg/bg of the style, ignores protection, homes the cursor |
| ESC D | IND | index | S:2505, T:1969 | |
| ESC E | NEL | index + CR | S:2515 | |
| ESC H | HTS | tabSet | S:2525, T:1948 | |
| ESC M | RI | reverseIndex | S:2535, T:2082 | |
| ESC N | SS2 | Single shift G2 | S:2548 | The field is named `locking=true` but the shift is single |
| ESC O | SS3 | Single shift G3 | S:2562 | |
| ESC V | SPA | Protected mode iso | S:2576, T:1709 | |
| ESC W | EPA | Protected off | S:2582 | Protected mode stays iso for erase purposes |
| ESC Z | DECID | Same as DA1 | S:2588 | |
| ESC c | RIS | fullReset | S:2593, T:3885 | |
| ESC n | LS2 | GL = G2 | S:2602 | |
| ESC o | LS3 | GL = G3 | S:2616 | |
| ESC ~ | LS1R | GR = G1 | S:2630 | GR is stored but never used when printing |
| ESC } | LS2R | GR = G2 | S:2644 | |
| ESC \| | LS3R | GR = G3 | S:2658 | |
| ESC = | DECKPAM | Sets mode 66 | S:2672 | |
| ESC > | DECKPNM | Resets mode 66 | S:2681 | |
| ESC \\ | ST | No-op | S:~2690 | |
| Anything else | | Logged as unsupported | S `logUnsupportedOnce` :2722 | Includes ESC SP F/G, ESC # 3/4/5/6, ESC % G/@, ESC 6, ESC 9 |

## 4. CSI

Handler: S `csiDispatch` at 1089-2257.

Default rule, written "def1" below: 0 params → 1, 1 param → that value, more than 1 → invalid. A 0 value is turned into 1 by the Terminal function. Unless noted, the form must have no intermediates; otherwise it is ignored.

| Seq | Name | Effect | Line | Notes |
|---|---|---|---|---|
| CSI Ps A | CUU | Cursor up | S:1116, T:1502 | def1. Clamped to the margin when inside the region |
| CSI Ps k | VPB | Same as CUU | S:1116 | |
| CSI Ps B | CUD | Cursor down | S:1139, T:1520 | |
| CSI Ps C | CUF | Cursor right | S:1159, T:1537 | |
| CSI Ps D | CUB | Cursor left | S:1182, T:1551 | Reverse wrap applies with 45 (which needs 7) or 1045. Pending wrap reduces the count by 1 |
| CSI Ps j | HPB | Same as CUB | S:1182 | |
| CSI Ps E | CNL | down + CR | S:1202 | |
| CSI Ps F | CPL | up + CR | S:1225 | |
| CSI Ps G | CHA | cursor_col | S:1249 | In ST, origin mode adds the margin offset twice (quirk) |
| CSI Ps \` | HPA | Same as CHA | S:1249 | |
| CSI Pr;Pc H | CUP | setCursorPos | S:1270, T:2100 | 0–2 params; 3 or more is invalid. Relative to margins in origin mode |
| CSI Pr;Pc f | HVP | Same as CUP | S:1270 | |
| CSI Ps I | CHT | Tab forward n times | S:1295 | ST stops early if the cursor does not move |
| CSI Ps J | ED | eraseDisplay | S:1312, T:3145 | 0 below, 1 above, 2 complete, 3 scrollback, 22 scroll_complete; anything else is invalid |
| CSI ? Ps J | DECSED | ED that respects protection | S:1312 | |
| CSI Ps K | EL | eraseLine | S:1345, T:3065 | 0 right, 1 left, 2 all. Mode 4 is unreachable (the check is `<3`) |
| CSI ? Ps K | DECSEL | Protected EL | S:1345 | |
| CSI Ps L | IL | insertLines | S:1385, T:2470 | No-op if the cursor is outside the region. Moves the cursor to the left margin |
| CSI Ps M | DL | deleteLines | S:1403, T:2677 | Same as IL. Count 0 returns early |
| CSI Ps P | DCH | deleteChars | S:1420, T:2963 | Clamped to the right margin. No-op outside the margins |
| CSI Ps S | SU | scrollUp | S:1438, T:2207 | Keeps the cursor and pending wrap. Count clamped to the region |
| CSI Ps T | SD | scrollDown | S:1455, T:2185 | |
| CSI Ps W | CTC | 0 (or none) set tab, 2 clear current, 5 clear all | S:1472 | Other values are invalid |
| CSI ? 5 W | DECST8C | tabReset (every 8 columns) | S:1472, T:1953 | |
| CSI Ps X | ECH | eraseChars | S:1515, T:3013 | max(n,1), clamped to the end of the row. Extends over a wide character. Resets row soft-wrap. Protection only if mode is iso |
| CSI Ps Z | CBT | Tab back | S:1536, T:1923 | Bounded by the left margin only in origin mode |
| CSI Ps a | HPR | cursor_col_relative | S:1553 | Same origin quirk as CHA |
| CSI Ps b | REP | Repeat previous_char max(n,1) times | S:1572 | No-op if nothing has been printed |
| CSI c / CSI 0 c | DA1 | Primary device attributes | S:1589 | |
| CSI > c | DA2 | Secondary device attributes | S:1593 | |
| CSI = c | DA3 | Tertiary device attributes | S:1594 | The app does NOT reply |
| CSI Ps d | VPA | cursor_row | S:1613 | Same origin quirk |
| CSI Ps e | VPR | cursor_row_relative | S:1632 | |
| CSI Ps g | TBC | 0 current, 3 all | S:1652, T:1938 | Needs EXACTLY 1 param; bare `CSI g` is ignored (quirk) |
| CSI Pm h | SM | Set ANSI modes | S:1678 | Loops over all params; unknown modes are logged |
| CSI ? Pm h | DECSET | Set DEC modes | S:1678 | |
| CSI Pm l | RM | Reset ANSI modes | S:1699 | |
| CSI ? Pm l | DECRST | Reset DEC modes | S:1699 | |
| CSI Pm m | SGR | See §6 | S:1720 | |
| CSI > Pp;Pv m | XTMODKEYS | none → legacy; 0 legacy; 1 cursor; 2 function; 4 → other none; `4;2` → other numeric | S:1738 | More than 2 params is invalid. Only numeric sets `modify_other_keys_2` |
| CSI ? Ps m | XTQMODKEYS | Ignored | S:1720 | Not supported |
| CSI 5 n | DSR | Status OK | S:1804 | Needs exactly 1 param |
| CSI 6 n | CPR | Cursor position report | S:1804 | |
| CSI ? 996 n | | Color-scheme query | S:1804 | |
| CSI ? 6 n | DECXCPR | Not supported | | |
| CSI > Ps n | XTMODKEYS reset | Sets other_keys_numeric_except | S:1837 | |
| CSI ? Ps $ p | DECRQM | Mode report (DECRPM) | S:1859 | Exactly the `?$` intermediates. Unknown mode → state 0 |
| CSI Ps $ p | ANSI DECRQM | NO REPLY | S:1859 | Falls through to unimplemented (quirk) |
| CSI ! p | DECSTR | Not implemented | | |
| CSI Ps SP q | DECSCUSR | 0 default, 1/2 block, 3/4 underline, 5/6 bar; odd = blinking | S:1903 | 7 or more is invalid. Updates mode 12 |
| CSI Ps " q | DECSCA | 0/2 off, 1 dec | S:1928 | More than 1 param is invalid |
| CSI > Ps q | XTVERSION | Reports the version | S:1952 | Any params accepted |
| CSI Pt;Pb r | DECSTBM | setTopAndBottomMargin | S:1967, T:2160 | 0 means default. top ≥ bottom is a no-op. Homes the cursor |
| CSI ? Pm r | XTRESTORE | Restore saved modes | S:1983 | ST calls `modes.restore` then `setMode` |
| CSI s | SCOSC / DECSLRM | If mode 69 is set, DECSLRM(0,0); otherwise save cursor | S:2009 | Ambiguous form |
| CSI Pl;Pr s | DECSLRM | setLeftAndRightMargin | S:2009, T:2171 | Needs mode 69 |
| CSI ? Pm s | XTSAVE | Save modes | S:2023 | One slot per mode |
| CSI > Ps s | XTSHIFTESCAPE | 0/1; no param → false | S:2037 | |
| CSI 14 t | | Text area size in pixels | S:2070 | Exactly 1 param |
| CSI 16 t | | Cell size in pixels | S:2070 | |
| CSI 18 t | | Text area size in characters | S:2070 | |
| CSI 21 t | | Title report | S:2070 | |
| CSI 22;Ps[;idx] t | | Push title | S:2070 | Ps must be 0 or 2. Icon (1) is not supported |
| CSI 23;Ps[;idx] t | | Pop title | S:2070 | |
| Other CSI t | XTWINOPS | Unsupported | | No params → error |
| CSI u | SCORC | Restore cursor | S:2140 | |
| CSI ? u | | Kitty keyboard query | S:2145 | |
| CSI > Pf u | | Kitty keyboard push | S:2147 | u5; >31 is invalid; default 0. The stack has 8 entries, and when full the oldest is evicted |
| CSI < Pn u | | Kitty keyboard pop | S:2159 | Default 1. n ≥ 8 resets the stack |
| CSI = Pf;Pm u | | Kitty keyboard set | S:2168 | Pm: 1 set (default), 2 or, 3 and-not; other values invalid |
| CSI Ps @ | ICH | insertBlanks max(1,p) | S:2214, T:2862 | Clears pending wrap even outside the margins |
| CSI Ps $ } | DECSASD | 0 main, 1 status line | S:2232 | Status-line output is discarded |

## 5. Modes

Handlers: M (definitions), ST:578-654, SH:655-824. Default is off unless marked "on". DECRPM is `CSI {?}{mode};{0|1|2}$y`; states 3 and 4 are never used.

| Mode | Name | Effect / notes |
|---|---|---|
| ANSI 2 | KAM | disable_keyboard |
| ANSI 4 | IRM | Insert: print calls insertBlanks(width), except at the end of the line |
| ANSI 12 | SRM | on |
| ANSI 20 | LNM | LF also does CR. SH sends a linefeed_mode message |
| ?1 | DECCKM | Cursor keys use SS3 |
| ?3 | DECCOLM | 80/132 columns; only if ?40 is set (T:3509). Resizes, clears with ED complete, homes the cursor |
| ?4 | DECSCLM | slow_scroll; stored only |
| ?5 | DECSCNM | reverse_colors; marks dirty |
| ?6 | DECOM | Origin; setting it moves the cursor to (1,1) |
| ?7 | DECAWM | on. Wraparound; also controls whether resize reflows |
| ?8 | DECARM | autorepeat; stored |
| ?9 | X10 mouse | mouse_event = x10 |
| ?12 | | cursor_blinking. SH ignores it if config default_cursor_blink is set |
| ?25 | DECTCEM | on |
| ?40 | | enable_mode_3. SH resizes back to the window when it changes |
| ?45 | | reverse_wrap |
| ?47 | | Alt screen, legacy: copies the cursor, no clear |
| ?66 | DECNKM | keypad_keys |
| ?67 | DECBKM | Off means BS sends 0x7F; on means 0x08 |
| ?69 | DECLRMM | Turning it off resets the left/right margins |
| ?1000 | | Mouse normal |
| ?1002 | | Mouse button-event |
| ?1003 | | Mouse any-event. Turning any of 9/1000/1002/1003 off sets mouse_event = none |
| ?1004 | | Focus events. SH sends an immediate focus report when it is set |
| ?1005 | | Mouse utf8 format |
| ?1006 | | Mouse sgr format |
| ?1007 | | alternate_scroll; on |
| ?1015 | | Mouse urxvt format |
| ?1016 | | Mouse sgr_pixels format. Turning any of 1005/1006/1015/1016 off sets format = x10 |
| ?1035 | | ignore_keypad_with_numlock; on |
| ?1036 | | alt_esc_prefix; on |
| ?1039 | | alt_sends_escape; stored (no consumer found) |
| ?1045 | | reverse_wrap_extended |
| ?1047 | | Alt screen; clears the alt screen when leaving it; copies the cursor |
| ?1048 | | Save cursor when set, restore when reset |
| ?1049 | | Save cursor, switch to alt, clear alt, copy cursor. Leaving restores the cursor (T:3775) |
| ?2004 | | Bracketed paste |
| ?2026 | | Synchronized output (SH starts a timer) |
| ?2027 | | Grapheme clustering; the default can come from config |
| ?2031 | | Report color scheme |
| ?2048 | | In-band size reports; SH sends one immediately when set |

## 6. SGR

Parser: G `Parser.next` at 182-430. Applied by SC `setAttribute` at 2048-2210. Colon groups are allowed only on 4, 38, 48 and 58; on any other code the colon group is consumed and becomes `unknown`, which is ignored. An empty parameter list means 0.

| Code | Effect | Notes |
|---|---|---|
| 0 / empty | Reset (unset) | |
| 1 | Bold | |
| 2 | Faint | |
| 3 | Italic | |
| 4 | Single underline | |
| 4:0 | No underline | |
| 4:1 | Single | |
| 4:2 | Double | |
| 4:3 | Curly | |
| 4:4 | Dotted | |
| 4:5 | Dashed | Other values → single |
| 5 | Blink | |
| 6 | Blink | Same as 5 |
| 7 | Inverse | |
| 8 | Invisible | |
| 9 | Strikethrough | |
| 21 | Double underline | |
| 22 | Bold and faint off | |
| 23 | Italic off | |
| 24 | Underline off | |
| 25 | Blink off | |
| 27 | Inverse off | |
| 28 | Invisible off | |
| 29 | Strikethrough off | |
| 30–37 | fg palette 0–7 | |
| 38;5;n / 38:5:n | fg 256-color | n truncated to u8 |
| 38;2;r;g;b | fg RGB | Needs ≥ 5 values in the semicolon form |
| 38:2:r:g:b | fg RGB | 3 colons after the 2 |
| 38:2:cs:r:g:b | fg RGB | Colorspace ignored. Any other colon count → invalid |
| 39 | Default fg | |
| 40–47 | bg palette 0–7 | |
| 48;5;n / 48:5:n | bg 256-color | |
| 48;2… / 48:2… | bg RGB | Same forms as 38 |
| 49 | Default bg | |
| 53 | Overline | |
| 55 | Overline off | |
| 58;5;n / 58:5:n | Underline color 256 | |
| 58;2… / 58:2… | Underline color RGB | |
| 59 | Underline color off | |
| 90–97 | fg palette 8–15 | |
| 100–107 | bg palette 8–15 | |
| 26, 50–52, 54, 56–57, 60–65, 73–75 | Unknown | Ignored |
| 38:3 / 38:4 (CMY/CMYK) | Unsupported | |
| Missing sub-params | Unknown | Ignored |
| Parsing more than 16 params | Works | S test "SGR 17+ params" |

## 7. OSC

Dispatch: S `oscDispatch` at 2259-2391. Prefix parsing: O.
- Capture buffer is MAX_BUF = 2048 bytes; overflow invalidates and drops the OSC.
- OSC 52, 66, 72 and 5522 use an allocating capture with no limit.
- Replies use the same terminator as the request: BEL → `\x07`, otherwise `ESC \`.

| Seq | Name | Effect | Notes |
|---|---|---|---|
| OSC 0 ; t | Title (and icon) | Sets the title | Invalid UTF-8 is ignored. ST truncates at 1024; SH ignores titles ≥ 256 bytes; empty resets to the pwd |
| OSC 1 ; t | Icon | Ignored | |
| OSC 2 ; t | Title | Sets the title | |
| OSC 4 ; i ; spec [; i ; spec…] | Palette | Set | i 0–255 = palette, 256–260 = special. Parsing stops at the first error |
| OSC 4 ; i ; ? | Palette query | Reply | |
| OSC 5 ; i ; spec | Special colors | Parsed | 0 bold, 1 underline, 2 blink, 3 reverse, 4 italic. Not applied by ST |
| OSC 7 ; url | pwd | report_pwd | ST stores the URL raw (truncated at 4096). SH accepts file:// or kitty-shell-cwd:// with a local host, percent-decodes, and uses it as the title if no title was seen |
| OSC 8 ; params ; uri | Hyperlink start | | Params are `:`-separated key=value; only `id` is used, and an empty id is ignored |
| OSC 8 ;; | Hyperlink end | | Empty URI with an id is invalid |
| OSC 9 ; text | iTerm2 notification | Body only | Any text not matching the subcommands below |
| OSC 9;1;ms | ConEmu sleep | Parsed only | max 10000, default 100 |
| OSC 9;2 | ConEmu message box | Parsed only | |
| OSC 9;3 | ConEmu tab title | Parsed only | Empty resets |
| OSC 9;4;st;pct | ConEmu progress | Implemented | st 0 remove, 1 set, 2 error, 3 indeterminate, 4 pause. pct clamped to 0–100 |
| OSC 9;5 | ConEmu wait input | Parsed only | |
| OSC 9;6 | ConEmu guimacro | Parsed only | |
| OSC 9;7 | ConEmu run process | Parsed only | |
| OSC 9;8 | ConEmu env var | Parsed only | |
| OSC 9;9;path | ConEmu pwd | report_pwd | Implemented |
| OSC 9;10[;0-3] | ConEmu xterm emulation | Parsed only | |
| OSC 9;11 | ConEmu comment | Parsed only | |
| OSC 9;12 | ConEmu prompt | Semantic prompt A | Implemented |
| OSC 10 | fg | Set or `?` query | OSC 10–19 set successive dynamic colors when given several args |
| OSC 11 | bg | Set or query | |
| OSC 12 | cursor | Set or query | Query falls back to fg (T:159) |
| OSC 13–19 | Pointer fg/bg, tek fg/bg, highlight bg, tek cursor, highlight fg | Parsed | Ignored by the handlers |
| RGB spec forms (all color OSCs) | | | `rgb:h/h/h` (1–4 hex digits each), `rgbi:f/f/f`, `#rgb`, `#rrggbb`, `#rrrgggbbb`, `#rrrrggggbbbb`, bare hex, X11 names (x11_color.zig); whitespace trimmed |
| OSC 21 ; k=v… | Kitty color | Set; empty value resets; `?` queries | Keys 0–255, foreground, background, selection_foreground, selection_background, cursor, cursor_text, visual_bell, second_transparent_background. Key limit 2·(255+8). Only fg, bg and cursor are applied |
| OSC 22 ; name | Mouse shape | | 34 CSS names plus X11 aliases (mouse.zig:101-158). Unknown names are ignored |
| OSC 52 ; kind ; b64 | Clipboard set | | Empty kind means `c`; `s` selection, `p` primary, other → standard. Empty data clears |
| OSC 52 ; kind ; ? | Clipboard read | SH only, with permission | ST never forwards reads |
| OSC 66 | Kitty text sizing | Parsed only | ≤ 4096 bytes |
| OSC 72 | Kitty drag and drop | Parsed only | |
| OSC 104 | Palette reset (all) | | ST resets only the modified (masked) entries |
| OSC 104 ; i;… | Palette reset (listed) | | Invalid entries are skipped |
| OSC 105 | Special color reset | UNREACHABLE | The state machine accepts only `;` or `4` after "10" |
| OSC 110 | fg reset | | |
| OSC 111 | bg reset | | |
| OSC 112 | cursor reset | | |
| OSC 113–119 | Other dynamic resets | Parsed, ignored | OSC 110–119 with any arg are a no-op |
| OSC 133;A | Fresh line + prompt start | | Options: k, aid, redraw (0/1/last), click_events (1/2), cl (line/m/v/w) |
| OSC 133;B | Input start | | Ended by C |
| OSC 133;I | Input start | | Ended at end of line |
| OSC 133;C | Output start | | Column-0 heuristic. SH starts a command |
| OSC 133;D[;code] | Command end | | SH stop_command; code cast to u8, out of range → 1 |
| OSC 133;L | Fresh line | | No options allowed |
| OSC 133;N | Same as A | | |
| OSC 133;P;k=i/r/c/s | Prompt start | | Also options err, cmdline (printf %q), cmdline_url (percent-encoded), special_key |
| OSC 777;notify;title;body | Desktop notification | | rxvt_extension.zig; only `notify` is accepted |
| OSC 1337;Copy=:b64 | Clipboard | | Keys case-insensitive. Empty or `?` is invalid |
| OSC 1337;CurrentDir=path | pwd | | Every other 1337 key is rejected |
| OSC 3008 | Context signal | Parsed only | start=/end= id of 1–64 chars |
| OSC 5522 | Kitty clipboard | Parsed only | |
| Any other number | | Invalid, dropped | |

## 8. DCS

Parsing: D. Handling: SH:391-547. ST ignores DCS.

| Seq | Name | Effect | Notes |
|---|---|---|---|
| DCS 1000 p | tmux control mode | Enter/exit tmux control mode via a Viewer | Build option |
| DCS + q hex;hex… ST | XTGETTCAP | Looks up each key in the terminfo map | Hex is uppercased, split on `;`, max 1 MB. Extra keys TN, Co=256, RGB=8, plus about 268 caps (terminfo/ghostty.zig, Source.zig `xtgettcapMap`). Unknown keys get NO reply |
| DCS $ q m ST | DECRQSS SGR | `0;…m` | See §12 |
| DCS $ q r ST | DECRQSS DECSTBM | `{t+1};{b+1}r` | |
| DCS $ q s ST | DECRQSS DECSLRM | `{l+1};{r+1}s` | Only if mode 69 is set |
| DCS $ q SP q ST | DECRQSS DECSCUSR | `{1-6} q` | block_hollow is reported as block |
| DCS $ q other | DECRQSS invalid | `\x1bP0$r\x1b\\` | More than 2 payload bytes → ignored with NO reply |
| Anything else (sixel, DECUDK, ReGIS, …) | | Ignored | |

## 9. APC / kitty graphics / glyph protocol

Handlers: A (identify, feed, end); ST apcEnd at 837; KG.

| Seq | Name | Effect | Location | Notes |
|---|---|---|---|---|
| APC G… | Kitty graphics | Identified by the first byte `G` | A | Per-protocol max_bytes |
| APC 25a1;… | Glyph protocol | Identified when the prefix before `;` is `25a1` | A, apc/glyph.zig | Unknown prefixes are ignored |
| a=t | Transmit | | graphics_command.zig:200 | Default action |
| a=T | Transmit and display | | :201 | |
| a=q | Query | | :199 | Loads the image but does not store it |
| a=p | Put (display) | | :205 | |
| a=d | Delete | | :206 | |
| a=f | Animation frame | `ERROR: unimplemented action` | graphics_exec.zig | |
| a=a | Control animation | Unimplemented | | |
| a=c | Compose animation | Unimplemented | | |
| d=a/A | Delete all | Lowercase deletes placements only; uppercase also frees image data | :883 | |
| d=i/I | Delete by id | | | |
| d=n/N | Delete by number | | | |
| d=c/C | Delete at cursor | | | |
| d=f/F | Delete animation frames | | | |
| d=p/P | Delete at point | | | |
| d=q/Q | Delete at point + z | | | |
| d=r/R | Delete by id range | | | |
| d=x/X | Delete by column | | | |
| d=y/Y | Delete by row | | | |
| d=z/Z | Delete by z-index | | | |
| f= | Format | 24 rgb, 32 rgba, 100 png | | Grayscale formats exist internally |
| t= | Medium | d direct, f file, t temp file, s shared memory | | `m` (more chunks) applies only to direct |
| o=z | zlib compression | | | |
| Other keys | | | | q (quiet 0/1/2), i, I, p, s, v, S, O, x, y, w, h, X, Y, c, r, C, U (virtual placement), z, H, V, P, Q; z/H/V are signed i32 |
| Limits | | | graphics_image.zig:22-25 | max dimension 10000; max size 400 MB; storage 320 MB (app) / 10 MB (lib) |
| U+10EEEE | Unicode placeholder | Virtual placement | graphics_unicode.zig:16 | |
| Response | | `ESC _G i=…[,I=…][,p=…];MSG ESC \` | graphics_command.zig:308 | Sent only if id or number is set. Quiet: q=1 suppresses OK, q=2 suppresses all. Chunked transfers inherit q from the first chunk |
| Error strings | | ENOENT / EINVAL … | graphics_exec.zig:108-392 | |
| Storage disabled | | No responses at all | graphics_exec.zig:31 | |
| Glyph s | Support query | Reply `ESC _25a1;s;fmt=<list> ESC \` | apc/glyph.zig | |
| Glyph q | Codepoint query | | | |
| Glyph r | Register glyph | | | |
| Glyph c | Clear registrations | | | Cleared by RIS |

## 10. Input encoding

All sequences are from KE, FK, KT, ME, PS, and `terminal/focus.zig`.

**Modifier values.**
- Legacy modifier param: 2 S, 3 A, 4 SA, 5 C, 6 SC, 7 AC, 8 SAC, 9 Super, through 16 (FK:53-69).
- Kitty modifiers: 1 + bits (shift 1, alt 2, ctrl 4, super 8, hyper 16, meta 32, caps 64, num 128).

**Legacy keys.**

| Key | Plain | Modified | Notes |
|---|---|---|---|
| Up / Down / Right / Left | `CSI A/B/C/D` | `CSI 1;m X` | DECCKM: `SS3 A–D` |
| Home / End | `CSI H` / `CSI F` | `CSI 1;m H/F` | DECCKM: `SS3 H/F` |
| Insert / Delete | `CSI 2~` / `CSI 3~` | `CSI 2;m~` / `CSI 3;m~` | |
| PgUp / PgDn | `CSI 5~` / `CSI 6~` | `CSI 5;m~` / `CSI 6;m~` | |
| F1–F4 | `SS3 P/Q/R/S` | `CSI 1;m P`, `CSI 1;m Q`, `CSI 13;m~`, `CSI 1;m S` | Modified F3 uses `13;m~` |
| F5–F12 | `CSI 15/17/18/19/20/21/23/24 ~` | `CSI n;m~` | F13 and above have no legacy encoding |
| Keypad 0–9 . / * - + Enter | Digits / Enter → CR | Application: `SS3 p–y n o j m k M`, or `SS3 m X` with mods | Application only when ?66 is set and ?1035 is off |
| Keypad arrows / Home / End / Begin / Ins / Del / PgUp / PgDn | Same as the main keys | | Begin = `CSI E` / `SS3 E` |
| Backspace | 0x7F (0x08 with DECBKM) | Ctrl → 0x08 (0x7F with DECBKM); alt → `ESC 0x7F`; other_keys mode 2 → `CSI 27;m;127~` | |
| Tab | `\t` | Shift → `CSI Z`; alt → `ESC \t`; others → `CSI 27;m;9~` | |
| Enter | `\r` | Shift → `CSI 27;2;13~`; alt → `ESC \r`; others → `CSI 27;m;13~` | |
| Escape | `ESC` | Alt → `ESC ESC`; others → `CSI 27;m;27~` | |

**Legacy text, Ctrl and Alt.**

| Case | Output | Notes |
|---|---|---|
| Ctrl + char | Kitty C0 table (KE:774-827) | Space, @, 2 → 0; 3 → ESC; 4 → FS; 5 → GS; 6, ~, ^ → RS; 7, /, _ → US; 8, ? → DEL; a–z → 1–26 except i, m, [ (those go to CSI u). Ctrl+0, Ctrl+1, Ctrl+9 send the digit itself |
| Ctrl + C0 + alt | ESC prefix | |
| Ctrl + other char | `CSI cp;m u` (fixterms) | Uppercase A–Z with shift is lowercased |
| Alt + char | `ESC c` | Needs ?1036. On macOS depends on option-as-alt |
| modifyOtherKeys 2 | `CSI 27;m;cp~` | For cp in 0x40–0x7F, any non-shift modifier, or shift+space |
| macOS Cmd + key | Nothing | |
| IME / composing text | Committed UTF-8 is sent; nothing while composing | |

**Kitty keyboard protocol.**

| Case | Output | Notes |
|---|---|---|
| Kitty, flag 1 (disambiguate) | `CSI code[:shifted[:base]][;mods[:event]][;text]u`, or `~`, or `CSI 1;m X` for special keys | Functional codes in KT:27-111: Esc 27, Enter 13, Tab 9, BS 127, F13–F25 57376–57388, KP 57399–57427, Caps 57358, Scroll 57359, Num 57360, PrtSc 57361, Pause 57362, modifier keys 57441–57450 |
| Kitty, no report-all | Enter/Tab/BS with no mods → legacy bytes | Unmodified printable text is sent as text |
| Kitty, flag 2 (events) | `:1` press, `:2` repeat, `:3` release | In the `u` and `~` forms press `:1` is never sent, and `;mods` is omitted when mods = 1 with no other event (KE:1029); special keys always send the event, press too: `CSI 1;m:1 X`, e.g. `CSI 1;1:1 A` (KE:1074) |
| Kitty, flag 2 without flag 8 | Releases for Enter/Tab/BS are dropped | |
| Kitty, flag 4 (alternates) | Shifted and base-layout alternates | Not for control codes |
| Kitty, flag 8 (report all) | Everything as escapes, including modifier-only keys | |
| Kitty, flag 16 (associated text) | Third field carries codepoints | Not with ctrl/super/hyper/meta, or alt (except macOS option); not on release |

**Mouse, focus and paste.**

| Case | Output | Notes |
|---|---|---|
| Mouse X10 (format) | `CSI M` + (32+b), (33+x), (33+y) | x, y ≤ 222, otherwise dropped |
| Mouse utf8 (1005) | `CSI M` + UTF-8(32+b, x+33, y+33) | |
| Mouse SGR (1006) | `CSI < b;x;y M` / `m` | Release uses `m` |
| Mouse urxvt (1015) | `CSI 32+b;x;y M` | |
| Mouse SGR-pixels (1016) | Same as SGR with pixel coordinates | Rounded; can be negative or outside |
| Mouse button codes | L 0, M 1, R 2, wheel 4–7 → 64–67, buttons 8/9 → 128/129 | Shift +4, alt +8, ctrl +16, motion +32; no-button motion = 3. Legacy release = 3. X10 event mode has no modifiers, press of L/M/R only |
| Mouse event modes | 1000: no motion; 1002: motion only with a button held; 1003: all | Motion only when the cell changes (except sgr_pixels). Outside the viewport: release always; motion only in motion modes with a button held |
| Focus (1004) | `CSI I` / `CSI O` | terminal/focus.zig:22 |
| Paste | NUL, BS, ENQ, EOT, ESC, DEL, ^C, ^\\, ^U, ^Z, ^Q, ^S, ^W, ^V, ^R, ^O → space | |
| Paste, bracketed (2004) | Wrapped in `CSI 200~` … `CSI 201~` | |
| Paste, not bracketed | LF → CR | `isSafe` rejects text containing `\n` or `ESC[201~` |

## 11. Screen model

All in T unless noted.

| Behaviour | Location | Notes |
|---|---|---|
| Printing | 882 | Grapheme clustering only with ?2027. Zero-width marks attach to the previous cell. VS15/VS16 attach only after an emoji |
| Pending wrap | 882, 1410 | Printing at the right limit sets pending_wrap. With DECAWM, the next print wraps first. The row is marked wrapped only at the real last column |
| Wide character at the last column | 882 | Writes a spacer_head and wraps. Without wraparound the character is dropped |
| Right margin | 882 | Applies only when the cursor is inside it |
| Insert mode | 882 | insertBlanks(width) before printing |
| Charset mapping | printCell 1242 | Single shift is used once, otherwise GL. A non-ASCII character in a table charset becomes a space |
| Cell attributes | 1242 | The protected flag and hyperlink are stored per cell |
| CR | 1475 | Goes to the left margin in origin mode or when the cursor is at or past it; otherwise column 0 |
| index / LF | 1969 | Scrolls at the region bottom. Scrollback is created only when the region is the full screen. Outside the region the cursor just moves down (it does not scroll) |
| RI | 2082 | Scrolls down at the region top, only when inside the left/right margins |
| Cursor movement | 1502-1551 | Clamped to the margins when inside them |
| Reverse wrap | 1551 | Follows xterm |
| Tabs | Tabstops.zig, 1910-1953 | Interval 8. HT is bounded by the right margin |
| Margins | 2160, 2171 | Bottom 0 means max. Invalid margins are a no-op. Setting margins homes the cursor |
| IL / DL / ICH / DCH | 2470, 2677, 2862, 2963 | Margin-aware. ICH clears pending wrap first |
| Protection | 1709 | SPA → iso; DECSCA → dec. iso persists even after EPA. Erases respect protection if mode is iso or a selective erase was requested |
| ED 2 (complete) | 3176 | On the primary screen, if the last non-empty row is a prompt, it does scrollClear first (^L heuristic, issue #905). Deletes all kitty images. Sets the dirty.clear flag |
| ED 22 | 3156 | scrollClear (pushes content into scrollback). Falls back to complete |
| ED 3 | 3270 | eraseHistory |
| ED 0 / ED 1 | 3236, 3253 | EL right/left plus the rows below/above |
| EL right | 3072 | Includes the spacer_tail column. Resets row soft-wrap |
| EL left | 3087 | Includes the wide tail |
| EL complete | 3100 | Does NOT reset soft-wrap (matches xterm). All EL modes clear pending wrap |
| ECH | 3013 | Extends over a wide character. Resets soft-wrap |
| DECALN | 3277 | |
| Screens | switchScreen 3705 | Primary and alternate. The alternate is created lazily with scrollback 0. Switching ends the hyperlink on the old screen, carries charset state over, and clears the selection |
| 47 / 1047 / 1049 | 3775-3844 | 47 copies the cursor. 1047 copies the cursor and clears the alt screen when leaving. 1049 always saves the cursor, then switches, clears and copies; leaving restores the cursor on primary |
| Resize | 3537 | Primary reflows only if ?7 is set. Alternate never reflows. Tab stops are rebuilt when cols change. Margins reset |
| DECCOLM | 3509 | |
| RIS | 3885 | Back to primary; alternate destroyed; screen reset; modes and flags reset; tabs; previous_char; pwd; title; glyph glossary; status display; margins. ST also resets the cursor style; SH also resets mouse shape, color scheme and progress |
| REP | | Uses previous_char |
| Cursor styles | cursor.zig | bar, block, underline, block_hollow |
| Kitty keyboard flags | kitty/key.zig | 8-entry stack per screen |
| Scrollback | | Default 10000 |
| Pwd / title storage | 3603, 3647 | Null-terminated buffers; empty clears |
| Hyperlinks | hyperlink.zig | Per-cell id. Implicit ids exist |
| Status line (DECSASD 1) | S:2232 | Printing is discarded |
| Title stack | XTWINOPS 22/23 | With an optional index |
| Saved modes | M | One slot per mode |
| Color palette | ST:656-741 | 256 entries with a mask of modified entries; fg, bg and cursor dynamic colors |
| Semantic prompt rows | 1736 | Row flags `prompt` / `prompt_continuation` / `none`, used by the ED heuristic and click_events |
| Kitty images | | Per screen; marked dirty on scroll |
| Grapheme storage | page.zig | |
| Selection | Selection.zig | Not a VT feature |
| Glyph glossary | | Per terminal |

## 12. Replies

| Trigger | Reply | Location |
|---|---|---|
| DA1 (`CSI c`, `ESC Z`, 0x9A) | App: `\x1B[?62;22;52c`, or `\x1B[?62;22c` when clipboard write is denied. Lib default: `\x1b[?62;22c` | SH:834, device_attributes.zig |
| DA2 `CSI > c` | App: `\x1B[>1;10;0c`. Lib default: `\x1b[>1;0;0c` | |
| DA3 `CSI = c` | Lib: `\x1bP!\|00000000\x1b\\`. App: NONE | |
| DSR 5 | `\x1B[0n` | ST:382 |
| CPR (DSR 6) | `\x1B[{row};{col}R` | 1-based; relative to margins in origin mode |
| `CSI ? 996 n` | `\x1B[?997;1n` (dark) or `\x1B[?997;2n` (light) | device_status.zig. Also sent unsolicited when ?2031 is set |
| DECRQM `CSI ? n $ p` | `\x1B[?{n};{0\|1\|2}$y` | M. The ANSI form gets no reply |
| XTVERSION | App: `\x1BP>\|ghostty {ver}\x1B\\`. Lib: `\x1BP>\|{ver or libghostty}\x1B\\` | ST:429, SH |
| `CSI 14 t` | `\x1b[4;{hpx};{wpx}t` | size_report.zig |
| `CSI 16 t` | `\x1b[6;{cellh};{cellw}t` | |
| `CSI 18 t` | `\x1b[8;{rows};{cols}t` | |
| `CSI 21 t` | `\x1b]l{title}\x1b\\` | ST:440; the app sends it via the surface |
| ?2048 set / on resize | `\x1B[48;{rows};{cols};{hpx};{wpx}t` | |
| Kitty keyboard query `CSI ? u` | `\x1b[?{flags}u` | |
| OSC 4 query | `\x1b]4;{i};rgb:rrrr/gggg/bbbb` + terminator | App can be configured for 8-bit `rgb:rr/gg/bb` or no reply |
| OSC 10 / 11 / 12 query | `\x1b]{10\|11\|12};rgb:…` + terminator | Cursor falls back to fg |
| OSC 21 query | `\x1b]21;{key}=rgb:rr/gg/bb` (8-bit) + terminator | An unset color gives an empty value. SH:1482 |
| OSC 52 read | `\x1b]52;{kind};{b64}` + terminator | App only, with permission |
| ENQ | Config `enquiry_response` | |
| DECRQSS valid | `\x1bP1$r{payload}\x1b\\` | SH |
| DECRQSS invalid | `\x1bP0$r\x1b\\` | |
| DECRQSS m payload | `0[;1][;2][;3][;4][;5][;7][;8][;9]` + fg (`;3n`, `;9n`, `;38:5:n`, `;38:2::r:g:b`) + bg (`;4n`, `;10n`, `;48:5:n`, `;48:2::r:g:b`) + `m` | T:3416. Underline style, overline and underline color are NOT reported |
| DECRQSS r / s / SP q | See §8 | |
| XTGETTCAP | `\x1bP1+r{HEXNAME}={HEXVAL}\x1b\\` | Booleans have no `=`. Strings without `%` have `\E` and `^X` decoded; strings with `%` are returned raw. Unknown keys get no reply |
| Kitty graphics | `\x1b_Gi=…;OK\x1b\\` or `…;EINVAL: …` | Sent only if the response is longer than 3 bytes |
| Glyph protocol | `\x1b_25a1;…\x1b\\` | |
| Focus | `CSI I` / `CSI O` | |
| Mouse reports | See §10 | |
| Paste brackets | See §10 | |
| Color-scheme change | `\x1B[?997;Psn` | When ?2031 is set |
| Report effects (lib) | write_pty, bell, color_scheme, device_attributes, enquiry, size, title_changed, pwd_changed, clipboard_write, xtversion | ST:54-129 |

## Oracle tests

All under `/Volumes/Projects/ghostty/`.

| File | Tests | Covers |
|---|---|---|
| `src/terminal/Parser.zig` | about 24 (from L417) | Parser |
| `src/terminal/parse_table.zig` | — | Parse table |
| `src/terminal/stream.zig` | 44 (from L2767) | DECSCA, DECSED/DECSEL, DECSCUSR, XTSHIFTESCAPE, SCOSC/SCORC, CSI t, title push/pop, CSI W, SGR with 17+ params, APC slices |
| `src/terminal/stream_terminal.zig` | 69 (from L884) | End-to-end replies: OSC 4/104/10/11/12/21, OSC 133, DECRQM, kitty keyboard query, xtversion, size reports, ENQ, DSR |
| `src/terminal/Terminal.zig` | 392 | Screen semantics; L12561 is a differential test for C0 printing |
| `src/terminal/Screen.zig` | 203 | |
| `src/terminal/PageList.zig` | 258 | Reflow, scrollback |
| `src/terminal/sgr.zig` | L537-1103 | SGR parsing |
| `src/terminal/osc.zig` and `src/terminal/osc/parsers/*.zig` | — | OSC 4/5/7/8/9/21/22/52/66/72/133/777/1337/3008/5522, X11 colors |
| Other terminal files | — | `modes.zig`, `dcs.zig`, `apc.zig`, `apc/glyph/*.zig`, `charsets.zig`, `device_status.zig`, `device_attributes.zig`, `size_report.zig`, `focus.zig`, `Tabstops.zig`, `kitty/key.zig`, `kitty/graphics_command.zig`, `kitty/graphics_image.zig`, `kitty/graphics.zig` |
| `src/terminal/kitty/testdata/` | — | Image fixtures |
| `src/input/key_encode.zig` | 90 | Key encoding |
| `src/input/mouse_encode.zig` | 23 | Mouse encoding |
| `src/input/paste.zig`, `src/input/function_keys.zig` | — | |
| `test/fuzz-libghostty/corpus/` | — | Fuzz corpus, plus `replay-crashes.nu` |

## Deliberately NOT supported

**Control functions**
- 8-bit C1 in ground.
- 96-character charset designators `- . /`; any NRCS other than British; ESC % (UTF-8 select).
- ESC # 3/4/5/6 (double-height/width lines); ESC SP F/G (S7C1T/S8C1T); ESC 6 / ESC 9 (DECBI/DECFI).
- DECSTR; the ANSI DECRQM reply; the DA3 reply (app only); DECXCPR; any DSR other than 5, 6 and ?996.

**CSI forms**
- XTQMODKEYS; XTWINOPS other than 14/16/18/21/22/23 (no move, resize, iconify, or icon-title stack).
- Rectangular operations: DECCRA, DECFRA, DECERA, DECSERA, DECSACE, DECRQCRA.
- DECIC/DECDC; SL/SR (`CSI SP @` / `CSI SP A`).
- XTPUSHSGR/XTPOPSGR, XTPUSHCOLORS/XTPOPCOLORS, XTREPORTSGR, XTSMGRAPHICS, XTCHECKSUM.
- DECSCPP, DECSLPP, DECSNLS, DECSWBV/DECSMBV, DECLL, DECSSDT.
- The `CSI ? 5 W` form is the only DEC form of CTC.
- EL mode 4 (it exists in the code but cannot be reached).

**Mode-related**
- Status-line content (DECSASD 1 output is discarded).
- Modes not in §5: for example 1001, 1010/1011, 1034, 1037, 1040/1041, 1046, 1050–1061, 2001–2003, 7727, 80 (sixel scrolling), 95.

**SGR**
- 26, 50–52, 54, 56–57, 60–65, 73–75, CMY/CMYK color forms; the colorspace field is ignored.

**OSC**
- Applying OSC 1 (icon), OSC 5 and OSC 13–19 (parsed only).
- OSC 105 (unreachable); OSC 106; OSC 113–119 reset actions; OSC 50/46/51.
- OSC 9 subcommands 1, 2, 3, 5, 6, 7, 8, 10, 11 (parsed only).
- OSC 1337 keys other than Copy and CurrentDir.
- OSC 66, 72, 3008 and 5522 (parsed only).
- Kitty OSC 21 keys other than fg, bg and cursor (parsed only).
- OSC 52 read in the lib handler.

**DCS**
- Sixel, DECUDK, ReGIS.
- DECRQSS other than m, r, s and SP q.

**Kitty graphics**
- Animation actions f, a, c.

**Input**
- F13 and above in legacy encoding.