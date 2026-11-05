| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| bat | polonius | 576 | 2749 | 269 | 157 | 0 | 58.4% | 475 | 74 | 2407 | 34264 |
| bat | nll | 576 | 2749 | 269 | 139 | 0 | 51.6% | 475 | 57 | 2329 | 34005 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 61 | 3 | 1 | 3 | 45 | 2 | 0 | 20 | 5 | 0 | 9 |
| nll | 54 | 3 | 1 | 3 | 43 | 2 | 0 | 18 | 0 | 0 | 8 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 34264 | 7320 | bat | `bat::syntax_mapping::builtin::BUILTIN_MAPPINGS` | 12513 | 2209 | 0 | 1487 | 1487 | liveness 20379, constraints 7908, moves 3465 |
| 7718 | 9712 | bat | `bat::printer::<InteractivePrinter<'_>>::print_line` | 3680 | 1306 | 117 | 1419 | 1419 | constraints 3156, liveness 1995, moves 842 |
| 5063 | 15855 | bat | `bat::app::<App>::config` | 2440 | 1265 | 35 | 1173 | 1171 | constraints 2592, liveness 979, moves 543 |
| 4749 | 20551 | bat | `bat::clap_app::build_app` | 2667 | 1306 | 15 | 1037 | 1027 | constraints 2400, liveness 1024, moves 591 |
| 3025 | 5818 | bat | `bat::get_languages` | 1189 | 572 | 46 | 556 | 556 | constraints 1663, liveness 549, moves 287 |
| 2407 | 6619 | bat | `bat::printer::<InteractivePrinter<'_>>::print_header` | 1350 | 570 | 39 | 583 | 583 | constraints 1200, liveness 431, moves 301 |
| 2331 | 3286 | bat | `bat::run` | 1584 | 565 | 44 | 497 | 495 | constraints 847, liveness 590, moves 318 |
| 2319 | 6316 | bat | `bat::line_range::<LineRange>::parse_range` | 1527 | 401 | 24 | 387 | 385 | constraints 1177, liveness 424, moves 324 |
| 1984 | 2151 | bat | `bat::controller::<Controller<'_>>::run_with_error_handler` | 797 | 286 | 32 | 342 | 342 | constraints 905, liveness 409, scopes 214 |
| 1964 | 2573 | bat | `bat::list_themes` | 1156 | 406 | 39 | 391 | 391 | constraints 616, liveness 593, moves 280 |
| 1752 | 5450 | bat | `bat::app::<App>::inputs` | 517 | 478 | 14 | 531 | 519 | constraints 1133, liveness 194, moves 143 |
| 1612 | 3685 | bat | `bat::input::<Input<'a>>::open` | 1027 | 299 | 21 | 271 | 271 | constraints 623, liveness 400, moves 273 |
| 1511 | 3141 | bat | `bat::printer::<InteractivePrinter<'a>>::new` | 824 | 356 | 13 | 378 | 376 | constraints 734, liveness 288, moves 180 |
| 1324 | 2322 | bat | `build_script_main::application::gen_man_and_comp` | 1000 | 444 | 30 | 350 | 350 | constraints 565, liveness 289, moves 195 |
| 1316 | 1977 | bat | `bat::diff::get_git_diff` | 781 | 158 | 22 | 140 | 140 | constraints 554, liveness 355, moves 171 |
| 1261 | 2287 | bat | `bat::output::<OutputType>::try_pager` | 800 | 318 | 34 | 250 | 250 | constraints 508, liveness 286, moves 172 |
| 1187 | 1510 | bat | `bat::controller::<Controller<'_>>::print_file_ranges` | 832 | 153 | 25 | 150 | 150 | constraints 459, liveness 323, moves 178 |
| 1184 | 3176 | bat | `build_script_main::syntax_mapping::parse_glob` | 559 | 290 | 23 | 271 | 271 | constraints 637, liveness 204, moves 128 |
| 1151 | 3490 | bat | `bat::config::get_args_from_str` | 141 | 173 | 1 | 241 | 241 | constraints 952, liveness 52, moves 47 |
| 1137 | 2651 | bat | `bat::assets::<HighlightingAssets>::get_syntax` | 622 | 248 | 8 | 238 | 238 | constraints 512, liveness 213, moves 157 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 34005 | 7320 | bat | `bat::syntax_mapping::builtin::BUILTIN_MAPPINGS` | 12513 | 2209 | 0 | 1487 | 1487 | liveness 20240, constraints 7945, moves 3483 |
| 7073 | 9712 | bat | `bat::printer::<InteractivePrinter<'_>>::print_line` | 3680 | 1306 | 117 | 1419 | 1419 | constraints 3103, liveness 1915, moves 821 |
| 4796 | 15855 | bat | `bat::app::<App>::config` | 2440 | 1265 | 35 | 1173 | 1171 | constraints 2526, liveness 923, moves 527 |
| 4502 | 20551 | bat | `bat::clap_app::build_app` | 2667 | 1306 | 15 | 1037 | 1027 | constraints 2353, liveness 914, moves 591 |
| 2821 | 5818 | bat | `bat::get_languages` | 1189 | 572 | 46 | 556 | 556 | constraints 1610, liveness 524, moves 272 |
| 2329 | 6619 | bat | `bat::printer::<InteractivePrinter<'_>>::print_header` | 1350 | 570 | 39 | 583 | 583 | constraints 1172, liveness 435, moves 300 |
| 2077 | 3286 | bat | `bat::run` | 1584 | 565 | 44 | 497 | 495 | constraints 770, liveness 570, moves 307 |
| 2067 | 6316 | bat | `bat::line_range::<LineRange>::parse_range` | 1527 | 401 | 24 | 387 | 385 | constraints 1078, liveness 385, moves 264 |
| 1704 | 2151 | bat | `bat::controller::<Controller<'_>>::run_with_error_handler` | 797 | 286 | 32 | 342 | 342 | constraints 875, liveness 382, moves 174 |
| 1685 | 2573 | bat | `bat::list_themes` | 1156 | 406 | 39 | 391 | 391 | constraints 556, liveness 536, moves 259 |
| 1507 | 5450 | bat | `bat::app::<App>::inputs` | 517 | 478 | 14 | 531 | 519 | constraints 967, liveness 192, moves 127 |
| 1380 | 3685 | bat | `bat::input::<Input<'a>>::open` | 1027 | 299 | 21 | 271 | 271 | constraints 551, liveness 359, moves 218 |
| 1364 | 3141 | bat | `bat::printer::<InteractivePrinter<'a>>::new` | 824 | 356 | 13 | 378 | 376 | constraints 652, liveness 280, moves 173 |
| 1141 | 2322 | bat | `build_script_main::application::gen_man_and_comp` | 1000 | 444 | 30 | 350 | 350 | constraints 486, liveness 260, moves 173 |
| 1092 | 2287 | bat | `bat::output::<OutputType>::try_pager` | 800 | 318 | 34 | 250 | 250 | constraints 446, liveness 247, moves 157 |
| 1084 | 3176 | bat | `build_script_main::syntax_mapping::parse_glob` | 559 | 290 | 23 | 271 | 271 | constraints 592, liveness 197, moves 119 |
| 1069 | 1977 | bat | `bat::diff::get_git_diff` | 781 | 158 | 22 | 140 | 140 | constraints 448, liveness 295, moves 147 |
| 1064 | 3490 | bat | `bat::config::get_args_from_str` | 141 | 173 | 1 | 241 | 241 | constraints 900, liveness 47, moves 40 |
| 1058 | 1510 | bat | `bat::controller::<Controller<'_>>::print_file_ranges` | 832 | 153 | 25 | 150 | 150 | constraints 414, liveness 293, moves 161 |
| 995 | 2651 | bat | `bat::assets::<HighlightingAssets>::get_syntax` | 622 | 248 | 8 | 238 | 238 | constraints 445, liveness 203, moves 144 |
