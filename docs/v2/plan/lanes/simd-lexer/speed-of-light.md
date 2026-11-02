---
status: plan
summary: lane simd-lexer's speed of light: the bound on LexedStr::new per classification kernel, the arithmetic, how the SIMD-first lexer reaches it, and where it does not.
date: 2026-10-30
---
# Lane simd-lexer: speed of light

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md); more than 10% off the bound is a bug. The component
is `LexedStr::new` (`crates/parser/src/lexed_str.rs`, `lexed_str/classify.rs`,
`lexed_str/scan.rs`). Machine figures are those of the format lane's lexer section
([format/speed-of-light.md](../format/speed-of-light.md#the-lexer-lexedstrnew-cratesparsersrclexed_strrs-lexed_strscanrs-format-lexer)):
one core of EPYC Turin or Graviton 5, about 3.5 GHz, about 4 instructions a cycle for
straight-line integer code. Its token mix applies too: about 3.5 bytes a token, 40%
whitespace, 30% identifiers and keywords, 22% punctuation, 8% literals, lifetimes and comments.

The benchmark `crates/parser/benches/lex.rs` measures the clock (a chain of dependent adds),
lexes every `.rs` file of `library/` warm, and fails when cycles a byte are more than 10% over
the bound below for the kernel the build has (`LexedStr::KERNEL`).

## The output fixes the work

The output is unchanged: a 4-byte token per token (kind in 10 bits, length in 22, longer tokens
in a side table), a checkpoint per 16 tokens, and the errors, exactly as `rustc_lexer` driven by
the converter gives them. Memory does not bind: about 2.9 bytes moved per byte of text (the text,
40 bytes of classes per 64 written and read back from L1, and 4.25 bytes a token) is 0.09 cycles
a byte from L2 and about 0.4 from DRAM.

## Instructions, per byte of text

- **Each simple token, about 11.** Bit scan of the start bitmap (1), clear the lowest bit (1),
  length from the next start (1), first byte and its kind from `FIRST` (2), pack (2), store the
  token and its start into the block's 64-entry windows (2), the hard-bit test and the loop
  (2). The format lane's 9 leaves out the start store, which the checkpoints need, and the test
  that the token is before the block's first special start.
- **Keywords, 5.7 a token.** An identifier needs `SyntaxKind::from_keyword_in` (about 19
  instructions; 30% of tokens).
- **Checkpoints and the special dispatch, about 1.3 a token.**

  So about 18 a token, **5.15 a byte**.
- **Each block, 0.6 a byte.** Growing the token column by a 64-entry window and truncating it,
  the checkpoint pushes (about 4 per block), the special-start masks
  (`first_hard = hard & -hard`, `run = bits & (first_hard - 1)`), about 40 a block. Zeroing the
  window is part of this: 8 vector stores with AVX-512, 16 with AVX2 or NEON. Writing into the
  column's spare capacity without zeroing would save about 0.15 a byte but needs
  `Vec::set_len`, unsafe code in the token loop; it is inside the 10%, so the loop stays safe.
- **Special tokens, 0.8 a byte.** About 8% of tokens, about 35 instructions each: a number,
  comment, string, char or lifetime is lexed by `scan::special` from the bitmaps (the next
  `\n`, quote or `/`/`*` bit), escapes checked once on the contents.
- **Classification**, per kernel (the bitmaps of 64 bytes, `finish`'s run starts and carry,
  about 15, and the 5 stores):

  | kernel | how | instructions / 64 B | a byte |
  | --- | --- | --- | --- |
  | `avx512vbmi` | `vpermi2b` over the 128-entry class table, a `vptestmb` per class, high bytes by sign | 34 | 0.53 |
  | `avx2` | simdjson's nibble tables: four `pshufb` and two `and` per 32 bytes, then and/cmpeq/movemask per class | 108 | 1.69 |
  | `neon` | the 128-entry table in `tbl4`/`tbx4`, `cmtst` and a weighted pairwise `addp` tree, two classes at a time | 105 | 1.64 |
  | `sse2` | compares and ranges, 16 bytes at a time | 200 | 3.1 |
  | `scalar` | the table a byte at a time (targets with none of the above) | 1280 | 20 |

  The nibble tables: `CLASS` is the union of 15 rectangles (a set of low nibbles times a set
  of high nibbles, each giving some classes); 8 are in table A and 7 in table B, one bit each,
  and a `const` assertion checks the tables reproduce `CLASS` for all 256 bytes. AVX-512 VBMI and
  NEON can index 128 entries directly, so they use `CLASS` itself.
- **Mispredicts, 0.2 cycles a byte.** The simple/special branch follows the text; about one
  mispredict in two special tokens, at about 15 cycles, is \(0.08 / 2 \times 15 / 3.5\).

## The bound

The common part is \(5.15 + 0.6 + 0.8 = 6.55\) instructions a byte. At IPC 4, plus the
mispredicts:

| kernel | instructions a byte | cycles a byte | at 3.5 GHz |
| --- | --- | --- | --- |
| `avx512vbmi` | 7.08 | **2.0** | 1.75 GB/s |
| `avx2` | 8.24 | **2.25** | 1.55 GB/s |
| `neon` | 8.19 | **2.25** | 1.55 GB/s |
| `sse2` | 9.65 | **2.6** | 1.35 GB/s |
| `scalar` | 26.6 | **6.85** | 0.5 GB/s |

The critical path does not bind: the only chain carried from token to token is
`bits &= bits - 1`, one cycle a token (0.3 a byte).

The brief asks for several GB/s a core. Classification is not what stands between this bound
and that: on AVX-512 it is 0.53 of 7.08 instructions a byte. The bound is set by the per-token
work the output requires, and above all the keyword test (5.7 of 18 a token). Getting past it
means fewer instructions a token for the same output, for instance testing keywords for a
block's identifiers together with vector compares instead of one probe each; the output and
errors would not change.

## How the design reaches it

simdjson's two stages ([Langdale and Lemire, "Parsing Gigabytes of JSON per Second"](https://arxiv.org/abs/1902.08318)):

- **Stage 1, `classify`.** One kernel is compiled in, chosen by `target_feature`: AVX-512 VBMI,
  AVX2, SSE2 (x86-64 without AVX2), NEON, or the table for other targets. They are one design:
  each gives the same six bitmaps a 64-byte block (identifier byte, whitespace, `\n`, `"` or `\`,
  `/` or `*`, special first byte), and `finish` turns identifier and whitespace runs into token
  starts. Every x86-64 kernel is built in every x86-64 build, so the tests run each one the CPU
  has against the table. Bits past the text's end are set, so every search stops there.
- **Stage 2, `scan::simple`.** Between a block's special starts every token is a run or a
  one-byte punctuation, so the loop is the 11 instructions above, with no load on the chain.
  The run just before a special start is checked against `STOPS` (an identifier run followed
  by `#`, `"` or `'`, or a run followed by a byte of 0x80 or more, can be one token with it).
- **`scan::special`** lexes numbers, comments (nested block comments by walking the `/`/`*`
  bits with a depth), strings, raw strings, chars, byte forms, lifetimes and raw identifiers
  from the bitmaps. It hands a token to `rustc_lexer` and the converter only where the answer
  is theirs to give: a lexer error, a form whose meaning is the edition's (C strings, raw
  lifetimes, guarded strings, reserved prefixes, `k#`), or a non-ASCII byte outside strings and
  comments. A test holds this to at least 99% of the special tokens of `crates/parser/src`.

The differential tests (`lexed_str/differential.rs`) compare every token, checkpoint, long
token and error with `rustc_lexer` driven by the converter, on every `.rs` file of `library/`,
`crates/` and `harness/` in two editions, on random and mutated texts, and per kernel on the
classes.

## Where the code is (speed-sol2-format, 2026-10-30)

This table is the bound for the loop's shape. The format lane's lexer section used to give 1.26
cycles a byte; it left out the special tokens, the mispredicts and the start store, and now
points here. The code counts higher than the bound in two places:
- **A simple token is about 15 instructions, not 11.** The 11 leaves out the first byte's
  bounds check, the `IDENT` compare before the keyword path, the window index and the count.
- **A special token used to end the block's window.** `simple` returned to `lex_rest`, and the
  next call zeroed a new window, about 45 instructions on 8% of tokens. Now a special token
  that ends in its block is lexed in the open window.

The estimate is about 2.2 cycles a byte with AVX-512 VBMI, 1.1× the bound. The count, what is
left (the start store, the bounds check, a special token past its block) and the tests are in
[format's section](../format/speed-of-light.md#where-the-code-is-2026-10-30-speed-sol2-format).

## Hashing names for the interner (`LexedStr::new_hashed`)

`syntax_bridge`'s text-to-token-tree paths intern every identifier they lex. `new_hashed` gives
`new`'s tokens and also a `u64` column, `name_hashes`: the interner's hash
(`blaze_phf::name_hash`, FxHash's multiply-rotate on 8-byte words with murmur3's finalizer) of
each identifier, `_` and strict keyword, of the name without `r#`. The bridge hands each to
`Symbol::intern_hashed`, so no name is hashed twice. `new` does not hash and costs what it did.

- **One read for the keyword and the hash.** A name of 8 bytes or fewer, most of them, is one
  word. `scan::simple` reads it once (`keywords::ident_window`) and gives the keyword test
  (`strict_by_window`, the perfect hash of `syntax_kind/keywords.rs`) and the hash
  (`name_hash_finish(name_hash_word(name_hash_start(len), window))`) the same register. A longer
  name, which is no keyword, is hashed word by word. Tokens lexed off the simple path
  (`LexedStr::push`: raw identifiers, non-ASCII names) are hashed from their text.
- **Cost, about 20 instructions a name.** The start and the word (4), the finalizer (8), the
  push (about 5), more for the names longer than a word. At 30% of tokens and 3.5 bytes a
  token that is about 1.7 instructions a byte, **0.45 cycles a byte** at IPC 4, added to every
  kernel's bound: 2.45 for `avx512vbmi`. The bench measures both lexes.
- **Compiled-in names skip the table.** `intern::symbol::symbols::COMPILED` is a perfect hash
  (`blaze_phf::name_phf_table!`) keyed by the same `name_hash` over every compiled-in text: the
  declared symbols, plus the 372 of the 512 most frequent names of `library/` and `harness/`
  (keywords, comments and strings left out) that were not already among them and do not
  shadow a name `symbols.rs` binds. Interning one is that
  lookup, a compare of the bytes and `Symbol::from_index`, with no probe of the interner's
  table. Any other name probes the table once under the hash it was handed.
- **Tests.** `differential.rs` lexes every text the differential tests lex (the corpus, the
  parser's files, random and mutated texts) with `new_hashed` too: the tokens and errors are
  `new`'s, and each hash is `name_hash` of the name read back from the text. A warm hashed lex
  allocates nothing. `blaze-phf` checks `name_hash` against the interner's old code and the
  one-word formula; `intern` pins its values, finds every compiled-in text without the table,
  and checks a handed hash gives the same symbol as hashing. The bridge's corpus test compares
  the hashed lex's token trees with the syntax tree's conversion, which hashes names itself.
