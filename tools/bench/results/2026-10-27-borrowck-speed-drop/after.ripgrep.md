| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| ripgrep | polonius | 2980 | 2125 | 583 | 309 | 0 | 53.0% | 827 | 32 | 1045 | 7405 |
| ripgrep | nll | 2980 | 2125 | 583 | 265 | 0 | 45.5% | 827 | 28 | 911 | 6949 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 122 | 7 | 1 | 9 | 62 | 6 | 0 | 43 | 11 | 0 | 22 |
| nll | 106 | 6 | 1 | 8 | 57 | 5 | 0 | 38 | 1 | 0 | 19 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 7405 | 5906 | ripgrep | `ignore::default_types::DEFAULT_TYPES` | 10832 | 10683 | 0 | 10305 | 10305 | constraints 2474, liveness 1421, moves 879 |
| 2794 | 4252 | ripgrep | `rg::flags::lowargs::<LowArgs>::fmt` | 2498 | 2573 | 234 | 3813 | 3813 | constraints 922, liveness 666, moves 282 |
| 2522 | 3870 | ripgrep | `rg::flags::hiargs::<HiArgs>::fmt` | 2274 | 2342 | 213 | 3470 | 3470 | constraints 851, liveness 577, moves 255 |
| 2402 | 4545 | ripgrep | `ignore::dir::<Ignore>::matched_ignore` | 1130 | 548 | 24 | 662 | 662 | constraints 1283, liveness 459, moves 270 |
| 2274 | 3384 | ripgrep | `rg::flags::hiargs::<HiArgs>::from_low_args` | 1715 | 365 | 52 | 322 | 322 | constraints 706, liveness 652, moves 365 |
| 2088 | 4051 | ripgrep | `rg::flags::doc::help::generate_long_flag` | 1286 | 815 | 57 | 812 | 812 | constraints 1022, liveness 346, moves 235 |
| 2068 | 4865 | ripgrep | `ignore::dir::<Ignore>::add_child_path_with_found_ignore_files` | 911 | 465 | 19 | 463 | 461 | constraints 1188, liveness 325, moves 211 |
| 1987 | 4768 | ripgrep | `rg::search_parallel` | 1342 | 488 | 58 | 464 | 464 | constraints 672, liveness 527, moves 322 |
| 1677 | 3239 | ripgrep | `globset::<GlobSet>::new` | 1164 | 365 | 54 | 309 | 309 | liveness 539, constraints 510, moves 256 |
| 1632 | 4817 | ripgrep | `ignore::walk::<WalkParallel>::visit` | 817 | 340 | 30 | 401 | 397 | constraints 693, liveness 397, moves 219 |
| 1530 | 3756 | ripgrep | `grep_printer::standard::<StandardImpl<'a, M, W>>::write_exceeded_line` | 1022 | 367 | 14 | 381 | 381 | constraints 779, liveness 271, moves 191 |
| 1456 | 3004 | ripgrep | `grep_regex::ban::check` | 818 | 290 | 18 | 289 | 281 | constraints 789, liveness 235, moves 160 |
| 1426 | 2342 | ripgrep | `rg::flags::defs::FLAGS` | 1523 | 879 | 0 | 879 | 879 | liveness 514, constraints 325, loans 182 |
| 1425 | 1896 | ripgrep | `rg::search` | 887 | 212 | 27 | 190 | 190 | liveness 461, constraints 409, moves 228 |
| 1398 | 2248 | ripgrep | `grep_regex::config::<ConfiguredHIR>::new` | 988 | 310 | 40 | 282 | 282 | liveness 431, constraints 410, moves 226 |
| 1349 | 3384 | ripgrep | `rg::flags::doc::man::generate_flag` | 1052 | 706 | 49 | 683 | 683 | constraints 616, liveness 229, moves 168 |
| 1331 | 2204 | ripgrep | `grep_regex::strip::strip_from_match_ascii` | 827 | 143 | 19 | 132 | 132 | constraints 619, liveness 259, moves 189 |
| 1304 | 2942 | ripgrep | `rg::flags::parse::<Parser>::parse` | 912 | 311 | 22 | 287 | 287 | constraints 475, liveness 324, moves 204 |
| 1292 | 1805 | ripgrep | `rg::flags::doc::help::remove_roff` | 672 | 415 | 27 | 339 | 339 | constraints 659, liveness 224, moves 141 |
| 1274 | 1610 | ripgrep | `rg::flags::hiargs::<HiArgs>::walk_builder` | 801 | 263 | 35 | 241 | 241 | constraints 389, liveness 364, moves 179 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 6949 | 5906 | ripgrep | `ignore::default_types::DEFAULT_TYPES` | 10832 | 10683 | 0 | 10305 | 10305 | constraints 2404, liveness 1260, infer 1088 |
| 2496 | 4252 | ripgrep | `rg::flags::lowargs::<LowArgs>::fmt` | 2498 | 2573 | 234 | 3813 | 3813 | constraints 887, liveness 647, moves 287 |
| 2298 | 4545 | ripgrep | `ignore::dir::<Ignore>::matched_ignore` | 1130 | 548 | 24 | 662 | 662 | constraints 1252, liveness 452, moves 244 |
| 2223 | 3870 | ripgrep | `rg::flags::hiargs::<HiArgs>::fmt` | 2274 | 2342 | 213 | 3470 | 3470 | constraints 803, liveness 583, moves 252 |
| 2086 | 3384 | ripgrep | `rg::flags::hiargs::<HiArgs>::from_low_args` | 1715 | 365 | 52 | 322 | 322 | constraints 666, liveness 625, moves 337 |
| 1937 | 4051 | ripgrep | `rg::flags::doc::help::generate_long_flag` | 1286 | 815 | 57 | 812 | 812 | constraints 1000, liveness 331, moves 223 |
| 1893 | 4865 | ripgrep | `ignore::dir::<Ignore>::add_child_path_with_found_ignore_files` | 911 | 465 | 19 | 463 | 461 | constraints 1131, liveness 314, moves 169 |
| 1761 | 4768 | ripgrep | `rg::search_parallel` | 1342 | 488 | 58 | 464 | 464 | constraints 610, liveness 495, moves 276 |
| 1573 | 4817 | ripgrep | `ignore::walk::<WalkParallel>::visit` | 817 | 340 | 30 | 401 | 397 | constraints 701, liveness 372, moves 227 |
| 1482 | 3239 | ripgrep | `globset::<GlobSet>::new` | 1164 | 365 | 54 | 309 | 309 | liveness 486, constraints 458, moves 243 |
| 1448 | 3756 | ripgrep | `grep_printer::standard::<StandardImpl<'a, M, W>>::write_exceeded_line` | 1022 | 367 | 14 | 381 | 381 | constraints 735, liveness 275, moves 197 |
| 1323 | 3004 | ripgrep | `grep_regex::ban::check` | 818 | 290 | 18 | 289 | 281 | constraints 728, liveness 216, moves 153 |
| 1323 | 2342 | ripgrep | `rg::flags::defs::FLAGS` | 1523 | 879 | 0 | 879 | 879 | liveness 485, constraints 297, loans 187 |
| 1276 | 2942 | ripgrep | `rg::flags::parse::<Parser>::parse` | 912 | 311 | 22 | 287 | 287 | liveness 398, constraints 398, moves 195 |
| 1271 | 3384 | ripgrep | `rg::flags::doc::man::generate_flag` | 1052 | 706 | 49 | 683 | 683 | constraints 603, liveness 211, moves 175 |
| 1264 | 1896 | ripgrep | `rg::search` | 887 | 212 | 27 | 190 | 190 | liveness 423, constraints 369, moves 212 |
| 1240 | 2248 | ripgrep | `grep_regex::config::<ConfiguredHIR>::new` | 988 | 310 | 40 | 282 | 282 | liveness 396, constraints 383, moves 212 |
| 1223 | 2204 | ripgrep | `grep_regex::strip::strip_from_match_ascii` | 827 | 143 | 19 | 132 | 132 | constraints 574, liveness 260, moves 177 |
| 1138 | 2046 | ripgrep | `ignore::gitignore::<GitignoreBuilder>::add_line` | 897 | 327 | 24 | 326 | 326 | constraints 521, liveness 230, moves 153 |
| 1130 | 1610 | ripgrep | `rg::flags::hiargs::<HiArgs>::walk_builder` | 801 | 263 | 35 | 241 | 241 | constraints 377, liveness 349, moves 165 |
