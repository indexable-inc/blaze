| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| bat | polonius | 576 | 2683 | 264 | 155 | 0 | 58.7% | 462 | 70 | 2445 | 33974 |
| bat | nll | 576 | 2683 | 264 | 138 | 0 | 52.3% | 462 | 57 | 2307 | 33462 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 60 | 3 | 1 | 3 | 45 | 2 | 0 | 20 | 5 | 0 | 9 |
| nll | 54 | 3 | 1 | 3 | 42 | 2 | 0 | 18 | 0 | 0 | 8 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 33974 | 6992 | bat | `bat::syntax_mapping::builtin::BUILTIN_MAPPINGS` | 12513 | 2209 | 0 | 1487 | 1487 | liveness 20035, constraints 7802, moves 3656 |
| 7672 | 9838 | bat | `bat::printer::<InteractivePrinter<'_>>::print_line` | 3680 | 1306 | 117 | 1419 | 1419 | constraints 3059, liveness 2049, moves 818 |
| 4992 | 15496 | bat | `bat::app::<App>::config` | 2440 | 1265 | 35 | 1173 | 1171 | constraints 2554, liveness 941, moves 527 |
| 4624 | 20139 | bat | `bat::clap_app::build_app` | 2667 | 1306 | 15 | 1037 | 1027 | constraints 2399, liveness 924, moves 586 |
| 2950 | 5595 | bat | `bat::get_languages` | 1189 | 572 | 46 | 556 | 556 | constraints 1607, liveness 525, moves 282 |
| 2445 | 6616 | bat | `bat::printer::<InteractivePrinter<'_>>::print_header` | 1350 | 570 | 39 | 583 | 583 | constraints 1204, liveness 461, moves 298 |
| 2265 | 3223 | bat | `bat::run` | 1584 | 565 | 44 | 497 | 495 | constraints 794, liveness 561, moves 325 |
| 2199 | 5588 | bat | `bat::line_range::<LineRange>::parse_range` | 1527 | 401 | 24 | 387 | 385 | constraints 1112, liveness 396, moves 281 |
| 1956 | 2128 | bat | `bat::controller::<Controller<'_>>::run_with_error_handler` | 797 | 286 | 32 | 342 | 342 | constraints 875, liveness 413, scopes 216 |
| 1901 | 2539 | bat | `bat::list_themes` | 1156 | 406 | 39 | 391 | 391 | constraints 598, liveness 554, moves 280 |
| 1696 | 5326 | bat | `bat::app::<App>::inputs` | 517 | 478 | 14 | 531 | 519 | constraints 1053, liveness 218, moves 140 |
| 1528 | 3600 | bat | `bat::input::<Input<'a>>::open` | 1027 | 299 | 21 | 271 | 271 | constraints 617, liveness 374, moves 235 |
| 1522 | 3091 | bat | `bat::printer::<InteractivePrinter<'a>>::new` | 824 | 356 | 13 | 378 | 376 | constraints 721, liveness 295, moves 179 |
| 1297 | 1886 | bat | `bat::diff::get_git_diff` | 781 | 158 | 22 | 140 | 140 | constraints 548, liveness 348, moves 167 |
| 1282 | 2261 | bat | `build_script_main::application::gen_man_and_comp` | 1000 | 444 | 30 | 350 | 350 | constraints 527, liveness 295, moves 186 |
| 1203 | 2208 | bat | `bat::output::<OutputType>::try_pager` | 800 | 318 | 34 | 250 | 250 | constraints 480, liveness 273, moves 160 |
| 1202 | 3150 | bat | `build_script_main::syntax_mapping::parse_glob` | 559 | 290 | 23 | 271 | 271 | constraints 641, liveness 187, moves 140 |
| 1180 | 1466 | bat | `bat::controller::<Controller<'_>>::print_file_ranges` | 832 | 153 | 25 | 150 | 150 | constraints 452, liveness 329, moves 173 |
| 1160 | 3396 | bat | `bat::config::get_args_from_str` | 141 | 173 | 1 | 241 | 241 | constraints 954, moves 57, liveness 49 |
| 1127 | 2684 | bat | `bat::assets::<HighlightingAssets>::get_syntax` | 622 | 248 | 8 | 238 | 238 | constraints 511, liveness 227, moves 157 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 33462 | 6992 | bat | `bat::syntax_mapping::builtin::BUILTIN_MAPPINGS` | 12513 | 2209 | 0 | 1487 | 1487 | liveness 19822, constraints 7777, moves 3600 |
| 7131 | 9838 | bat | `bat::printer::<InteractivePrinter<'_>>::print_line` | 3680 | 1306 | 117 | 1419 | 1419 | constraints 3028, liveness 2046, moves 818 |
| 4732 | 15496 | bat | `bat::app::<App>::config` | 2440 | 1265 | 35 | 1173 | 1171 | constraints 2453, liveness 941, moves 515 |
| 4475 | 20139 | bat | `bat::clap_app::build_app` | 2667 | 1306 | 15 | 1037 | 1027 | constraints 2351, liveness 909, moves 587 |
| 2730 | 5595 | bat | `bat::get_languages` | 1189 | 572 | 46 | 556 | 556 | constraints 1568, liveness 487, moves 258 |
| 2307 | 6616 | bat | `bat::printer::<InteractivePrinter<'_>>::print_header` | 1350 | 570 | 39 | 583 | 583 | constraints 1175, liveness 444, moves 281 |
| 2045 | 3223 | bat | `bat::run` | 1584 | 565 | 44 | 497 | 495 | constraints 778, liveness 528, moves 305 |
| 2032 | 5588 | bat | `bat::line_range::<LineRange>::parse_range` | 1527 | 401 | 24 | 387 | 385 | constraints 1057, liveness 380, moves 258 |
| 1684 | 2539 | bat | `bat::list_themes` | 1156 | 406 | 39 | 391 | 391 | constraints 542, liveness 527, moves 263 |
| 1650 | 2128 | bat | `bat::controller::<Controller<'_>>::run_with_error_handler` | 797 | 286 | 32 | 342 | 342 | constraints 821, liveness 369, moves 172 |
| 1491 | 5326 | bat | `bat::app::<App>::inputs` | 517 | 478 | 14 | 531 | 519 | constraints 971, liveness 180, moves 126 |
| 1425 | 3600 | bat | `bat::input::<Input<'a>>::open` | 1027 | 299 | 21 | 271 | 271 | constraints 573, liveness 358, moves 225 |
| 1355 | 3091 | bat | `bat::printer::<InteractivePrinter<'a>>::new` | 824 | 356 | 13 | 378 | 376 | constraints 653, liveness 271, moves 175 |
| 1151 | 2261 | bat | `build_script_main::application::gen_man_and_comp` | 1000 | 444 | 30 | 350 | 350 | constraints 493, liveness 260, moves 174 |
| 1092 | 1886 | bat | `bat::diff::get_git_diff` | 781 | 158 | 22 | 140 | 140 | constraints 456, liveness 305, moves 147 |
| 1085 | 2208 | bat | `bat::output::<OutputType>::try_pager` | 800 | 318 | 34 | 250 | 250 | constraints 422, liveness 245, moves 173 |
| 1063 | 1466 | bat | `bat::controller::<Controller<'_>>::print_file_ranges` | 832 | 153 | 25 | 150 | 150 | constraints 406, liveness 304, moves 162 |
| 1061 | 3150 | bat | `build_script_main::syntax_mapping::parse_glob` | 559 | 290 | 23 | 271 | 271 | constraints 578, liveness 177, moves 121 |
| 1056 | 3396 | bat | `bat::config::get_args_from_str` | 141 | 173 | 1 | 241 | 241 | constraints 894, liveness 44, moves 42 |
| 994 | 2684 | bat | `bat::assets::<HighlightingAssets>::get_syntax` | 622 | 248 | 8 | 238 | 238 | constraints 451, liveness 214, moves 142 |
