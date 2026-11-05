| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| bat | polonius | 576 | 2983 | 285 | 255 | 0 | 89.5% | 550 | 106 | 3725 | 66175 |
| bat | nll | 576 | 2983 | 285 | 236 | 0 | 82.9% | 550 | 89 | 3500 | 64764 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 100 | 3 | 1 | 3 | 52 | 3 | 0 | 34 | 5 | 1 | 46 |
| nll | 94 | 3 | 1 | 3 | 49 | 2 | 0 | 31 | 0 | 0 | 46 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 66175 | 7606 | bat | `bat::syntax_mapping::builtin::BUILTIN_MAPPINGS` | 12513 | 2209 | 0 | 1487 | 1487 | walk 29358, liveness 22148, constraints 8741 |
| 9689 | 11049 | bat | `bat::printer::<InteractivePrinter<'_>>::print_line` | 3678 | 1306 | 117 | 1419 | 1419 | constraints 4111, liveness 2015, moves 1217 |
| 8203 | 16793 | bat | `bat::app::<App>::config` | 2440 | 1265 | 35 | 1173 | 1171 | constraints 4505, liveness 1360, moves 1033 |
| 7761 | 23152 | bat | `bat::clap_app::build_app` | 2667 | 1301 | 15 | 1037 | 1027 | constraints 3587, walk 1651, liveness 1055 |
| 3990 | 6031 | bat | `bat::get_languages` | 1187 | 572 | 46 | 556 | 556 | constraints 2085, liveness 646, moves 568 |
| 3725 | 3462 | bat | `bat::run` | 1584 | 565 | 44 | 497 | 495 | constraints 1520, liveness 724, walk 559 |
| 3622 | 7408 | bat | `bat::printer::<InteractivePrinter<'_>>::print_header` | 1350 | 570 | 39 | 583 | 583 | constraints 1748, moves 562, liveness 513 |
| 3284 | 2706 | bat | `bat::list_themes` | 1156 | 406 | 39 | 391 | 391 | constraints 1198, liveness 705, walk 516 |
| 3264 | 6467 | bat | `bat::line_range::<LineRange>::parse_range` | 1527 | 401 | 24 | 387 | 385 | constraints 1937, liveness 506, moves 366 |
| 3000 | 5691 | bat | `bat::app::<App>::inputs` | 517 | 478 | 14 | 531 | 519 | constraints 2039, moves 310, liveness 227 |
| 2719 | 3782 | bat | `bat::input::<Input<'a>>::open` | 1027 | 299 | 21 | 271 | 271 | constraints 1371, moves 488, liveness 482 |
| 2619 | 2246 | bat | `bat::controller::<Controller<'_>>::run_with_error_handler` | 797 | 286 | 32 | 342 | 342 | constraints 1082, liveness 500, moves 401 |
| 2598 | 2077 | bat | `bat::diff::get_git_diff` | 781 | 158 | 22 | 140 | 140 | constraints 1139, liveness 628, moves 340 |
| 2368 | 1862 | bat | `snapshot_tests::tester::create_sample_directory` | 579 | 189 | 12 | 165 | 165 | constraints 1134, liveness 496, moves 302 |
| 2275 | 3398 | bat | `bat::printer::<InteractivePrinter<'a>>::new` | 824 | 356 | 13 | 378 | 376 | constraints 1110, moves 388, liveness 377 |
| 2074 | 2541 | bat | `build_script_main::application::gen_man_and_comp` | 1000 | 444 | 30 | 350 | 350 | constraints 1092, liveness 347, moves 321 |
| 1928 | 2888 | bat | `bat::assets::<HighlightingAssets>::get_syntax` | 622 | 248 | 8 | 238 | 238 | constraints 880, moves 360, liveness 293 |
| 1903 | 2361 | bat | `bat::output::<OutputType>::try_pager` | 799 | 318 | 34 | 250 | 250 | constraints 898, liveness 347, moves 307 |
| 1790 | 1597 | bat | `bat::controller::<Controller<'_>>::print_file_ranges` | 832 | 153 | 25 | 150 | 150 | constraints 752, liveness 337, moves 309 |
| 1679 | 3591 | bat | `build_script_main::syntax_mapping::parse_glob` | 559 | 290 | 23 | 271 | 271 | constraints 957, moves 271, liveness 216 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 64764 | 7606 | bat | `bat::syntax_mapping::builtin::BUILTIN_MAPPINGS` | 12513 | 2209 | 0 | 1487 | 1487 | walk 29301, liveness 21232, constraints 8675 |
| 9184 | 11049 | bat | `bat::printer::<InteractivePrinter<'_>>::print_line` | 3678 | 1306 | 117 | 1419 | 1419 | constraints 4191, liveness 1985, moves 1166 |
| 7894 | 16793 | bat | `bat::app::<App>::config` | 2440 | 1265 | 35 | 1173 | 1171 | constraints 4399, liveness 1273, moves 1019 |
| 7470 | 23152 | bat | `bat::clap_app::build_app` | 2667 | 1301 | 15 | 1037 | 1027 | constraints 3478, walk 1673, liveness 964 |
| 3866 | 6031 | bat | `bat::get_languages` | 1187 | 572 | 46 | 556 | 556 | constraints 2085, liveness 644, moves 560 |
| 3500 | 3462 | bat | `bat::run` | 1584 | 565 | 44 | 497 | 495 | constraints 1476, liveness 730, walk 553 |
| 3444 | 7408 | bat | `bat::printer::<InteractivePrinter<'_>>::print_header` | 1350 | 570 | 39 | 583 | 583 | constraints 1662, liveness 544, moves 526 |
| 3091 | 6467 | bat | `bat::line_range::<LineRange>::parse_range` | 1527 | 401 | 24 | 387 | 385 | constraints 1889, liveness 445, moves 343 |
| 2997 | 2706 | bat | `bat::list_themes` | 1156 | 406 | 39 | 391 | 391 | constraints 1145, liveness 688, walk 513 |
| 2938 | 5691 | bat | `bat::app::<App>::inputs` | 517 | 478 | 14 | 531 | 519 | constraints 2041, moves 310, walk 217 |
| 2553 | 3782 | bat | `bat::input::<Input<'a>>::open` | 1027 | 299 | 21 | 271 | 271 | constraints 1301, moves 467, liveness 465 |
| 2418 | 2077 | bat | `bat::diff::get_git_diff` | 781 | 158 | 22 | 140 | 140 | constraints 1104, liveness 594, walk 310 |
| 2291 | 2246 | bat | `bat::controller::<Controller<'_>>::run_with_error_handler` | 797 | 286 | 32 | 342 | 342 | constraints 1025, liveness 510, moves 358 |
| 2155 | 1862 | bat | `snapshot_tests::tester::create_sample_directory` | 579 | 189 | 12 | 165 | 165 | constraints 1076, liveness 468, moves 257 |
| 2150 | 3398 | bat | `bat::printer::<InteractivePrinter<'a>>::new` | 824 | 356 | 13 | 378 | 376 | constraints 1065, moves 371, liveness 338 |
| 1886 | 2541 | bat | `build_script_main::application::gen_man_and_comp` | 1000 | 444 | 30 | 350 | 350 | constraints 1044, liveness 281, moves 263 |
| 1805 | 2361 | bat | `bat::output::<OutputType>::try_pager` | 799 | 318 | 34 | 250 | 250 | constraints 873, liveness 339, moves 298 |
| 1764 | 2888 | bat | `bat::assets::<HighlightingAssets>::get_syntax` | 622 | 248 | 8 | 238 | 238 | constraints 810, moves 301, liveness 263 |
| 1626 | 1597 | bat | `bat::controller::<Controller<'_>>::print_file_ranges` | 832 | 153 | 25 | 150 | 150 | constraints 711, liveness 325, moves 247 |
| 1582 | 3591 | bat | `build_script_main::syntax_mapping::parse_glob` | 559 | 290 | 23 | 271 | 271 | constraints 894, moves 241, liveness 239 |
