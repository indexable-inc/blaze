| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| ripgrep | polonius | 2980 | 2135 | 583 | 311 | 0 | 53.4% | 833 | 32 | 1060 | 7068 |
| ripgrep | nll | 2980 | 2135 | 583 | 267 | 0 | 45.8% | 833 | 28 | 919 | 6564 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 124 | 7 | 2 | 9 | 62 | 6 | 0 | 43 | 11 | 0 | 22 |
| nll | 108 | 6 | 1 | 8 | 56 | 5 | 0 | 39 | 1 | 0 | 20 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 7068 | 5844 | ripgrep | `ignore::default_types::DEFAULT_TYPES` | 10832 | 10683 | 0 | 10305 | 10305 | constraints 2498, liveness 1025, infer 883 |
| 2848 | 4302 | ripgrep | `rg::flags::lowargs::<LowArgs>::fmt` | 2498 | 2573 | 234 | 3813 | 3813 | constraints 955, liveness 682, moves 284 |
| 2567 | 3903 | ripgrep | `rg::flags::hiargs::<HiArgs>::fmt` | 2274 | 2342 | 213 | 3470 | 3470 | constraints 873, liveness 564, moves 272 |
| 2410 | 4523 | ripgrep | `ignore::dir::<Ignore>::matched_ignore` | 1130 | 548 | 24 | 662 | 662 | constraints 1293, liveness 461, moves 259 |
| 2234 | 3311 | ripgrep | `rg::flags::hiargs::<HiArgs>::from_low_args` | 1715 | 365 | 52 | 322 | 322 | constraints 706, liveness 635, moves 373 |
| 2093 | 4069 | ripgrep | `rg::flags::doc::help::generate_long_flag` | 1286 | 815 | 57 | 812 | 812 | constraints 1019, liveness 375, moves 226 |
| 2038 | 4911 | ripgrep | `ignore::dir::<Ignore>::add_child_path_with_found_ignore_files` | 911 | 465 | 19 | 463 | 461 | constraints 1200, liveness 318, moves 188 |
| 1960 | 4819 | ripgrep | `rg::search_parallel` | 1342 | 488 | 58 | 464 | 464 | constraints 687, liveness 512, moves 308 |
| 1864 | 4924 | ripgrep | `ignore::walk::<WalkParallel>::visit` | 817 | 340 | 30 | 401 | 397 | constraints 928, liveness 385, moves 224 |
| 1677 | 3242 | ripgrep | `globset::<GlobSet>::new` | 1164 | 365 | 54 | 309 | 309 | liveness 536, constraints 511, moves 260 |
| 1576 | 3784 | ripgrep | `grep_printer::standard::<StandardImpl<'a, M, W>>::write_exceeded_line` | 1022 | 367 | 14 | 381 | 381 | constraints 786, liveness 306, moves 191 |
| 1563 | 2370 | ripgrep | `rg::flags::defs::FLAGS` | 1523 | 879 | 0 | 879 | 879 | liveness 511, constraints 408, loans 200 |
| 1469 | 2996 | ripgrep | `grep_regex::ban::check` | 818 | 290 | 18 | 289 | 281 | constraints 788, liveness 232, moves 159 |
| 1433 | 1844 | ripgrep | `rg::search` | 887 | 212 | 27 | 190 | 190 | liveness 458, constraints 419, moves 228 |
| 1396 | 2285 | ripgrep | `grep_regex::config::<ConfiguredHIR>::new` | 988 | 310 | 40 | 282 | 282 | constraints 431, liveness 427, moves 233 |
| 1359 | 3389 | ripgrep | `rg::flags::doc::man::generate_flag` | 1052 | 706 | 49 | 683 | 683 | constraints 627, liveness 215, moves 176 |
| 1341 | 2955 | ripgrep | `rg::flags::parse::<Parser>::parse` | 912 | 311 | 22 | 287 | 287 | constraints 477, liveness 353, moves 213 |
| 1315 | 2186 | ripgrep | `grep_regex::strip::strip_from_match_ascii` | 827 | 143 | 19 | 132 | 132 | constraints 621, liveness 256, moves 184 |
| 1281 | 1852 | ripgrep | `rg::flags::doc::help::remove_roff` | 672 | 415 | 27 | 339 | 339 | constraints 650, liveness 238, moves 151 |
| 1242 | 1629 | ripgrep | `rg::flags::hiargs::<HiArgs>::walk_builder` | 801 | 263 | 35 | 241 | 241 | constraints 413, liveness 339, moves 179 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 6564 | 5844 | ripgrep | `ignore::default_types::DEFAULT_TYPES` | 10832 | 10683 | 0 | 10305 | 10305 | constraints 2418, infer 1052, liveness 933 |
| 2439 | 4302 | ripgrep | `rg::flags::lowargs::<LowArgs>::fmt` | 2498 | 2573 | 234 | 3813 | 3813 | constraints 891, liveness 618, moves 280 |
| 2302 | 4523 | ripgrep | `ignore::dir::<Ignore>::matched_ignore` | 1130 | 548 | 24 | 662 | 662 | constraints 1268, liveness 442, moves 252 |
| 2223 | 3903 | ripgrep | `rg::flags::hiargs::<HiArgs>::fmt` | 2274 | 2342 | 213 | 3470 | 3470 | constraints 819, liveness 572, moves 255 |
| 2055 | 3311 | ripgrep | `rg::flags::hiargs::<HiArgs>::from_low_args` | 1715 | 365 | 52 | 322 | 322 | constraints 659, liveness 606, moves 337 |
| 1928 | 4069 | ripgrep | `rg::flags::doc::help::generate_long_flag` | 1286 | 815 | 57 | 812 | 812 | constraints 971, liveness 371, moves 214 |
| 1927 | 4911 | ripgrep | `ignore::dir::<Ignore>::add_child_path_with_found_ignore_files` | 911 | 465 | 19 | 463 | 461 | constraints 1160, liveness 311, moves 172 |
| 1802 | 4819 | ripgrep | `rg::search_parallel` | 1342 | 488 | 58 | 464 | 464 | constraints 632, liveness 498, moves 302 |
| 1676 | 4924 | ripgrep | `ignore::walk::<WalkParallel>::visit` | 817 | 340 | 30 | 401 | 397 | constraints 774, liveness 388, moves 225 |
| 1504 | 3242 | ripgrep | `globset::<GlobSet>::new` | 1164 | 365 | 54 | 309 | 309 | liveness 487, constraints 464, moves 259 |
| 1449 | 3784 | ripgrep | `grep_printer::standard::<StandardImpl<'a, M, W>>::write_exceeded_line` | 1022 | 367 | 14 | 381 | 381 | constraints 766, liveness 256, moves 181 |
| 1358 | 2996 | ripgrep | `grep_regex::ban::check` | 818 | 290 | 18 | 289 | 281 | constraints 748, liveness 220, moves 154 |
| 1331 | 2370 | ripgrep | `rg::flags::defs::FLAGS` | 1523 | 879 | 0 | 879 | 879 | liveness 485, constraints 320, loans 175 |
| 1289 | 1844 | ripgrep | `rg::search` | 887 | 212 | 27 | 190 | 190 | liveness 424, constraints 394, moves 210 |
| 1285 | 3389 | ripgrep | `rg::flags::doc::man::generate_flag` | 1052 | 706 | 49 | 683 | 683 | constraints 621, liveness 210, moves 178 |
| 1271 | 2285 | ripgrep | `grep_regex::config::<ConfiguredHIR>::new` | 988 | 310 | 40 | 282 | 282 | liveness 402, constraints 374, moves 237 |
| 1255 | 2186 | ripgrep | `grep_regex::strip::strip_from_match_ascii` | 827 | 143 | 19 | 132 | 132 | constraints 591, liveness 257, moves 181 |
| 1171 | 2955 | ripgrep | `rg::flags::parse::<Parser>::parse` | 912 | 311 | 22 | 287 | 287 | constraints 406, liveness 326, moves 195 |
| 1134 | 2043 | ripgrep | `ignore::gitignore::<GitignoreBuilder>::add_line` | 897 | 327 | 24 | 326 | 326 | constraints 506, liveness 230, moves 165 |
| 1129 | 1288 | ripgrep | `globset::glob::<Parser<'a>>::parse_star` | 1142 | 156 | 30 | 200 | 200 | constraints 397, liveness 291, moves 196 |
