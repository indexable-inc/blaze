| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| hyperfine | polonius | 304 | 2273 | 201 | 80 | 0 | 39.7% | 226 | 83 | 3801 | 8173 |
| hyperfine | nll | 304 | 2273 | 201 | 70 | 0 | 34.9% | 226 | 68 | 3598 | 7437 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 38 | 1 | 0 | 2 | 13 | 1 | 0 | 9 | 3 | 0 | 8 |
| nll | 34 | 1 | 0 | 2 | 12 | 1 | 0 | 8 | 0 | 0 | 8 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 8173 | 21140 | hyperfine | `hyperfine::benchmark::<Benchmark<'a>>::run` | 3222 | 1638 | 84 | 1954 | 1954 | constraints 3976, liveness 1417, scopes 773 |
| 5143 | 12628 | hyperfine | `hyperfine::output::report::print` | 2576 | 1158 | 92 | 1147 | 1147 | constraints 2228, liveness 1175, moves 679 |
| 4957 | 9685 | hyperfine | `hyperfine::options::<Options>::from_cli_arguments` | 2088 | 723 | 19 | 675 | 673 | constraints 2445, liveness 856, walk 652 |
| 3801 | 15192 | hyperfine | `hyperfine::command::<Commands<'a>>::build_from_cli_arguments` | 1238 | 925 | 31 | 1162 | 1156 | constraints 2016, liveness 572, walk 487 |
| 2729 | 4476 | hyperfine | `build_script_build::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 951, walk 812, liveness 463 |
| 2718 | 4601 | hyperfine | `hyperfine::cli::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 932, walk 812, liveness 451 |
| 2526 | 7860 | hyperfine | `hyperfine::export::csv::<CsvExporter>::serialize` | 898 | 449 | 28 | 431 | 429 | constraints 1507, liveness 362, moves 234 |
| 1488 | 3130 | hyperfine | `hyperfine::metric::<MetricSelection>::parse_list` | 646 | 401 | 23 | 403 | 401 | constraints 810, liveness 237, moves 141 |
| 1178 | 2232 | hyperfine | `hyperfine::benchmark::executor::run_command_and_measure_common` | 767 | 305 | 24 | 254 | 254 | constraints 437, liveness 246, moves 162 |
| 1127 | 1335 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::run_command_and_measure` | 309 | 158 | 4 | 247 | 247 | constraints 786, liveness 107, walk 89 |
| 1104 | 1715 | hyperfine | `hyperfine::run` | 680 | 223 | 24 | 215 | 215 | constraints 359, liveness 299, moves 139 |
| 1025 | 1499 | hyperfine | `hyperfine::timer::execute_and_measure` | 251 | 98 | 4 | 152 | 152 | constraints 699, liveness 118, walk 81 |
| 998 | 1936 | hyperfine | `hyperfine::quantity::statistics::median` | 164 | 59 | 7 | 59 | 59 | constraints 811, walk 58, liveness 45 |
| 997 | 2134 | hyperfine | `hyperfine::export::markup::MarkupExporter::table_results` | 717 | 393 | 32 | 362 | 362 | constraints 352, liveness 243, moves 137 |
| 914 | 1811 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::calibrate` | 475 | 205 | 18 | 217 | 217 | constraints 426, liveness 205, moves 107 |
| 914 | 1138 | hyperfine | `hyperfine::export::<ExportManager>::write_results` | 554 | 181 | 13 | 190 | 190 | constraints 326, liveness 221, moves 142 |
| 909 | 1678 | hyperfine | `hyperfine::command::<Commands<'a>>::get_parameter_scan_commands` | 433 | 146 | 4 | 177 | 177 | constraints 395, walk 179, liveness 145 |
| 860 | 3563 | hyperfine | `hyperfine::metric::<Metric>::summarize` | 260 | 179 | 10 | 210 | 210 | constraints 589, liveness 74, moves 72 |
| 850 | 1583 | hyperfine | `hyperfine::command::<Commands<'a>>::from_cli_arguments` | 391 | 142 | 11 | 190 | 190 | constraints 345, liveness 162, walk 139 |
| 807 | 1805 | hyperfine | `hyperfine::command::parse_environment` | 488 | 182 | 14 | 163 | 163 | constraints 323, liveness 162, moves 148 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 7437 | 21140 | hyperfine | `hyperfine::benchmark::<Benchmark<'a>>::run` | 3222 | 1638 | 84 | 1954 | 1954 | constraints 3921, liveness 1416, moves 756 |
| 4861 | 12628 | hyperfine | `hyperfine::output::report::print` | 2576 | 1158 | 92 | 1147 | 1147 | constraints 2191, liveness 1172, moves 664 |
| 4746 | 9685 | hyperfine | `hyperfine::options::<Options>::from_cli_arguments` | 2088 | 723 | 19 | 675 | 673 | constraints 2386, liveness 806, walk 611 |
| 3598 | 15192 | hyperfine | `hyperfine::command::<Commands<'a>>::build_from_cli_arguments` | 1238 | 925 | 31 | 1162 | 1156 | constraints 1958, liveness 570, walk 471 |
| 2547 | 4476 | hyperfine | `build_script_build::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 889, walk 813, liveness 420 |
| 2530 | 4601 | hyperfine | `hyperfine::cli::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 868, walk 795, liveness 433 |
| 2391 | 7860 | hyperfine | `hyperfine::export::csv::<CsvExporter>::serialize` | 898 | 449 | 28 | 431 | 429 | constraints 1421, liveness 403, moves 220 |
| 1332 | 3130 | hyperfine | `hyperfine::metric::<MetricSelection>::parse_list` | 646 | 401 | 23 | 403 | 401 | constraints 747, liveness 208, moves 134 |
| 1062 | 1335 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::run_command_and_measure` | 309 | 158 | 4 | 247 | 247 | constraints 746, liveness 104, walk 84 |
| 1048 | 2232 | hyperfine | `hyperfine::benchmark::executor::run_command_and_measure_common` | 767 | 305 | 24 | 254 | 254 | constraints 386, liveness 252, moves 152 |
| 980 | 1936 | hyperfine | `hyperfine::quantity::statistics::median` | 164 | 59 | 7 | 59 | 59 | constraints 808, walk 57, liveness 44 |
| 949 | 1715 | hyperfine | `hyperfine::run` | 680 | 223 | 24 | 215 | 215 | constraints 309, liveness 270, moves 133 |
| 904 | 1499 | hyperfine | `hyperfine::timer::execute_and_measure` | 251 | 98 | 4 | 152 | 152 | constraints 634, liveness 106, moves 62 |
| 851 | 2134 | hyperfine | `hyperfine::export::markup::MarkupExporter::table_results` | 717 | 393 | 32 | 362 | 362 | constraints 305, liveness 211, moves 130 |
| 837 | 1811 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::calibrate` | 475 | 205 | 18 | 217 | 217 | constraints 390, liveness 203, moves 98 |
| 803 | 1678 | hyperfine | `hyperfine::command::<Commands<'a>>::get_parameter_scan_commands` | 433 | 146 | 4 | 177 | 177 | constraints 339, walk 162, liveness 138 |
| 770 | 1138 | hyperfine | `hyperfine::export::<ExportManager>::write_results` | 554 | 181 | 13 | 190 | 190 | constraints 280, liveness 200, moves 136 |
| 729 | 1583 | hyperfine | `hyperfine::command::<Commands<'a>>::from_cli_arguments` | 391 | 142 | 11 | 190 | 190 | constraints 303, liveness 146, walk 119 |
| 704 | 3563 | hyperfine | `hyperfine::metric::<Metric>::summarize` | 260 | 179 | 10 | 210 | 210 | constraints 485, liveness 67, moves 61 |
| 676 | 1805 | hyperfine | `hyperfine::command::parse_environment` | 488 | 182 | 14 | 163 | 163 | constraints 269, liveness 153, moves 118 |
