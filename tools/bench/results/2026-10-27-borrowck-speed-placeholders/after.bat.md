| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| bat | polonius | 576 | 2705 | 263 | 195 | 0 | 74.0% | 500 | 71 | 2840 | 62597 |
| bat | nll | 576 | 2705 | 263 | 180 | 0 | 68.4% | 500 | 58 | 2652 | 63257 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 59 | 3 | 1 | 3 | 48 | 2 | 0 | 20 | 4 | 0 | 46 |
| nll | 54 | 3 | 1 | 3 | 46 | 2 | 0 | 18 | 0 | 0 | 46 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 62597 | 6954 | bat | `bat::syntax_mapping::builtin::BUILTIN_MAPPINGS` | 12513 | 2209 | 0 | 1487 | 1487 | walk 28510, liveness 20980, constraints 7987 |
| 8213 | 9851 | bat | `bat::printer::<InteractivePrinter<'_>>::print_line` | 3680 | 1306 | 117 | 1419 | 1419 | constraints 3092, liveness 2036, walk 1086 |
| 6288 | 20423 | bat | `bat::clap_app::build_app` | 2667 | 1306 | 15 | 1037 | 1027 | constraints 2418, walk 1631, liveness 1158 |
| 5735 | 15365 | bat | `bat::app::<App>::config` | 2440 | 1265 | 35 | 1173 | 1171 | constraints 2577, liveness 1109, walk 902 |
| 3176 | 5575 | bat | `bat::get_languages` | 1189 | 572 | 46 | 556 | 556 | constraints 1583, liveness 614, walk 353 |
| 2840 | 6777 | bat | `bat::printer::<InteractivePrinter<'_>>::print_header` | 1350 | 570 | 39 | 583 | 583 | constraints 1237, liveness 487, walk 486 |
| 2738 | 3181 | bat | `bat::run` | 1584 | 565 | 44 | 497 | 495 | constraints 814, liveness 642, walk 570 |
| 2451 | 2495 | bat | `bat::list_themes` | 1156 | 406 | 39 | 391 | 391 | constraints 654, liveness 632, walk 542 |
| 2333 | 5718 | bat | `bat::line_range::<LineRange>::parse_range` | 1527 | 401 | 24 | 387 | 385 | constraints 1146, liveness 444, moves 275 |
| 2063 | 2098 | bat | `bat::controller::<Controller<'_>>::run_with_error_handler` | 797 | 286 | 32 | 342 | 342 | constraints 856, liveness 426, walk 221 |
| 1907 | 5253 | bat | `bat::app::<App>::inputs` | 517 | 478 | 14 | 531 | 519 | constraints 1050, walk 315, liveness 206 |
| 1644 | 1894 | bat | `bat::diff::get_git_diff` | 781 | 158 | 22 | 140 | 140 | constraints 528, liveness 456, walk 336 |
| 1606 | 3520 | bat | `bat::input::<Input<'a>>::open` | 1027 | 299 | 21 | 271 | 271 | constraints 589, liveness 406, moves 235 |
| 1577 | 3061 | bat | `bat::printer::<InteractivePrinter<'a>>::new` | 824 | 356 | 13 | 378 | 376 | constraints 697, liveness 329, walk 184 |
| 1455 | 2255 | bat | `bat::output::<OutputType>::try_pager` | 800 | 318 | 34 | 250 | 250 | constraints 474, liveness 414, walk 197 |
| 1327 | 1697 | bat | `snapshot_tests::tester::create_sample_directory` | 579 | 189 | 12 | 165 | 165 | constraints 455, liveness 339, walk 269 |
| 1299 | 1448 | bat | `bat::controller::<Controller<'_>>::print_file_ranges` | 832 | 153 | 25 | 150 | 150 | constraints 445, liveness 314, walk 213 |
| 1281 | 2248 | bat | `build_script_main::application::gen_man_and_comp` | 1000 | 444 | 30 | 350 | 350 | constraints 518, liveness 294, moves 186 |
| 1273 | 2564 | bat | `bat::assets::<HighlightingAssets>::get_syntax` | 622 | 248 | 8 | 238 | 238 | constraints 489, liveness 262, walk 215 |
| 1182 | 3154 | bat | `build_script_main::syntax_mapping::parse_glob` | 559 | 290 | 23 | 271 | 271 | constraints 630, liveness 206, moves 126 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 63257 | 6954 | bat | `bat::syntax_mapping::builtin::BUILTIN_MAPPINGS` | 12513 | 2209 | 0 | 1487 | 1487 | walk 28851, liveness 21186, constraints 8241 |
| 7603 | 9851 | bat | `bat::printer::<InteractivePrinter<'_>>::print_line` | 3680 | 1306 | 117 | 1419 | 1419 | constraints 3033, liveness 1944, walk 1101 |
| 6008 | 20423 | bat | `bat::clap_app::build_app` | 2667 | 1306 | 15 | 1037 | 1027 | constraints 2361, walk 1633, liveness 949 |
| 5462 | 15365 | bat | `bat::app::<App>::config` | 2440 | 1265 | 35 | 1173 | 1171 | constraints 2505, liveness 1052, walk 865 |
| 3006 | 5575 | bat | `bat::get_languages` | 1189 | 572 | 46 | 556 | 556 | constraints 1581, liveness 561, walk 357 |
| 2652 | 6777 | bat | `bat::printer::<InteractivePrinter<'_>>::print_header` | 1350 | 570 | 39 | 583 | 583 | constraints 1147, walk 485, liveness 474 |
| 2528 | 3181 | bat | `bat::run` | 1584 | 565 | 44 | 497 | 495 | constraints 797, liveness 610, walk 556 |
| 2215 | 2495 | bat | `bat::list_themes` | 1156 | 406 | 39 | 391 | 391 | liveness 619, constraints 576, walk 556 |
| 2130 | 5718 | bat | `bat::line_range::<LineRange>::parse_range` | 1527 | 401 | 24 | 387 | 385 | constraints 1066, liveness 404, moves 266 |
| 1764 | 5253 | bat | `bat::app::<App>::inputs` | 517 | 478 | 14 | 531 | 519 | constraints 1002, walk 297, liveness 189 |
| 1762 | 2098 | bat | `bat::controller::<Controller<'_>>::run_with_error_handler` | 797 | 286 | 32 | 342 | 342 | constraints 822, liveness 381, walk 221 |
| 1494 | 3520 | bat | `bat::input::<Input<'a>>::open` | 1027 | 299 | 21 | 271 | 271 | constraints 566, liveness 364, moves 233 |
| 1444 | 3061 | bat | `bat::printer::<InteractivePrinter<'a>>::new` | 824 | 356 | 13 | 378 | 376 | constraints 641, liveness 299, moves 178 |
| 1433 | 1894 | bat | `bat::diff::get_git_diff` | 781 | 158 | 22 | 140 | 140 | constraints 477, liveness 381, walk 312 |
| 1220 | 2255 | bat | `bat::output::<OutputType>::try_pager` | 800 | 318 | 34 | 250 | 250 | constraints 461, liveness 280, walk 166 |
| 1215 | 1448 | bat | `bat::controller::<Controller<'_>>::print_file_ranges` | 832 | 153 | 25 | 150 | 150 | constraints 406, liveness 314, walk 210 |
| 1206 | 2248 | bat | `build_script_main::application::gen_man_and_comp` | 1000 | 444 | 30 | 350 | 350 | constraints 461, liveness 318, moves 187 |
| 1164 | 2564 | bat | `bat::assets::<HighlightingAssets>::get_syntax` | 622 | 248 | 8 | 238 | 238 | constraints 448, liveness 239, walk 218 |
| 1137 | 1697 | bat | `snapshot_tests::tester::create_sample_directory` | 579 | 189 | 12 | 165 | 165 | constraints 387, liveness 293, walk 242 |
| 1063 | 3154 | bat | `build_script_main::syntax_mapping::parse_glob` | 559 | 290 | 23 | 271 | 271 | constraints 573, liveness 188, moves 119 |
