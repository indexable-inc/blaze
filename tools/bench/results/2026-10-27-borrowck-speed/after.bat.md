| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| bat | polonius | 576 | 2880 | 288 | 234 | 0 | 81.3% | 545 | 91 | 3586 | 64304 |
| bat | nll | 576 | 2880 | 288 | 214 | 0 | 74.4% | 545 | 75 | 3227 | 63947 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 81 | 3 | 1 | 3 | 51 | 3 | 0 | 34 | 5 | 0 | 46 |
| nll | 74 | 3 | 1 | 3 | 48 | 2 | 0 | 31 | 0 | 0 | 45 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 64304 | 7736 | bat | `bat::syntax_mapping::builtin::BUILTIN_MAPPINGS` | 12513 | 2209 | 0 | 1487 | 1487 | walk 28704, liveness 21134, constraints 8431 |
| 9302 | 11486 | bat | `bat::printer::<InteractivePrinter<'_>>::print_line` | 3678 | 1306 | 117 | 1419 | 1419 | constraints 3853, liveness 1974, moves 1138 |
| 7938 | 18006 | bat | `bat::app::<App>::config` | 2440 | 1265 | 35 | 1173 | 1171 | constraints 4215, liveness 1252, moves 1131 |
| 7393 | 23353 | bat | `bat::clap_app::build_app` | 2667 | 1306 | 15 | 1037 | 1027 | constraints 3280, walk 1679, liveness 1005 |
| 3982 | 6695 | bat | `bat::get_languages` | 1187 | 572 | 46 | 556 | 556 | constraints 2045, liveness 646, moves 587 |
| 3586 | 3691 | bat | `bat::run` | 1584 | 565 | 44 | 497 | 495 | constraints 1295, liveness 736, walk 626 |
| 3412 | 7438 | bat | `bat::printer::<InteractivePrinter<'_>>::print_header` | 1350 | 570 | 39 | 583 | 583 | constraints 1504, moves 556, liveness 544 |
| 2985 | 2830 | bat | `bat::list_themes` | 1156 | 406 | 39 | 391 | 391 | constraints 885, liveness 747, walk 515 |
| 2926 | 6531 | bat | `bat::line_range::<LineRange>::parse_range` | 1527 | 401 | 24 | 387 | 385 | constraints 1625, liveness 456, moves 373 |
| 2822 | 5946 | bat | `bat::app::<App>::inputs` | 517 | 478 | 14 | 531 | 519 | constraints 1835, moves 340, walk 213 |
| 2440 | 2224 | bat | `bat::controller::<Controller<'_>>::run_with_error_handler` | 797 | 286 | 32 | 342 | 342 | constraints 934, liveness 488, moves 385 |
| 2389 | 2188 | bat | `bat::diff::get_git_diff` | 781 | 158 | 22 | 140 | 140 | constraints 885, liveness 606, moves 360 |
| 2262 | 3895 | bat | `bat::input::<Input<'a>>::open` | 1027 | 299 | 21 | 271 | 271 | constraints 980, moves 484, liveness 421 |
| 2099 | 3372 | bat | `bat::printer::<InteractivePrinter<'a>>::new` | 824 | 356 | 13 | 378 | 376 | constraints 987, moves 384, liveness 330 |
| 1907 | 1907 | bat | `snapshot_tests::tester::create_sample_directory` | 579 | 189 | 12 | 165 | 165 | constraints 761, liveness 460, moves 284 |
| 1765 | 2788 | bat | `bat::assets::<HighlightingAssets>::get_syntax` | 622 | 248 | 8 | 238 | 238 | constraints 760, moves 321, liveness 290 |
| 1648 | 2580 | bat | `build_script_main::application::gen_man_and_comp` | 1000 | 444 | 30 | 350 | 350 | constraints 737, liveness 308, moves 277 |
| 1599 | 1571 | bat | `bat::controller::<Controller<'_>>::print_file_ranges` | 832 | 153 | 25 | 150 | 150 | constraints 603, liveness 330, moves 276 |
| 1556 | 3419 | bat | `build_script_main::syntax_mapping::parse_glob` | 559 | 290 | 23 | 271 | 271 | constraints 847, moves 261, liveness 216 |
| 1550 | 2482 | bat | `bat::output::<OutputType>::try_pager` | 799 | 318 | 34 | 250 | 250 | constraints 587, liveness 324, moves 300 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 63947 | 7736 | bat | `bat::syntax_mapping::builtin::BUILTIN_MAPPINGS` | 12513 | 2209 | 0 | 1487 | 1487 | walk 28876, liveness 20795, constraints 8506 |
| 8694 | 11486 | bat | `bat::printer::<InteractivePrinter<'_>>::print_line` | 3678 | 1306 | 117 | 1419 | 1419 | constraints 3804, liveness 1972, moves 1097 |
| 7501 | 18006 | bat | `bat::app::<App>::config` | 2440 | 1265 | 35 | 1173 | 1171 | constraints 4018, liveness 1225, moves 991 |
| 7210 | 23353 | bat | `bat::clap_app::build_app` | 2667 | 1306 | 15 | 1037 | 1027 | constraints 3196, walk 1668, liveness 1014 |
| 3654 | 6695 | bat | `bat::get_languages` | 1187 | 572 | 46 | 556 | 556 | constraints 1925, liveness 604, moves 535 |
| 3227 | 3691 | bat | `bat::run` | 1584 | 565 | 44 | 497 | 495 | constraints 1213, liveness 703, walk 570 |
| 3188 | 7438 | bat | `bat::printer::<InteractivePrinter<'_>>::print_header` | 1350 | 570 | 39 | 583 | 583 | constraints 1430, moves 519, liveness 513 |
| 2724 | 6531 | bat | `bat::line_range::<LineRange>::parse_range` | 1527 | 401 | 24 | 387 | 385 | constraints 1575, liveness 411, moves 335 |
| 2713 | 2830 | bat | `bat::list_themes` | 1156 | 406 | 39 | 391 | 391 | constraints 838, liveness 691, walk 504 |
| 2606 | 5946 | bat | `bat::app::<App>::inputs` | 517 | 478 | 14 | 531 | 519 | constraints 1710, moves 327, walk 205 |
| 2092 | 2224 | bat | `bat::controller::<Controller<'_>>::run_with_error_handler` | 797 | 286 | 32 | 342 | 342 | constraints 892, liveness 437, moves 372 |
| 2069 | 3895 | bat | `bat::input::<Input<'a>>::open` | 1027 | 299 | 21 | 271 | 271 | constraints 898, moves 441, liveness 403 |
| 2053 | 2188 | bat | `bat::diff::get_git_diff` | 781 | 158 | 22 | 140 | 140 | constraints 773, liveness 526, walk 324 |
| 1953 | 3372 | bat | `bat::printer::<InteractivePrinter<'a>>::new` | 824 | 356 | 13 | 378 | 376 | constraints 916, moves 348, liveness 331 |
| 1646 | 1907 | bat | `snapshot_tests::tester::create_sample_directory` | 579 | 189 | 12 | 165 | 165 | constraints 637, liveness 424, moves 257 |
| 1613 | 2788 | bat | `bat::assets::<HighlightingAssets>::get_syntax` | 622 | 248 | 8 | 238 | 238 | constraints 688, moves 300, liveness 270 |
| 1510 | 1571 | bat | `bat::controller::<Controller<'_>>::print_file_ranges` | 832 | 153 | 25 | 150 | 150 | constraints 568, liveness 355, moves 241 |
| 1509 | 2580 | bat | `build_script_main::application::gen_man_and_comp` | 1000 | 444 | 30 | 350 | 350 | constraints 671, liveness 316, moves 265 |
| 1444 | 3419 | bat | `build_script_main::syntax_mapping::parse_glob` | 559 | 290 | 23 | 271 | 271 | constraints 781, moves 256, liveness 215 |
| 1430 | 2482 | bat | `bat::output::<OutputType>::try_pager` | 799 | 318 | 34 | 250 | 250 | constraints 512, liveness 321, moves 303 |
