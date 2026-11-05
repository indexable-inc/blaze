# Wasm proc macros in sessions: #830 against #918

[#908](https://github.com/indexable-inc/blaze/issues/908): #830 expanded every wasm proc macro
in a session, so no expansion was kept and no crate that used serde or clap was frozen.
[#918](https://github.com/indexable-inc/blaze/pull/918) keeps sessions for the modules seen to
keep state. Every other module goes back to a fresh instance and the kept expansion.

`harness/bench.py bat just --backend cranelift --runs 3` on an ix VM (arm64, 16 cpu, 32 GB).
The two blazes are CI's `blaze-linux-arm64` of #830 on main (58faf9b1d5, train run
40661487096) and of #918's branch (78baf4f758, train run 40660394631), run one after the
other twice (#830, #918, #830, #918). The `commit` in each JSON is the harness's tree, not
the blaze measured. `sessions-N.json` is #830, `fix-N.json` is #918.

Seconds, #830 first round, #918 first round, #830 second round, #918 second round:

| bat | #830 | #918 | #830 | #918 |
|---|---|---|---|---|
| cold | 61.13 | 58.80 | 68.22 | 68.79 |
| restart (fresh daemon, warm store) | 39.17 | 38.55 | 42.08 | 36.48 |
| no-op | 0.008 | 0.009 | 0.009 | 0.008 |
| body | 0.526 | 0.349 | 0.512 | 0.346 |
| signature | 0.517 | 0.346 | 0.508 | 0.352 |
| leaf | 0.506 | 0.335 | 0.505 | 0.338 |
| struct | 0.520 | 0.354 | 0.525 | 0.355 |
| newfn | 0.528 | 0.353 | 0.523 | 0.351 |

| just | #830 | #918 | #830 | #918 |
|---|---|---|---|---|
| cold | 37.37 | 44.23 | 42.66 | 35.86 |
| restart (fresh daemon, warm store) | 19.70 | 20.31 | 21.22 | 19.18 |
| no-op | 0.007 | 0.007 | 0.007 | 0.007 |
| body | 0.220 | 0.233 | 0.215 | 0.232 |
| signature | 0.258 | 0.255 | 0.242 | 0.253 |
| leaf | 0.220 | 0.229 | 0.216 | 0.234 |
| struct | 0.294 | 0.308 | 0.288 | 0.307 |
| newfn | 0.226 | 0.237 | 0.225 | 0.243 |

On the same VM, the same blaze in both arms measured edits within 5 ms of each other, and
cold and restart within 5 to 7 s. So on bat, every edit is a third faster with #918
(0.51 to 0.53 s against 0.34 to 0.36 s). On just, edits are 10 to 15 ms slower with #918 in
both rounds, which is above that noise. Cold, restart and no-op are within noise for both
projects.

just's extra time is not in instantiating the module for each expansion: a build whose
stateless modules reuse an idle instance
([#963](https://github.com/indexable-inc/blaze/pull/963), closed) measured the same.

On CI's x86_64 runner, ripgrep with #918 was no slower than with #830 or than before #830
(the numbers are in #918).

nushell, which #908 also names, did not build on this VM with either blaze: `nu` reaches a
function blaze could not compile on aarch64, so it has no phases here. anim is not a
`bench.py` project.
