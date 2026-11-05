| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| bat | polonius | 576 | 2764 | 271 | 228 | 0 | 83.9% | 504 | 92 | 3195 | 63350 |
| bat | nll | 576 | 2764 | 271 | 210 | 0 | 77.3% | 504 | 76 | 3001 | 62941 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 77 | 3 | 1 | 3 | 50 | 3 | 0 | 33 | 5 | 1 | 45 |
| nll | 71 | 3 | 1 | 3 | 48 | 2 | 0 | 30 | 0 | 0 | 44 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 63350 | 7122 | bat | `bat::syntax_mapping::builtin::BUILTIN_MAPPINGS` | 12513 | 2209 | 0 | 1487 | 1487 | walk 28021, liveness 21285, constraints 8209 |
| 8662 | 10105 | bat | `bat::printer::<InteractivePrinter<'_>>::print_line` | 3678 | 1306 | 117 | 1419 | 1419 | constraints 3270, liveness 2046, moves 1085 |
| 7219 | 16634 | bat | `bat::app::<App>::config` | 2440 | 1265 | 35 | 1173 | 1171 | constraints 3774, liveness 1188, moves 972 |
| 6762 | 21431 | bat | `bat::clap_app::build_app` | 2667 | 1306 | 15 | 1037 | 1027 | constraints 2835, walk 1622, liveness 954 |
| 3628 | 6057 | bat | `bat::get_languages` | 1187 | 572 | 46 | 556 | 556 | constraints 1826, liveness 601, moves 525 |
| 3195 | 7056 | bat | `bat::printer::<InteractivePrinter<'_>>::print_header` | 1350 | 570 | 39 | 583 | 583 | constraints 1350, moves 551, liveness 519 |
| 3181 | 3276 | bat | `bat::run` | 1584 | 565 | 44 | 497 | 495 | constraints 1084, liveness 689, walk 559 |
| 2832 | 5946 | bat | `bat::line_range::<LineRange>::parse_range` | 1527 | 401 | 24 | 387 | 385 | constraints 1555, liveness 454, moves 364 |
| 2761 | 2597 | bat | `bat::list_themes` | 1156 | 406 | 39 | 391 | 391 | constraints 807, liveness 678, walk 506 |
| 2608 | 5531 | bat | `bat::app::<App>::inputs` | 517 | 478 | 14 | 531 | 519 | constraints 1682, moves 316, liveness 207 |
| 2359 | 2109 | bat | `bat::controller::<Controller<'_>>::run_with_error_handler` | 797 | 286 | 32 | 342 | 342 | constraints 892, liveness 480, moves 385 |
| 2215 | 1923 | bat | `bat::diff::get_git_diff` | 781 | 158 | 22 | 140 | 140 | constraints 763, liveness 658, moves 315 |
| 2116 | 3648 | bat | `bat::input::<Input<'a>>::open` | 1027 | 299 | 21 | 271 | 271 | constraints 872, moves 456, liveness 426 |
| 2008 | 3160 | bat | `bat::printer::<InteractivePrinter<'a>>::new` | 824 | 356 | 13 | 378 | 376 | constraints 902, moves 355, liveness 354 |
| 1726 | 1763 | bat | `snapshot_tests::tester::create_sample_directory` | 579 | 189 | 12 | 165 | 165 | constraints 655, liveness 411, moves 257 |
| 1685 | 2641 | bat | `bat::assets::<HighlightingAssets>::get_syntax` | 622 | 248 | 8 | 238 | 238 | constraints 715, moves 312, liveness 271 |
| 1571 | 2382 | bat | `build_script_main::application::gen_man_and_comp` | 1000 | 444 | 30 | 350 | 350 | constraints 697, liveness 283, moves 272 |
| 1550 | 1500 | bat | `bat::controller::<Controller<'_>>::print_file_ranges` | 832 | 153 | 25 | 150 | 150 | constraints 575, liveness 339, moves 267 |
| 1550 | 3436 | bat | `build_script_main::syntax_mapping::parse_glob` | 559 | 290 | 23 | 271 | 271 | constraints 810, moves 291, liveness 205 |
| 1520 | 2270 | bat | `bat::output::<OutputType>::try_pager` | 799 | 318 | 34 | 250 | 250 | constraints 539, liveness 315, moves 302 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 62941 | 7122 | bat | `bat::syntax_mapping::builtin::BUILTIN_MAPPINGS` | 12513 | 2209 | 0 | 1487 | 1487 | walk 27808, liveness 21002, constraints 8263 |
| 8172 | 10105 | bat | `bat::printer::<InteractivePrinter<'_>>::print_line` | 3678 | 1306 | 117 | 1419 | 1419 | constraints 3205, liveness 2054, moves 1132 |
| 6910 | 16634 | bat | `bat::app::<App>::config` | 2440 | 1265 | 35 | 1173 | 1171 | constraints 3587, liveness 1201, moves 953 |
| 6614 | 21431 | bat | `bat::clap_app::build_app` | 2667 | 1306 | 15 | 1037 | 1027 | constraints 2754, walk 1622, liveness 989 |
| 3408 | 6057 | bat | `bat::get_languages` | 1187 | 572 | 46 | 556 | 556 | constraints 1750, liveness 579, moves 503 |
| 3001 | 7056 | bat | `bat::printer::<InteractivePrinter<'_>>::print_header` | 1350 | 570 | 39 | 583 | 583 | constraints 1262, moves 523, liveness 506 |
| 2890 | 3276 | bat | `bat::run` | 1584 | 565 | 44 | 497 | 495 | constraints 1008, liveness 649, walk 545 |
| 2616 | 5946 | bat | `bat::line_range::<LineRange>::parse_range` | 1527 | 401 | 24 | 387 | 385 | constraints 1472, liveness 412, moves 326 |
| 2546 | 2597 | bat | `bat::list_themes` | 1156 | 406 | 39 | 391 | 391 | constraints 757, liveness 656, walk 517 |
| 2508 | 5531 | bat | `bat::app::<App>::inputs` | 517 | 478 | 14 | 531 | 519 | constraints 1658, moves 293, liveness 199 |
| 2061 | 2109 | bat | `bat::controller::<Controller<'_>>::run_with_error_handler` | 797 | 286 | 32 | 342 | 342 | constraints 828, liveness 450, moves 361 |
| 2015 | 3648 | bat | `bat::input::<Input<'a>>::open` | 1027 | 299 | 21 | 271 | 271 | constraints 840, moves 453, liveness 410 |
| 1934 | 1923 | bat | `bat::diff::get_git_diff` | 781 | 158 | 22 | 140 | 140 | constraints 740, liveness 501, walk 291 |
| 1891 | 3160 | bat | `bat::printer::<InteractivePrinter<'a>>::new` | 824 | 356 | 13 | 378 | 376 | constraints 875, moves 339, liveness 330 |
| 1559 | 1763 | bat | `snapshot_tests::tester::create_sample_directory` | 579 | 189 | 12 | 165 | 165 | constraints 596, liveness 392, moves 248 |
| 1556 | 2641 | bat | `bat::assets::<HighlightingAssets>::get_syntax` | 622 | 248 | 8 | 238 | 238 | constraints 638, moves 294, liveness 269 |
| 1484 | 1500 | bat | `bat::controller::<Controller<'_>>::print_file_ranges` | 832 | 153 | 25 | 150 | 150 | constraints 555, liveness 309, moves 244 |
| 1424 | 2382 | bat | `build_script_main::application::gen_man_and_comp` | 1000 | 444 | 30 | 350 | 350 | constraints 631, liveness 270, moves 260 |
| 1418 | 2270 | bat | `bat::output::<OutputType>::try_pager` | 799 | 318 | 34 | 250 | 250 | constraints 512, liveness 329, moves 290 |
| 1378 | 3436 | bat | `build_script_main::syntax_mapping::parse_glob` | 559 | 290 | 23 | 271 | 271 | constraints 757, moves 239, liveness 193 |
