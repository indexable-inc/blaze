| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| ripgrep | polonius | 2980 | 2164 | 581 | 319 | 0 | 54.9% | 840 | 33 | 1123 | 7416 |
| ripgrep | nll | 2980 | 2164 | 581 | 275 | 0 | 47.4% | 840 | 28 | 999 | 6745 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 122 | 7 | 2 | 9 | 64 | 6 | 0 | 43 | 11 | 0 | 29 |
| nll | 107 | 6 | 1 | 8 | 59 | 5 | 0 | 39 | 1 | 0 | 27 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 7416 | 5887 | ripgrep | `ignore::default_types::DEFAULT_TYPES` | 10832 | 10683 | 0 | 10305 | 10305 | constraints 2698, liveness 1168, moves 888 |
| 2861 | 4229 | ripgrep | `rg::flags::lowargs::<LowArgs>::fmt` | 2498 | 2573 | 234 | 3813 | 3813 | constraints 937, liveness 700, moves 291 |
| 2578 | 3930 | ripgrep | `rg::flags::hiargs::<HiArgs>::fmt` | 2274 | 2342 | 213 | 3470 | 3470 | constraints 868, liveness 618, moves 264 |
| 2473 | 4494 | ripgrep | `ignore::dir::<Ignore>::matched_ignore` | 1130 | 548 | 24 | 662 | 662 | constraints 1277, liveness 478, moves 267 |
| 2298 | 3262 | ripgrep | `rg::flags::hiargs::<HiArgs>::from_low_args` | 1715 | 365 | 52 | 322 | 322 | constraints 707, liveness 652, moves 357 |
| 2199 | 4917 | ripgrep | `rg::search_parallel` | 1342 | 488 | 58 | 464 | 464 | constraints 680, liveness 598, walk 314 |
| 2110 | 4022 | ripgrep | `rg::flags::doc::help::generate_long_flag` | 1286 | 815 | 57 | 812 | 812 | constraints 1024, liveness 345, moves 221 |
| 2065 | 4906 | ripgrep | `ignore::dir::<Ignore>::add_child_path_with_found_ignore_files` | 911 | 465 | 19 | 463 | 461 | constraints 1163, liveness 353, moves 179 |
| 1744 | 3182 | ripgrep | `globset::<GlobSet>::new` | 1164 | 365 | 54 | 309 | 309 | liveness 500, constraints 491, moves 257 |
| 1743 | 4814 | ripgrep | `ignore::walk::<WalkParallel>::visit` | 817 | 340 | 30 | 401 | 397 | constraints 696, liveness 413, moves 254 |
| 1580 | 1883 | ripgrep | `rg::search` | 887 | 212 | 27 | 190 | 190 | liveness 493, constraints 411, moves 237 |
| 1574 | 3779 | ripgrep | `grep_printer::standard::<StandardImpl<'a, M, W>>::write_exceeded_line` | 1022 | 367 | 14 | 381 | 381 | constraints 788, liveness 283, moves 190 |
| 1500 | 3097 | ripgrep | `grep_regex::ban::check` | 818 | 290 | 18 | 289 | 281 | constraints 792, liveness 245, moves 174 |
| 1457 | 2296 | ripgrep | `grep_regex::config::<ConfiguredHIR>::new` | 988 | 310 | 40 | 282 | 282 | constraints 435, liveness 424, moves 266 |
| 1442 | 2412 | ripgrep | `rg::flags::defs::FLAGS` | 1523 | 879 | 0 | 879 | 879 | liveness 468, constraints 353, loans 205 |
| 1406 | 3376 | ripgrep | `rg::flags::doc::man::generate_flag` | 1052 | 706 | 49 | 683 | 683 | constraints 649, liveness 225, moves 180 |
| 1345 | 2169 | ripgrep | `grep_regex::strip::strip_from_match_ascii` | 827 | 143 | 19 | 132 | 132 | constraints 618, liveness 267, moves 186 |
| 1334 | 1786 | ripgrep | `rg::flags::doc::help::remove_roff` | 672 | 415 | 27 | 339 | 339 | constraints 635, liveness 230, walk 165 |
| 1310 | 2915 | ripgrep | `rg::flags::parse::<Parser>::parse` | 912 | 311 | 22 | 287 | 287 | constraints 467, liveness 319, moves 211 |
| 1307 | 2051 | ripgrep | `ignore::gitignore::<GitignoreBuilder>::add_line` | 897 | 327 | 24 | 326 | 326 | constraints 584, liveness 270, moves 155 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 6745 | 5887 | ripgrep | `ignore::default_types::DEFAULT_TYPES` | 10832 | 10683 | 0 | 10305 | 10305 | constraints 2473, infer 1065, liveness 1038 |
| 2526 | 4229 | ripgrep | `rg::flags::lowargs::<LowArgs>::fmt` | 2498 | 2573 | 234 | 3813 | 3813 | constraints 853, liveness 705, moves 288 |
| 2346 | 4494 | ripgrep | `ignore::dir::<Ignore>::matched_ignore` | 1130 | 548 | 24 | 662 | 662 | constraints 1235, liveness 466, moves 254 |
| 2217 | 3930 | ripgrep | `rg::flags::hiargs::<HiArgs>::fmt` | 2274 | 2342 | 213 | 3470 | 3470 | constraints 788, liveness 592, moves 257 |
| 2149 | 3262 | ripgrep | `rg::flags::hiargs::<HiArgs>::from_low_args` | 1715 | 365 | 52 | 322 | 322 | constraints 660, liveness 627, moves 357 |
| 2074 | 4022 | ripgrep | `rg::flags::doc::help::generate_long_flag` | 1286 | 815 | 57 | 812 | 812 | constraints 985, liveness 418, moves 215 |
| 2054 | 4917 | ripgrep | `rg::search_parallel` | 1342 | 488 | 58 | 464 | 464 | constraints 651, liveness 559, walk 308 |
| 1988 | 4906 | ripgrep | `ignore::dir::<Ignore>::add_child_path_with_found_ignore_files` | 911 | 465 | 19 | 463 | 461 | constraints 1148, liveness 344, moves 172 |
| 1616 | 3182 | ripgrep | `globset::<GlobSet>::new` | 1164 | 365 | 54 | 309 | 309 | liveness 477, constraints 460, moves 257 |
| 1592 | 4814 | ripgrep | `ignore::walk::<WalkParallel>::visit` | 817 | 340 | 30 | 401 | 397 | constraints 660, liveness 384, moves 212 |
| 1461 | 3779 | ripgrep | `grep_printer::standard::<StandardImpl<'a, M, W>>::write_exceeded_line` | 1022 | 367 | 14 | 381 | 381 | constraints 748, liveness 273, moves 182 |
| 1454 | 1883 | ripgrep | `rg::search` | 887 | 212 | 27 | 190 | 190 | liveness 466, constraints 379, moves 247 |
| 1350 | 3097 | ripgrep | `grep_regex::ban::check` | 818 | 290 | 18 | 289 | 281 | constraints 734, liveness 225, moves 152 |
| 1305 | 2412 | ripgrep | `rg::flags::defs::FLAGS` | 1523 | 879 | 0 | 879 | 879 | liveness 445, constraints 307, loans 195 |
| 1290 | 2296 | ripgrep | `grep_regex::config::<ConfiguredHIR>::new` | 988 | 310 | 40 | 282 | 282 | liveness 398, constraints 375, moves 237 |
| 1257 | 2169 | ripgrep | `grep_regex::strip::strip_from_match_ascii` | 827 | 143 | 19 | 132 | 132 | constraints 585, liveness 260, moves 179 |
| 1251 | 3376 | ripgrep | `rg::flags::doc::man::generate_flag` | 1052 | 706 | 49 | 683 | 683 | constraints 599, liveness 216, moves 163 |
| 1209 | 2915 | ripgrep | `rg::flags::parse::<Parser>::parse` | 912 | 311 | 22 | 287 | 287 | constraints 416, liveness 333, moves 195 |
| 1205 | 1786 | ripgrep | `rg::flags::doc::help::remove_roff` | 672 | 415 | 27 | 339 | 339 | constraints 581, liveness 211, walk 157 |
| 1161 | 1271 | ripgrep | `globset::glob::<Parser<'a>>::parse_star` | 1142 | 156 | 30 | 200 | 200 | constraints 380, liveness 301, moves 193 |
