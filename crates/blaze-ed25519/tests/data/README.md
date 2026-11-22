# blaze-ed25519's test data

| File | What | Source | Fetched | License |
|---|---|---|---|---|
| `speccheck-cases.json` | The 12 edge cases of Chalkias, Garillot and Nikolaenko, "Taming the many EdDSAs" (2020), in the paper's order | https://github.com/novifinancial/ed25519-speccheck/blob/65519336fda78a3d016e947df6d82848aca0c9da/cases.json | 2026-11-16, commit `65519336fda78a3d016e947df6d82848aca0c9da` (sha256 `08e47a36…3540450`) | Apache-2.0 (the repository's `LICENSE`) |
| `wycheproof-ed25519_test.json` | Project Wycheproof's 210 Ed25519 verification vectors (`testvectors_v1`, source version `0.9rc5`) | https://github.com/C2SP/wycheproof/blob/a3d5d1423f2592d6646836a45a21a1b209da9f4d/testvectors_v1/ed25519_test.json | 2026-11-16, commit `a3d5d1423f2592d6646836a45a21a1b209da9f4d` (sha256 `ea80ffeb…30b74b3`) | Apache-2.0 (the repository's `LICENSE`) |
| `published.txt` | Both files above as one line per vector, with the reason blaze refuses each | written by `convert.py` | | as its sources |
| `edge.py` | The Python reference: checks RFC 8032 section 7.1's five Ed25519 vectors from their secret keys, generates `EDGE`, `TORSION` and `MIXED_KEYS`, and gives `convert.py` its expected reasons | written here | | the workspace's |
| `convert.py` | The converter: `python3 -I crates/blaze-ed25519/tests/data/convert.py` | written here | | the workspace's |
| `mod.rs` | The vectors typed in, shared by the crate's tests and `examples/cost.rs` | written here; `RFC` from RFC 8032 section 7.1 (https://www.rfc-editor.org/rfc/rfc8032), the rest from `edge.py` | | the workspace's |

The JSON files are kept byte for byte as fetched. Neither the paper's cases nor Wycheproof's
vectors carry blaze's reasons: `convert.py` computes each with `edge.py`'s strict check and asserts
it against the source's own label (Wycheproof: every `valid` accepted, every `invalid` refused,
every `acceptable` refused for its small-order or mixed-order key) and, for the paper's 12,
against the table in `convert.py` (0 and 1 a small-order key, 2 to 5, 8 and 9 a mixed-order key,
6 and 7 S not below L, 10 and 11 the key (0, -1) written with its sign bit).
