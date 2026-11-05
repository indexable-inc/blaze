| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| bat | polonius | 576 | 2661 | 262 | 153 | 0 | 58.4% | 452 | 70 | 2411 | 33968 |
| bat | nll | 576 | 2661 | 262 | 137 | 0 | 52.2% | 452 | 57 | 2280 | 33593 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 59 | 3 | 1 | 3 | 44 | 2 | 0 | 19 | 4 | 0 | 9 |
| nll | 53 | 3 | 1 | 3 | 42 | 2 | 0 | 18 | 0 | 0 | 8 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 33968 | 6951 | bat | `bat::syntax_mapping::builtin::BUILTIN_MAPPINGS` | 12513 | 2209 | 0 | 1487 | 1487 | liveness 20297, constraints 7747, moves 3459 |
| 7411 | 9568 | bat | `bat::printer::<InteractivePrinter<'_>>::print_line` | 3680 | 1306 | 117 | 1419 | 1419 | constraints 3002, liveness 1914, moves 821 |
| 4976 | 15233 | bat | `bat::app::<App>::config` | 2440 | 1265 | 35 | 1173 | 1171 | constraints 2548, liveness 971, moves 545 |
| 4497 | 20084 | bat | `bat::clap_app::build_app` | 2667 | 1306 | 15 | 1037 | 1027 | constraints 2365, liveness 858, moves 572 |
| 2869 | 5556 | bat | `bat::get_languages` | 1189 | 572 | 46 | 556 | 556 | constraints 1594, liveness 500, moves 265 |
| 2411 | 6520 | bat | `bat::printer::<InteractivePrinter<'_>>::print_header` | 1350 | 570 | 39 | 583 | 583 | constraints 1169, liveness 468, moves 295 |
| 2206 | 5661 | bat | `bat::line_range::<LineRange>::parse_range` | 1527 | 401 | 24 | 387 | 385 | constraints 1107, liveness 421, moves 279 |
| 2202 | 3150 | bat | `bat::run` | 1584 | 565 | 44 | 497 | 495 | constraints 790, liveness 528, moves 310 |
| 1885 | 2076 | bat | `bat::controller::<Controller<'_>>::run_with_error_handler` | 797 | 286 | 32 | 342 | 342 | constraints 840, liveness 391, scopes 215 |
| 1876 | 2463 | bat | `bat::list_themes` | 1156 | 406 | 39 | 391 | 391 | constraints 598, liveness 530, moves 279 |
| 1657 | 5328 | bat | `bat::app::<App>::inputs` | 517 | 478 | 14 | 531 | 519 | constraints 1048, liveness 192, moves 138 |
| 1505 | 3545 | bat | `bat::input::<Input<'a>>::open` | 1027 | 299 | 21 | 271 | 271 | constraints 598, liveness 377, moves 230 |
| 1499 | 3047 | bat | `bat::printer::<InteractivePrinter<'a>>::new` | 824 | 356 | 13 | 378 | 376 | constraints 709, liveness 297, moves 195 |
| 1295 | 2240 | bat | `build_script_main::application::gen_man_and_comp` | 1000 | 444 | 30 | 350 | 350 | constraints 551, liveness 290, moves 179 |
| 1260 | 1852 | bat | `bat::diff::get_git_diff` | 781 | 158 | 22 | 140 | 140 | constraints 523, liveness 307, moves 165 |
| 1184 | 2151 | bat | `bat::output::<OutputType>::try_pager` | 800 | 318 | 34 | 250 | 250 | constraints 458, liveness 281, moves 158 |
| 1174 | 1486 | bat | `bat::controller::<Controller<'_>>::print_file_ranges` | 832 | 153 | 25 | 150 | 150 | constraints 460, liveness 302, moves 186 |
| 1137 | 3124 | bat | `build_script_main::syntax_mapping::parse_glob` | 559 | 290 | 23 | 271 | 271 | constraints 622, liveness 181, moves 125 |
| 1126 | 3337 | bat | `bat::config::get_args_from_str` | 141 | 173 | 1 | 241 | 241 | constraints 932, liveness 48, moves 46 |
| 1095 | 2603 | bat | `bat::assets::<HighlightingAssets>::get_syntax` | 622 | 248 | 8 | 238 | 238 | constraints 498, liveness 215, moves 161 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 33593 | 6951 | bat | `bat::syntax_mapping::builtin::BUILTIN_MAPPINGS` | 12513 | 2209 | 0 | 1487 | 1487 | liveness 20206, constraints 7707, moves 3412 |
| 6835 | 9568 | bat | `bat::printer::<InteractivePrinter<'_>>::print_line` | 3680 | 1306 | 117 | 1419 | 1419 | constraints 2970, liveness 1852, moves 819 |
| 4704 | 15233 | bat | `bat::app::<App>::config` | 2440 | 1265 | 35 | 1173 | 1171 | constraints 2473, liveness 907, moves 516 |
| 4392 | 20084 | bat | `bat::clap_app::build_app` | 2667 | 1306 | 15 | 1037 | 1027 | constraints 2306, liveness 867, moves 594 |
| 2686 | 5556 | bat | `bat::get_languages` | 1189 | 572 | 46 | 556 | 556 | constraints 1520, liveness 507, moves 259 |
| 2280 | 6520 | bat | `bat::printer::<InteractivePrinter<'_>>::print_header` | 1350 | 570 | 39 | 583 | 583 | constraints 1165, liveness 427, moves 289 |
| 2048 | 5661 | bat | `bat::line_range::<LineRange>::parse_range` | 1527 | 401 | 24 | 387 | 385 | constraints 1058, liveness 378, moves 268 |
| 1985 | 3150 | bat | `bat::run` | 1584 | 565 | 44 | 497 | 495 | constraints 729, liveness 522, moves 312 |
| 1632 | 2076 | bat | `bat::controller::<Controller<'_>>::run_with_error_handler` | 797 | 286 | 32 | 342 | 342 | constraints 797, liveness 361, moves 193 |
| 1608 | 2463 | bat | `bat::list_themes` | 1156 | 406 | 39 | 391 | 391 | constraints 525, liveness 503, moves 261 |
| 1495 | 5328 | bat | `bat::app::<App>::inputs` | 517 | 478 | 14 | 531 | 519 | constraints 938, liveness 204, moves 127 |
| 1381 | 3545 | bat | `bat::input::<Input<'a>>::open` | 1027 | 299 | 21 | 271 | 271 | constraints 553, liveness 355, moves 218 |
| 1335 | 3047 | bat | `bat::printer::<InteractivePrinter<'a>>::new` | 824 | 356 | 13 | 378 | 376 | constraints 643, liveness 270, moves 171 |
| 1129 | 2240 | bat | `build_script_main::application::gen_man_and_comp` | 1000 | 444 | 30 | 350 | 350 | constraints 485, liveness 252, moves 172 |
| 1064 | 3337 | bat | `bat::config::get_args_from_str` | 141 | 173 | 1 | 241 | 241 | constraints 887, liveness 58, moves 43 |
| 1055 | 1486 | bat | `bat::controller::<Controller<'_>>::print_file_ranges` | 832 | 153 | 25 | 150 | 150 | constraints 410, liveness 289, moves 161 |
| 1053 | 2151 | bat | `bat::output::<OutputType>::try_pager` | 800 | 318 | 34 | 250 | 250 | constraints 418, liveness 245, moves 151 |
| 1045 | 1852 | bat | `bat::diff::get_git_diff` | 781 | 158 | 22 | 140 | 140 | constraints 434, liveness 278, moves 147 |
| 1034 | 3124 | bat | `build_script_main::syntax_mapping::parse_glob` | 559 | 290 | 23 | 271 | 271 | constraints 576, liveness 167, moves 119 |
| 969 | 2603 | bat | `bat::assets::<HighlightingAssets>::get_syntax` | 622 | 248 | 8 | 238 | 238 | constraints 443, liveness 205, moves 140 |
