| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| bat | polonius | 576 | 2816 | 278 | 198 | 0 | 71.3% | 524 | 73 | 2981 | 62759 |
| bat | nll | 576 | 2816 | 278 | 181 | 0 | 65.1% | 524 | 59 | 2706 | 62778 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 62 | 3 | 1 | 3 | 47 | 3 | 0 | 20 | 5 | 0 | 47 |
| nll | 56 | 3 | 1 | 3 | 45 | 2 | 0 | 18 | 0 | 0 | 45 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 62759 | 7327 | bat | `bat::syntax_mapping::builtin::BUILTIN_MAPPINGS` | 12513 | 2209 | 0 | 1487 | 1487 | walk 28423, liveness 20570, constraints 8243 |
| 8213 | 10295 | bat | `bat::printer::<InteractivePrinter<'_>>::print_line` | 3678 | 1306 | 117 | 1419 | 1419 | constraints 3285, liveness 1878, walk 1064 |
| 6150 | 22518 | bat | `bat::clap_app::build_app` | 2667 | 1306 | 15 | 1037 | 1027 | constraints 2507, walk 1620, liveness 945 |
| 5940 | 17281 | bat | `bat::app::<App>::config` | 2440 | 1265 | 35 | 1173 | 1171 | constraints 2803, liveness 1039, walk 931 |
| 3338 | 6064 | bat | `bat::get_languages` | 1187 | 572 | 46 | 556 | 556 | constraints 1747, liveness 585, walk 357 |
| 2981 | 7262 | bat | `bat::printer::<InteractivePrinter<'_>>::print_header` | 1350 | 570 | 39 | 583 | 583 | constraints 1266, liveness 569, walk 494 |
| 2769 | 3341 | bat | `bat::run` | 1584 | 565 | 44 | 497 | 495 | constraints 853, liveness 635, walk 575 |
| 2514 | 2662 | bat | `bat::list_themes` | 1156 | 406 | 39 | 391 | 391 | liveness 672, constraints 659, walk 556 |
| 2383 | 6227 | bat | `bat::line_range::<LineRange>::parse_range` | 1527 | 401 | 24 | 387 | 385 | constraints 1194, liveness 436, moves 291 |
| 2099 | 2185 | bat | `bat::controller::<Controller<'_>>::run_with_error_handler` | 797 | 286 | 32 | 342 | 342 | constraints 890, liveness 414, walk 224 |
| 2003 | 5785 | bat | `bat::app::<App>::inputs` | 517 | 478 | 14 | 531 | 519 | constraints 1138, walk 320, liveness 199 |
| 1807 | 1935 | bat | `bat::diff::get_git_diff` | 781 | 158 | 22 | 140 | 140 | constraints 610, liveness 458, walk 393 |
| 1687 | 3261 | bat | `bat::printer::<InteractivePrinter<'a>>::new` | 824 | 356 | 13 | 378 | 376 | constraints 772, liveness 333, moves 192 |
| 1658 | 3701 | bat | `bat::input::<Input<'a>>::open` | 1027 | 299 | 21 | 271 | 271 | constraints 618, liveness 403, moves 241 |
| 1361 | 1545 | bat | `bat::controller::<Controller<'_>>::print_file_ranges` | 832 | 153 | 25 | 150 | 150 | constraints 460, liveness 332, walk 251 |
| 1351 | 1820 | bat | `snapshot_tests::tester::create_sample_directory` | 579 | 189 | 12 | 165 | 165 | constraints 460, liveness 337, walk 278 |
| 1346 | 2437 | bat | `bat::output::<OutputType>::try_pager` | 799 | 318 | 34 | 250 | 250 | constraints 503, liveness 309, walk 175 |
| 1333 | 2414 | bat | `build_script_main::application::gen_man_and_comp` | 1000 | 444 | 30 | 350 | 350 | constraints 554, liveness 294, moves 183 |
| 1293 | 2736 | bat | `bat::assets::<HighlightingAssets>::get_syntax` | 622 | 248 | 8 | 238 | 238 | constraints 517, liveness 255, walk 219 |
| 1215 | 3341 | bat | `build_script_main::syntax_mapping::parse_glob` | 559 | 290 | 23 | 271 | 271 | constraints 655, liveness 208, moves 124 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 62778 | 7327 | bat | `bat::syntax_mapping::builtin::BUILTIN_MAPPINGS` | 12513 | 2209 | 0 | 1487 | 1487 | walk 28531, liveness 20365, constraints 8467 |
| 7599 | 10295 | bat | `bat::printer::<InteractivePrinter<'_>>::print_line` | 3678 | 1306 | 117 | 1419 | 1419 | constraints 3132, liveness 1886, walk 1076 |
| 5972 | 22518 | bat | `bat::clap_app::build_app` | 2667 | 1306 | 15 | 1037 | 1027 | constraints 2395, walk 1599, liveness 933 |
| 5574 | 17281 | bat | `bat::app::<App>::config` | 2440 | 1265 | 35 | 1173 | 1171 | constraints 2639, liveness 1005, walk 879 |
| 3076 | 6064 | bat | `bat::get_languages` | 1187 | 572 | 46 | 556 | 556 | constraints 1637, liveness 577, walk 346 |
| 2706 | 7262 | bat | `bat::printer::<InteractivePrinter<'_>>::print_header` | 1350 | 570 | 39 | 583 | 583 | constraints 1201, walk 487, liveness 478 |
| 2515 | 3341 | bat | `bat::run` | 1584 | 565 | 44 | 497 | 495 | constraints 806, liveness 602, walk 552 |
| 2222 | 2662 | bat | `bat::list_themes` | 1156 | 406 | 39 | 391 | 391 | liveness 602, constraints 596, walk 547 |
| 2155 | 6227 | bat | `bat::line_range::<LineRange>::parse_range` | 1527 | 401 | 24 | 387 | 385 | constraints 1090, liveness 413, moves 255 |
| 1877 | 5785 | bat | `bat::app::<App>::inputs` | 517 | 478 | 14 | 531 | 519 | constraints 1043, walk 346, liveness 193 |
| 1830 | 2185 | bat | `bat::controller::<Controller<'_>>::run_with_error_handler` | 797 | 286 | 32 | 342 | 342 | constraints 841, liveness 390, walk 260 |
| 1495 | 3701 | bat | `bat::input::<Input<'a>>::open` | 1027 | 299 | 21 | 271 | 271 | constraints 560, liveness 363, moves 228 |
| 1480 | 1935 | bat | `bat::diff::get_git_diff` | 781 | 158 | 22 | 140 | 140 | constraints 508, liveness 383, walk 327 |
| 1476 | 3261 | bat | `bat::printer::<InteractivePrinter<'a>>::new` | 824 | 356 | 13 | 378 | 376 | constraints 683, liveness 304, walk 170 |
| 1237 | 2414 | bat | `build_script_main::application::gen_man_and_comp` | 1000 | 444 | 30 | 350 | 350 | constraints 517, liveness 293, moves 184 |
| 1235 | 1545 | bat | `bat::controller::<Controller<'_>>::print_file_ranges` | 832 | 153 | 25 | 150 | 150 | constraints 438, liveness 303, walk 212 |
| 1218 | 1820 | bat | `snapshot_tests::tester::create_sample_directory` | 579 | 189 | 12 | 165 | 165 | constraints 434, liveness 315, walk 252 |
| 1210 | 2736 | bat | `bat::assets::<HighlightingAssets>::get_syntax` | 622 | 248 | 8 | 238 | 238 | constraints 448, liveness 277, walk 211 |
| 1198 | 2437 | bat | `bat::output::<OutputType>::try_pager` | 799 | 318 | 34 | 250 | 250 | constraints 447, liveness 268, moves 166 |
| 1108 | 3559 | bat | `bat::config::get_args_from_str` | 141 | 173 | 1 | 241 | 241 | constraints 907, walk 61, liveness 49 |
