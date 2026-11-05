| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| ripgrep | polonius | 2980 | 2142 | 589 | 319 | 0 | 54.2% | 838 | 31 | 1120 | 7171 |
| ripgrep | nll | 2980 | 2142 | 589 | 277 | 0 | 47.1% | 838 | 28 | 983 | 6845 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 122 | 7 | 2 | 9 | 64 | 6 | 0 | 43 | 11 | 1 | 29 |
| nll | 107 | 6 | 1 | 9 | 59 | 5 | 0 | 39 | 1 | 0 | 27 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 7171 | 5854 | ripgrep | `ignore::default_types::DEFAULT_TYPES` | 10832 | 10683 | 0 | 10305 | 10305 | constraints 2533, liveness 1166, moves 880 |
| 2898 | 4295 | ripgrep | `rg::flags::lowargs::<LowArgs>::fmt` | 2498 | 2573 | 234 | 3813 | 3813 | constraints 974, liveness 714, moves 285 |
| 2689 | 3920 | ripgrep | `rg::flags::hiargs::<HiArgs>::fmt` | 2274 | 2342 | 213 | 3470 | 3470 | constraints 922, liveness 633, moves 265 |
| 2540 | 4599 | ripgrep | `ignore::dir::<Ignore>::matched_ignore` | 1130 | 548 | 24 | 662 | 662 | constraints 1357, liveness 478, moves 261 |
| 2404 | 3446 | ripgrep | `rg::flags::hiargs::<HiArgs>::from_low_args` | 1715 | 365 | 52 | 322 | 322 | constraints 733, liveness 697, moves 390 |
| 2221 | 4138 | ripgrep | `rg::flags::doc::help::generate_long_flag` | 1286 | 815 | 57 | 812 | 812 | constraints 1062, liveness 362, moves 253 |
| 2195 | 4893 | ripgrep | `rg::search_parallel` | 1342 | 488 | 58 | 464 | 464 | constraints 685, liveness 570, walk 319 |
| 2133 | 4927 | ripgrep | `ignore::dir::<Ignore>::add_child_path_with_found_ignore_files` | 911 | 465 | 19 | 463 | 461 | constraints 1197, liveness 345, moves 181 |
| 1783 | 3234 | ripgrep | `globset::<GlobSet>::new` | 1164 | 365 | 54 | 309 | 309 | liveness 517, constraints 499, moves 272 |
| 1730 | 5026 | ripgrep | `ignore::walk::<WalkParallel>::visit` | 817 | 340 | 30 | 401 | 397 | constraints 705, liveness 403, moves 221 |
| 1614 | 1999 | ripgrep | `rg::search` | 887 | 212 | 27 | 190 | 190 | liveness 517, constraints 421, moves 235 |
| 1611 | 3788 | ripgrep | `grep_printer::standard::<StandardImpl<'a, M, W>>::write_exceeded_line` | 1022 | 367 | 14 | 381 | 381 | constraints 798, liveness 309, moves 197 |
| 1487 | 3034 | ripgrep | `grep_regex::ban::check` | 818 | 290 | 18 | 289 | 281 | constraints 778, liveness 233, moves 161 |
| 1451 | 2545 | ripgrep | `rg::flags::defs::FLAGS` | 1523 | 879 | 0 | 879 | 879 | liveness 465, constraints 357, loans 206 |
| 1416 | 2299 | ripgrep | `grep_regex::config::<ConfiguredHIR>::new` | 988 | 310 | 40 | 282 | 282 | liveness 436, constraints 421, moves 224 |
| 1385 | 2286 | ripgrep | `grep_regex::strip::strip_from_match_ascii` | 827 | 143 | 19 | 132 | 132 | constraints 620, liveness 275, moves 188 |
| 1383 | 1860 | ripgrep | `rg::flags::doc::help::remove_roff` | 672 | 415 | 27 | 339 | 339 | constraints 649, liveness 229, walk 201 |
| 1375 | 3460 | ripgrep | `rg::flags::doc::man::generate_flag` | 1052 | 706 | 49 | 683 | 683 | constraints 630, liveness 231, moves 177 |
| 1359 | 2961 | ripgrep | `rg::flags::parse::<Parser>::parse` | 912 | 311 | 22 | 287 | 287 | constraints 484, liveness 341, moves 211 |
| 1289 | 2083 | ripgrep | `ignore::gitignore::<GitignoreBuilder>::add_line` | 897 | 327 | 24 | 326 | 326 | constraints 571, liveness 257, moves 159 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 6845 | 5854 | ripgrep | `ignore::default_types::DEFAULT_TYPES` | 10832 | 10683 | 0 | 10305 | 10305 | constraints 2498, infer 1127, liveness 1045 |
| 2592 | 4295 | ripgrep | `rg::flags::lowargs::<LowArgs>::fmt` | 2498 | 2573 | 234 | 3813 | 3813 | constraints 923, liveness 715, moves 287 |
| 2380 | 4599 | ripgrep | `ignore::dir::<Ignore>::matched_ignore` | 1130 | 548 | 24 | 662 | 662 | constraints 1275, liveness 459, moves 259 |
| 2323 | 3920 | ripgrep | `rg::flags::hiargs::<HiArgs>::fmt` | 2274 | 2342 | 213 | 3470 | 3470 | constraints 854, liveness 619, moves 256 |
| 2276 | 3446 | ripgrep | `rg::flags::hiargs::<HiArgs>::from_low_args` | 1715 | 365 | 52 | 322 | 322 | constraints 718, liveness 640, moves 357 |
| 2111 | 4893 | ripgrep | `rg::search_parallel` | 1342 | 488 | 58 | 464 | 464 | constraints 666, liveness 603, walk 312 |
| 2022 | 4927 | ripgrep | `ignore::dir::<Ignore>::add_child_path_with_found_ignore_files` | 911 | 465 | 19 | 463 | 461 | constraints 1180, liveness 336, moves 175 |
| 2022 | 4138 | ripgrep | `rg::flags::doc::help::generate_long_flag` | 1286 | 815 | 57 | 812 | 812 | constraints 1006, liveness 348, moves 237 |
| 1646 | 3234 | ripgrep | `globset::<GlobSet>::new` | 1164 | 365 | 54 | 309 | 309 | liveness 470, constraints 470, moves 258 |
| 1587 | 5026 | ripgrep | `ignore::walk::<WalkParallel>::visit` | 817 | 340 | 30 | 401 | 397 | constraints 661, liveness 387, moves 211 |
| 1523 | 3788 | ripgrep | `grep_printer::standard::<StandardImpl<'a, M, W>>::write_exceeded_line` | 1022 | 367 | 14 | 381 | 381 | constraints 766, liveness 298, moves 185 |
| 1442 | 1999 | ripgrep | `rg::search` | 887 | 212 | 27 | 190 | 190 | liveness 462, constraints 381, moves 225 |
| 1370 | 3034 | ripgrep | `grep_regex::ban::check` | 818 | 290 | 18 | 289 | 281 | constraints 729, liveness 240, moves 154 |
| 1316 | 2545 | ripgrep | `rg::flags::defs::FLAGS` | 1523 | 879 | 0 | 879 | 879 | liveness 450, constraints 300, loans 205 |
| 1294 | 2299 | ripgrep | `grep_regex::config::<ConfiguredHIR>::new` | 988 | 310 | 40 | 282 | 282 | liveness 399, constraints 381, moves 228 |
| 1292 | 2286 | ripgrep | `grep_regex::strip::strip_from_match_ascii` | 827 | 143 | 19 | 132 | 132 | constraints 593, liveness 266, moves 187 |
| 1288 | 3460 | ripgrep | `rg::flags::doc::man::generate_flag` | 1052 | 706 | 49 | 683 | 683 | constraints 607, liveness 238, moves 161 |
| 1264 | 1860 | ripgrep | `rg::flags::doc::help::remove_roff` | 672 | 415 | 27 | 339 | 339 | constraints 598, liveness 240, walk 168 |
| 1210 | 2083 | ripgrep | `ignore::gitignore::<GitignoreBuilder>::add_line` | 897 | 327 | 24 | 326 | 326 | constraints 543, liveness 257, moves 158 |
| 1187 | 1302 | ripgrep | `globset::glob::<Parser<'a>>::parse_star` | 1142 | 156 | 30 | 200 | 200 | constraints 381, liveness 298, moves 201 |
