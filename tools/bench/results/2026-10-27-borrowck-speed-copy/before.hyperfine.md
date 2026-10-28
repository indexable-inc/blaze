| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| hyperfine | polonius | 304 | 2326 | 209 | 119 | 0 | 56.8% | 247 | 100 | 5747 | 13049 |
| hyperfine | nll | 304 | 2326 | 209 | 109 | 0 | 51.9% | 247 | 84 | 5472 | 12162 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 62 | 1 | 0 | 2 | 14 | 1 | 0 | 23 | 3 | 0 | 7 |
| nll | 58 | 1 | 0 | 2 | 14 | 1 | 0 | 22 | 0 | 0 | 7 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 13049 | 21994 | hyperfine | `hyperfine::benchmark::<Benchmark<'a>>::run` | 3222 | 1638 | 84 | 1954 | 1954 | constraints 7113, moves 2256, liveness 1592 |
| 6696 | 13591 | hyperfine | `hyperfine::output::report::print` | 2576 | 1158 | 92 | 1147 | 1147 | constraints 3245, liveness 1202, moves 1106 |
| 5973 | 10291 | hyperfine | `hyperfine::options::<Options>::from_cli_arguments` | 2082 | 723 | 19 | 675 | 673 | constraints 2902, moves 1231, liveness 894 |
| 5747 | 15397 | hyperfine | `hyperfine::command::<Commands<'a>>::build_from_cli_arguments` | 1238 | 925 | 31 | 1162 | 1156 | constraints 3697, liveness 659, moves 614 |
| 3016 | 9011 | hyperfine | `hyperfine::export::csv::<CsvExporter>::serialize` | 898 | 449 | 28 | 431 | 429 | constraints 1710, moves 517, liveness 366 |
| 2984 | 4829 | hyperfine | `hyperfine::cli::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 1095, walk 809, liveness 470 |
| 2953 | 4764 | hyperfine | `build_script_build::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 1105, walk 799, liveness 460 |
| 2272 | 3716 | hyperfine | `hyperfine::metric::<Metric>::summarize` | 260 | 179 | 10 | 210 | 210 | constraints 1875, moves 168, liveness 83 |
| 1936 | 1377 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::run_command_and_measure` | 309 | 158 | 4 | 247 | 247 | constraints 1218, moves 450, liveness 125 |
| 1884 | 1556 | hyperfine | `hyperfine::timer::execute_and_measure` | 251 | 98 | 4 | 152 | 152 | constraints 1102, moves 524, liveness 137 |
| 1775 | 2186 | hyperfine | `hyperfine::benchmark::measurement::<Measurements>::time_user_mean` | 27 | 51 | 0 | 79 | 79 | constraints 1695, moves 31, liveness 12 |
| 1744 | 2140 | hyperfine | `hyperfine::benchmark::measurement::<Measurements>::time_system_mean` | 27 | 51 | 0 | 79 | 79 | constraints 1666, moves 30, liveness 11 |
| 1720 | 1768 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::calibrate` | 475 | 205 | 18 | 217 | 217 | constraints 699, moves 583, liveness 234 |
| 1690 | 3314 | hyperfine | `hyperfine::metric::<MetricSelection>::parse_list` | 646 | 401 | 23 | 403 | 401 | constraints 882, moves 298, liveness 233 |
| 1482 | 2303 | hyperfine | `hyperfine::benchmark::executor::run_command_and_measure_common` | 767 | 305 | 24 | 254 | 254 | constraints 573, moves 321, liveness 263 |
| 1482 | 2150 | hyperfine | `hyperfine::quantity::statistics::modified_zscores_f64` | 156 | 101 | 4 | 114 | 114 | constraints 1281, moves 78, liveness 42 |
| 1473 | 1569 | hyperfine | `hyperfine::benchmark::measurement::<Measurements>::time_wall_clock_mean` | 11 | 15 | 0 | 30 | 30 | constraints 1420, moves 20, scopes 9 |
| 1430 | 1779 | hyperfine | `hyperfine::run` | 680 | 223 | 24 | 215 | 215 | constraints 550, liveness 328, moves 267 |
| 1386 | 2252 | hyperfine | `hyperfine::export::markup::MarkupExporter::table_results` | 717 | 393 | 32 | 362 | 362 | constraints 588, moves 281, liveness 241 |
| 1344 | 2010 | hyperfine | `hyperfine::quantity::statistics::median` | 164 | 59 | 7 | 59 | 59 | constraints 611, moves 608, liveness 47 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 12162 | 21994 | hyperfine | `hyperfine::benchmark::<Benchmark<'a>>::run` | 3222 | 1638 | 84 | 1954 | 1954 | constraints 6967, moves 2236, liveness 1581 |
| 6328 | 13591 | hyperfine | `hyperfine::output::report::print` | 2576 | 1158 | 92 | 1147 | 1147 | constraints 3209, liveness 1173, moves 1071 |
| 5779 | 10291 | hyperfine | `hyperfine::options::<Options>::from_cli_arguments` | 2082 | 723 | 19 | 675 | 673 | constraints 2829, moves 1158, liveness 916 |
| 5472 | 15397 | hyperfine | `hyperfine::command::<Commands<'a>>::build_from_cli_arguments` | 1238 | 925 | 31 | 1162 | 1156 | constraints 3614, liveness 639, moves 598 |
| 2856 | 4764 | hyperfine | `build_script_build::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 1070, walk 795, liveness 435 |
| 2807 | 4829 | hyperfine | `hyperfine::cli::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 1061, walk 796, liveness 432 |
| 2785 | 9011 | hyperfine | `hyperfine::export::csv::<CsvExporter>::serialize` | 898 | 449 | 28 | 431 | 429 | constraints 1565, moves 490, liveness 363 |
| 2150 | 3716 | hyperfine | `hyperfine::metric::<Metric>::summarize` | 260 | 179 | 10 | 210 | 210 | constraints 1791, moves 163, liveness 79 |
| 1834 | 1377 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::run_command_and_measure` | 309 | 158 | 4 | 247 | 247 | constraints 1184, moves 409, liveness 123 |
| 1779 | 1556 | hyperfine | `hyperfine::timer::execute_and_measure` | 251 | 98 | 4 | 152 | 152 | constraints 1075, moves 475, liveness 127 |
| 1740 | 2140 | hyperfine | `hyperfine::benchmark::measurement::<Measurements>::time_system_mean` | 27 | 51 | 0 | 79 | 79 | constraints 1670, moves 28, liveness 12 |
| 1728 | 2186 | hyperfine | `hyperfine::benchmark::measurement::<Measurements>::time_user_mean` | 27 | 51 | 0 | 79 | 79 | constraints 1657, moves 29, liveness 12 |
| 1636 | 1768 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::calibrate` | 475 | 205 | 18 | 217 | 217 | constraints 673, moves 565, liveness 227 |
| 1615 | 3314 | hyperfine | `hyperfine::metric::<MetricSelection>::parse_list` | 646 | 401 | 23 | 403 | 401 | constraints 878, moves 269, liveness 224 |
| 1494 | 1569 | hyperfine | `hyperfine::benchmark::measurement::<Measurements>::time_wall_clock_mean` | 11 | 15 | 0 | 30 | 30 | constraints 1451, moves 19, liveness 7 |
| 1421 | 2150 | hyperfine | `hyperfine::quantity::statistics::modified_zscores_f64` | 156 | 101 | 4 | 114 | 114 | constraints 1245, moves 72, liveness 40 |
| 1385 | 2303 | hyperfine | `hyperfine::benchmark::executor::run_command_and_measure_common` | 767 | 305 | 24 | 254 | 254 | constraints 544, moves 304, liveness 257 |
| 1318 | 2010 | hyperfine | `hyperfine::quantity::statistics::median` | 164 | 59 | 7 | 59 | 59 | constraints 619, moves 586, liveness 48 |
| 1272 | 1779 | hyperfine | `hyperfine::run` | 680 | 223 | 24 | 215 | 215 | constraints 515, liveness 301, moves 237 |
| 1261 | 2252 | hyperfine | `hyperfine::export::markup::MarkupExporter::table_results` | 717 | 393 | 32 | 362 | 362 | constraints 543, moves 259, liveness 236 |
