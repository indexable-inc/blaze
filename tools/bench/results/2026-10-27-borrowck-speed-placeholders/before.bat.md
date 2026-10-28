| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| bat | polonius | 576 | 2729 | 266 | 196 | 0 | 73.5% | 514 | 71 | 2784 | 63250 |
| bat | nll | 576 | 2729 | 266 | 180 | 0 | 67.6% | 514 | 58 | 2670 | 63769 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 60 | 3 | 1 | 3 | 48 | 2 | 0 | 20 | 4 | 0 | 46 |
| nll | 54 | 3 | 1 | 3 | 46 | 2 | 0 | 18 | 0 | 0 | 45 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 63250 | 6967 | bat | `bat::syntax_mapping::builtin::BUILTIN_MAPPINGS` | 12513 | 2209 | 0 | 1487 | 1487 | walk 28337, liveness 21019, constraints 8247 |
| 8147 | 9935 | bat | `bat::printer::<InteractivePrinter<'_>>::print_line` | 3680 | 1306 | 117 | 1419 | 1419 | constraints 3071, liveness 1975, walk 1101 |
| 6139 | 19815 | bat | `bat::clap_app::build_app` | 2667 | 1306 | 15 | 1037 | 1027 | constraints 2479, walk 1651, liveness 965 |
| 5608 | 15184 | bat | `bat::app::<App>::config` | 2440 | 1265 | 35 | 1173 | 1171 | constraints 2539, liveness 1020, walk 905 |
| 3325 | 6051 | bat | `bat::get_languages` | 1189 | 572 | 46 | 556 | 556 | constraints 1735, liveness 577, walk 365 |
| 2784 | 6976 | bat | `bat::printer::<InteractivePrinter<'_>>::print_header` | 1350 | 570 | 39 | 583 | 583 | constraints 1194, walk 479, liveness 478 |
| 2711 | 3211 | bat | `bat::run` | 1584 | 565 | 44 | 497 | 495 | constraints 794, liveness 618, walk 576 |
| 2410 | 2594 | bat | `bat::list_themes` | 1156 | 406 | 39 | 391 | 391 | liveness 620, constraints 600, walk 567 |
| 2309 | 5858 | bat | `bat::line_range::<LineRange>::parse_range` | 1527 | 401 | 24 | 387 | 385 | constraints 1122, liveness 442, moves 285 |
| 2089 | 2334 | bat | `bat::controller::<Controller<'_>>::run_with_error_handler` | 797 | 286 | 32 | 342 | 342 | constraints 883, liveness 430, walk 223 |
| 1895 | 5260 | bat | `bat::app::<App>::inputs` | 517 | 478 | 14 | 531 | 519 | constraints 1043, walk 310, liveness 192 |
| 1646 | 3647 | bat | `bat::input::<Input<'a>>::open` | 1027 | 299 | 21 | 271 | 271 | constraints 600, liveness 403, moves 246 |
| 1638 | 1872 | bat | `bat::diff::get_git_diff` | 781 | 158 | 22 | 140 | 140 | constraints 522, liveness 439, walk 348 |
| 1605 | 3122 | bat | `bat::printer::<InteractivePrinter<'a>>::new` | 824 | 356 | 13 | 378 | 376 | constraints 700, liveness 329, moves 198 |
| 1348 | 2431 | bat | `build_script_main::application::gen_man_and_comp` | 1000 | 444 | 30 | 350 | 350 | constraints 557, liveness 301, moves 184 |
| 1330 | 2316 | bat | `bat::output::<OutputType>::try_pager` | 800 | 318 | 34 | 250 | 250 | constraints 481, liveness 312, walk 175 |
| 1319 | 1469 | bat | `bat::controller::<Controller<'_>>::print_file_ranges` | 832 | 153 | 25 | 150 | 150 | constraints 454, liveness 323, walk 225 |
| 1300 | 1708 | bat | `snapshot_tests::tester::create_sample_directory` | 579 | 189 | 12 | 165 | 165 | constraints 418, liveness 359, walk 261 |
| 1268 | 2606 | bat | `bat::assets::<HighlightingAssets>::get_syntax` | 622 | 248 | 8 | 238 | 238 | constraints 492, liveness 258, walk 214 |
| 1211 | 3543 | bat | `bat::config::get_args_from_str` | 141 | 173 | 1 | 241 | 241 | constraints 976, walk 67, liveness 50 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 63769 | 6967 | bat | `bat::syntax_mapping::builtin::BUILTIN_MAPPINGS` | 12513 | 2209 | 0 | 1487 | 1487 | walk 28543, liveness 21538, constraints 8304 |
| 7491 | 9935 | bat | `bat::printer::<InteractivePrinter<'_>>::print_line` | 3680 | 1306 | 117 | 1419 | 1419 | constraints 3007, liveness 1890, walk 1065 |
| 6041 | 19815 | bat | `bat::clap_app::build_app` | 2667 | 1306 | 15 | 1037 | 1027 | constraints 2424, walk 1659, liveness 962 |
| 5489 | 15184 | bat | `bat::app::<App>::config` | 2440 | 1265 | 35 | 1173 | 1171 | constraints 2511, liveness 1016, walk 879 |
| 3022 | 6051 | bat | `bat::get_languages` | 1189 | 572 | 46 | 556 | 556 | constraints 1615, liveness 547, walk 356 |
| 2670 | 6976 | bat | `bat::printer::<InteractivePrinter<'_>>::print_header` | 1350 | 570 | 39 | 583 | 583 | constraints 1183, walk 483, liveness 462 |
| 2500 | 3211 | bat | `bat::run` | 1584 | 565 | 44 | 497 | 495 | constraints 758, liveness 621, walk 558 |
| 2182 | 2594 | bat | `bat::list_themes` | 1156 | 406 | 39 | 391 | 391 | liveness 595, constraints 579, walk 546 |
| 2143 | 5858 | bat | `bat::line_range::<LineRange>::parse_range` | 1527 | 401 | 24 | 387 | 385 | constraints 1066, liveness 417, moves 260 |
| 1794 | 5260 | bat | `bat::app::<App>::inputs` | 517 | 478 | 14 | 531 | 519 | constraints 1018, walk 305, liveness 194 |
| 1757 | 2334 | bat | `bat::controller::<Controller<'_>>::run_with_error_handler` | 797 | 286 | 32 | 342 | 342 | constraints 813, liveness 390, walk 215 |
| 1472 | 3647 | bat | `bat::input::<Input<'a>>::open` | 1027 | 299 | 21 | 271 | 271 | constraints 560, liveness 361, moves 218 |
| 1457 | 3122 | bat | `bat::printer::<InteractivePrinter<'a>>::new` | 824 | 356 | 13 | 378 | 376 | constraints 646, liveness 291, walk 196 |
| 1448 | 1872 | bat | `bat::diff::get_git_diff` | 781 | 158 | 22 | 140 | 140 | constraints 464, liveness 397, walk 321 |
| 1204 | 1469 | bat | `bat::controller::<Controller<'_>>::print_file_ranges` | 832 | 153 | 25 | 150 | 150 | constraints 408, liveness 301, walk 207 |
| 1201 | 2316 | bat | `bat::output::<OutputType>::try_pager` | 800 | 318 | 34 | 250 | 250 | constraints 453, liveness 269, walk 165 |
| 1183 | 1708 | bat | `snapshot_tests::tester::create_sample_directory` | 579 | 189 | 12 | 165 | 165 | constraints 397, liveness 296, walk 251 |
| 1159 | 2431 | bat | `build_script_main::application::gen_man_and_comp` | 1000 | 444 | 30 | 350 | 350 | constraints 477, liveness 261, moves 180 |
| 1153 | 2606 | bat | `bat::assets::<HighlightingAssets>::get_syntax` | 622 | 248 | 8 | 238 | 238 | constraints 450, liveness 238, walk 210 |
| 1153 | 3506 | bat | `build_script_main::syntax_mapping::parse_glob` | 559 | 290 | 23 | 271 | 271 | constraints 640, liveness 195, moves 122 |
