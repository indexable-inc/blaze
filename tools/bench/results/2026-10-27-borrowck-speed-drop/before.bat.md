| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| bat | polonius | 576 | 2714 | 265 | 196 | 0 | 73.8% | 488 | 73 | 2855 | 61435 |
| bat | nll | 576 | 2714 | 265 | 179 | 0 | 67.4% | 488 | 59 | 2678 | 60918 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 61 | 3 | 1 | 3 | 48 | 2 | 0 | 20 | 4 | 0 | 46 |
| nll | 55 | 3 | 1 | 3 | 45 | 2 | 0 | 18 | 0 | 0 | 45 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 61435 | 6873 | bat | `bat::syntax_mapping::builtin::BUILTIN_MAPPINGS` | 12513 | 2209 | 0 | 1487 | 1487 | walk 27528, liveness 21022, constraints 7880 |
| 8124 | 9637 | bat | `bat::printer::<InteractivePrinter<'_>>::print_line` | 3680 | 1306 | 117 | 1419 | 1419 | constraints 3024, liveness 1950, walk 1083 |
| 6047 | 20354 | bat | `bat::clap_app::build_app` | 2667 | 1306 | 15 | 1037 | 1027 | constraints 2428, walk 1644, liveness 935 |
| 5807 | 15630 | bat | `bat::app::<App>::config` | 2440 | 1265 | 35 | 1173 | 1171 | constraints 2589, liveness 1074, walk 967 |
| 3202 | 5615 | bat | `bat::get_languages` | 1189 | 572 | 46 | 556 | 556 | constraints 1665, liveness 551, walk 370 |
| 2855 | 6629 | bat | `bat::printer::<InteractivePrinter<'_>>::print_header` | 1350 | 570 | 39 | 583 | 583 | constraints 1239, walk 501, liveness 494 |
| 2794 | 3218 | bat | `bat::run` | 1584 | 565 | 44 | 497 | 495 | constraints 817, liveness 649, walk 604 |
| 2414 | 2525 | bat | `bat::list_themes` | 1156 | 406 | 39 | 391 | 391 | liveness 616, constraints 614, walk 554 |
| 2294 | 5707 | bat | `bat::line_range::<LineRange>::parse_range` | 1527 | 401 | 24 | 387 | 385 | constraints 1158, liveness 412, moves 277 |
| 2135 | 2193 | bat | `bat::controller::<Controller<'_>>::run_with_error_handler` | 797 | 286 | 32 | 342 | 342 | constraints 874, liveness 417, walk 236 |
| 1957 | 5379 | bat | `bat::app::<App>::inputs` | 517 | 478 | 14 | 531 | 519 | constraints 1085, walk 336, liveness 193 |
| 1749 | 1921 | bat | `bat::diff::get_git_diff` | 781 | 158 | 22 | 140 | 140 | liveness 539, constraints 520, walk 352 |
| 1638 | 3178 | bat | `bat::printer::<InteractivePrinter<'a>>::new` | 824 | 356 | 13 | 378 | 376 | constraints 699, liveness 336, moves 196 |
| 1634 | 3599 | bat | `bat::input::<Input<'a>>::open` | 1027 | 299 | 21 | 271 | 271 | constraints 621, liveness 383, moves 242 |
| 1324 | 1664 | bat | `snapshot_tests::tester::create_sample_directory` | 579 | 189 | 12 | 165 | 165 | constraints 447, liveness 342, walk 271 |
| 1316 | 1462 | bat | `bat::controller::<Controller<'_>>::print_file_ranges` | 832 | 153 | 25 | 150 | 150 | constraints 447, liveness 331, walk 222 |
| 1308 | 2276 | bat | `build_script_main::application::gen_man_and_comp` | 1000 | 444 | 30 | 350 | 350 | constraints 529, liveness 302, moves 180 |
| 1307 | 2687 | bat | `bat::assets::<HighlightingAssets>::get_syntax` | 622 | 248 | 8 | 238 | 238 | constraints 513, liveness 262, walk 231 |
| 1291 | 2216 | bat | `bat::output::<OutputType>::try_pager` | 800 | 318 | 34 | 250 | 250 | constraints 485, liveness 277, walk 174 |
| 1197 | 3432 | bat | `bat::config::get_args_from_str` | 141 | 173 | 1 | 241 | 241 | constraints 944, walk 68, liveness 52 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 60918 | 6873 | bat | `bat::syntax_mapping::builtin::BUILTIN_MAPPINGS` | 12513 | 2209 | 0 | 1487 | 1487 | walk 27571, liveness 20718, constraints 7864 |
| 7690 | 9637 | bat | `bat::printer::<InteractivePrinter<'_>>::print_line` | 3680 | 1306 | 117 | 1419 | 1419 | constraints 3001, liveness 2022, walk 1076 |
| 5920 | 20354 | bat | `bat::clap_app::build_app` | 2667 | 1306 | 15 | 1037 | 1027 | constraints 2396, walk 1653, liveness 910 |
| 5608 | 15630 | bat | `bat::app::<App>::config` | 2440 | 1265 | 35 | 1173 | 1171 | constraints 2568, liveness 1031, walk 904 |
| 2985 | 5615 | bat | `bat::get_languages` | 1189 | 572 | 46 | 556 | 556 | constraints 1585, liveness 540, walk 355 |
| 2678 | 6629 | bat | `bat::printer::<InteractivePrinter<'_>>::print_header` | 1350 | 570 | 39 | 583 | 583 | constraints 1173, walk 494, liveness 474 |
| 2546 | 3218 | bat | `bat::run` | 1584 | 565 | 44 | 497 | 495 | constraints 776, liveness 639, walk 571 |
| 2260 | 2525 | bat | `bat::list_themes` | 1156 | 406 | 39 | 391 | 391 | liveness 648, constraints 582, walk 559 |
| 2145 | 5707 | bat | `bat::line_range::<LineRange>::parse_range` | 1527 | 401 | 24 | 387 | 385 | constraints 1054, liveness 418, moves 278 |
| 1799 | 2193 | bat | `bat::controller::<Controller<'_>>::run_with_error_handler` | 797 | 286 | 32 | 342 | 342 | constraints 839, liveness 382, walk 217 |
| 1785 | 5379 | bat | `bat::app::<App>::inputs` | 517 | 478 | 14 | 531 | 519 | constraints 1011, walk 307, liveness 188 |
| 1501 | 3599 | bat | `bat::input::<Input<'a>>::open` | 1027 | 299 | 21 | 271 | 271 | constraints 565, liveness 371, moves 225 |
| 1479 | 1921 | bat | `bat::diff::get_git_diff` | 781 | 158 | 22 | 140 | 140 | constraints 482, liveness 398, walk 315 |
| 1476 | 3178 | bat | `bat::printer::<InteractivePrinter<'a>>::new` | 824 | 356 | 13 | 378 | 376 | constraints 675, liveness 303, moves 174 |
| 1251 | 1462 | bat | `bat::controller::<Controller<'_>>::print_file_ranges` | 832 | 153 | 25 | 150 | 150 | constraints 420, liveness 334, walk 216 |
| 1186 | 2216 | bat | `bat::output::<OutputType>::try_pager` | 800 | 318 | 34 | 250 | 250 | constraints 433, liveness 268, walk 166 |
| 1181 | 2687 | bat | `bat::assets::<HighlightingAssets>::get_syntax` | 622 | 248 | 8 | 238 | 238 | constraints 452, liveness 253, walk 213 |
| 1178 | 1664 | bat | `snapshot_tests::tester::create_sample_directory` | 579 | 189 | 12 | 165 | 165 | constraints 382, liveness 302, walk 264 |
| 1172 | 2276 | bat | `build_script_main::application::gen_man_and_comp` | 1000 | 444 | 30 | 350 | 350 | constraints 498, liveness 259, moves 175 |
| 1109 | 3432 | bat | `bat::config::get_args_from_str` | 141 | 173 | 1 | 241 | 241 | constraints 904, walk 63, liveness 49 |
