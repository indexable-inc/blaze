| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| ripgrep | polonius | 2980 | 2145 | 585 | 311 | 0 | 53.2% | 841 | 32 | 1074 | 7272 |
| ripgrep | nll | 2980 | 2145 | 585 | 268 | 0 | 45.8% | 841 | 28 | 922 | 7026 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 124 | 7 | 1 | 9 | 61 | 6 | 0 | 43 | 11 | 0 | 22 |
| nll | 108 | 6 | 1 | 9 | 57 | 5 | 0 | 39 | 1 | 0 | 20 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 7272 | 5870 | ripgrep | `ignore::default_types::DEFAULT_TYPES` | 10832 | 10683 | 0 | 10305 | 10305 | constraints 2528, liveness 1237, moves 895 |
| 2794 | 4279 | ripgrep | `rg::flags::lowargs::<LowArgs>::fmt` | 2498 | 2573 | 234 | 3813 | 3813 | constraints 924, liveness 641, moves 281 |
| 2514 | 3896 | ripgrep | `rg::flags::hiargs::<HiArgs>::fmt` | 2274 | 2342 | 213 | 3470 | 3470 | constraints 855, liveness 540, moves 266 |
| 2406 | 4511 | ripgrep | `ignore::dir::<Ignore>::matched_ignore` | 1130 | 548 | 24 | 662 | 662 | constraints 1276, liveness 476, moves 257 |
| 2284 | 3379 | ripgrep | `rg::flags::hiargs::<HiArgs>::from_low_args` | 1715 | 365 | 52 | 322 | 322 | constraints 752, liveness 637, moves 370 |
| 2130 | 4075 | ripgrep | `rg::flags::doc::help::generate_long_flag` | 1286 | 815 | 57 | 812 | 812 | constraints 1081, liveness 352, moves 223 |
| 2029 | 4872 | ripgrep | `ignore::dir::<Ignore>::add_child_path_with_found_ignore_files` | 911 | 465 | 19 | 463 | 461 | constraints 1198, liveness 317, moves 180 |
| 1989 | 4852 | ripgrep | `rg::search_parallel` | 1342 | 488 | 58 | 464 | 464 | constraints 725, liveness 505, moves 296 |
| 1689 | 3240 | ripgrep | `globset::<GlobSet>::new` | 1164 | 365 | 54 | 309 | 309 | liveness 531, constraints 514, moves 268 |
| 1659 | 4857 | ripgrep | `ignore::walk::<WalkParallel>::visit` | 817 | 340 | 30 | 401 | 397 | constraints 718, liveness 379, moves 221 |
| 1613 | 3767 | ripgrep | `grep_printer::standard::<StandardImpl<'a, M, W>>::write_exceeded_line` | 1022 | 367 | 14 | 381 | 381 | constraints 812, liveness 307, moves 202 |
| 1476 | 1903 | ripgrep | `rg::search` | 887 | 212 | 27 | 190 | 190 | liveness 460, constraints 423, moves 250 |
| 1467 | 3072 | ripgrep | `grep_regex::ban::check` | 818 | 290 | 18 | 289 | 281 | constraints 791, liveness 246, moves 158 |
| 1455 | 2402 | ripgrep | `rg::flags::defs::FLAGS` | 1523 | 879 | 0 | 879 | 879 | liveness 501, constraints 343, loans 182 |
| 1404 | 2317 | ripgrep | `grep_regex::config::<ConfiguredHIR>::new` | 988 | 310 | 40 | 282 | 282 | constraints 449, liveness 425, moves 225 |
| 1382 | 3420 | ripgrep | `rg::flags::doc::man::generate_flag` | 1052 | 706 | 49 | 683 | 683 | constraints 641, liveness 226, moves 171 |
| 1349 | 2224 | ripgrep | `grep_regex::strip::strip_from_match_ascii` | 827 | 143 | 19 | 132 | 132 | constraints 639, liveness 263, moves 188 |
| 1311 | 2911 | ripgrep | `rg::flags::parse::<Parser>::parse` | 912 | 311 | 22 | 287 | 287 | constraints 465, liveness 336, moves 207 |
| 1274 | 1792 | ripgrep | `rg::flags::doc::help::remove_roff` | 672 | 415 | 27 | 339 | 339 | constraints 640, liveness 223, moves 141 |
| 1265 | 2022 | ripgrep | `ignore::gitignore::<GitignoreBuilder>::add_line` | 897 | 327 | 24 | 326 | 326 | constraints 568, liveness 238, moves 155 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 7026 | 5870 | ripgrep | `ignore::default_types::DEFAULT_TYPES` | 10832 | 10683 | 0 | 10305 | 10305 | constraints 2451, liveness 1256, infer 1131 |
| 2492 | 4279 | ripgrep | `rg::flags::lowargs::<LowArgs>::fmt` | 2498 | 2573 | 234 | 3813 | 3813 | constraints 893, liveness 623, moves 287 |
| 2290 | 4511 | ripgrep | `ignore::dir::<Ignore>::matched_ignore` | 1130 | 548 | 24 | 662 | 662 | constraints 1246, liveness 443, moves 249 |
| 2185 | 3896 | ripgrep | `rg::flags::hiargs::<HiArgs>::fmt` | 2274 | 2342 | 213 | 3470 | 3470 | constraints 789, liveness 552, moves 257 |
| 2089 | 3379 | ripgrep | `rg::flags::hiargs::<HiArgs>::from_low_args` | 1715 | 365 | 52 | 322 | 322 | constraints 691, liveness 607, moves 344 |
| 1929 | 4872 | ripgrep | `ignore::dir::<Ignore>::add_child_path_with_found_ignore_files` | 911 | 465 | 19 | 463 | 461 | constraints 1173, liveness 301, moves 175 |
| 1914 | 4075 | ripgrep | `rg::flags::doc::help::generate_long_flag` | 1286 | 815 | 57 | 812 | 812 | constraints 997, liveness 330, moves 215 |
| 1786 | 4852 | ripgrep | `rg::search_parallel` | 1342 | 488 | 58 | 464 | 464 | constraints 646, liveness 483, moves 280 |
| 1507 | 4857 | ripgrep | `ignore::walk::<WalkParallel>::visit` | 817 | 340 | 30 | 401 | 397 | constraints 640, liveness 373, moves 209 |
| 1498 | 3240 | ripgrep | `globset::<GlobSet>::new` | 1164 | 365 | 54 | 309 | 309 | constraints 478, liveness 471, moves 244 |
| 1474 | 3767 | ripgrep | `grep_printer::standard::<StandardImpl<'a, M, W>>::write_exceeded_line` | 1022 | 367 | 14 | 381 | 381 | constraints 769, liveness 272, moves 183 |
| 1356 | 3072 | ripgrep | `grep_regex::ban::check` | 818 | 290 | 18 | 289 | 281 | constraints 729, liveness 227, moves 164 |
| 1310 | 2402 | ripgrep | `rg::flags::defs::FLAGS` | 1523 | 879 | 0 | 879 | 879 | liveness 479, constraints 299, loans 182 |
| 1279 | 3420 | ripgrep | `rg::flags::doc::man::generate_flag` | 1052 | 706 | 49 | 683 | 683 | constraints 615, liveness 211, moves 164 |
| 1269 | 2224 | ripgrep | `grep_regex::strip::strip_from_match_ascii` | 827 | 143 | 19 | 132 | 132 | constraints 592, liveness 238, moves 192 |
| 1267 | 1903 | ripgrep | `rg::search` | 887 | 212 | 27 | 190 | 190 | liveness 413, constraints 376, moves 212 |
| 1252 | 2317 | ripgrep | `grep_regex::config::<ConfiguredHIR>::new` | 988 | 310 | 40 | 282 | 282 | liveness 395, constraints 374, moves 213 |
| 1162 | 2022 | ripgrep | `ignore::gitignore::<GitignoreBuilder>::add_line` | 897 | 327 | 24 | 326 | 326 | constraints 518, liveness 248, moves 152 |
| 1156 | 2911 | ripgrep | `rg::flags::parse::<Parser>::parse` | 912 | 311 | 22 | 287 | 287 | constraints 409, liveness 306, moves 192 |
| 1143 | 1792 | ripgrep | `rg::flags::doc::help::remove_roff` | 672 | 415 | 27 | 339 | 339 | constraints 585, liveness 233, moves 130 |
