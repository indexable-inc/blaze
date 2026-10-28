| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| hyperfine | polonius | 304 | 2477 | 223 | 125 | 0 | 55.8% | 272 | 98 | 6204 | 14849 |
| hyperfine | nll | 304 | 2477 | 223 | 113 | 0 | 50.7% | 272 | 84 | 5896 | 13929 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 66 | 1 | 0 | 2 | 15 | 1 | 0 | 24 | 3 | 0 | 8 |
| nll | 61 | 1 | 0 | 2 | 14 | 1 | 0 | 22 | 0 | 0 | 7 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 14849 | 25527 | hyperfine | `hyperfine::benchmark::<Benchmark<'a>>::run` | 3222 | 1638 | 84 | 1954 | 1954 | constraints 8290, moves 2640, liveness 1682 |
| 7544 | 14938 | hyperfine | `hyperfine::output::report::print` | 2576 | 1158 | 92 | 1147 | 1147 | constraints 3721, moves 1345, liveness 1301 |
| 6562 | 11711 | hyperfine | `hyperfine::options::<Options>::from_cli_arguments` | 2082 | 723 | 19 | 675 | 673 | constraints 3328, moves 1319, liveness 947 |
| 6204 | 16912 | hyperfine | `hyperfine::command::<Commands<'a>>::build_from_cli_arguments` | 1238 | 925 | 31 | 1162 | 1156 | constraints 4005, liveness 764, moves 663 |
| 3201 | 5363 | hyperfine | `build_script_build::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 1246, walk 816, liveness 485 |
| 3168 | 9227 | hyperfine | `hyperfine::export::csv::<CsvExporter>::serialize` | 898 | 449 | 28 | 431 | 429 | constraints 1739, moves 555, liveness 388 |
| 3141 | 5174 | hyperfine | `hyperfine::cli::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 1180, walk 822, liveness 492 |
| 2341 | 4213 | hyperfine | `hyperfine::metric::<Metric>::summarize` | 260 | 179 | 10 | 210 | 210 | constraints 1916, moves 192, liveness 84 |
| 2075 | 1636 | hyperfine | `hyperfine::timer::execute_and_measure` | 251 | 98 | 4 | 152 | 152 | constraints 1187, moves 601, liveness 153 |
| 2032 | 1404 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::run_command_and_measure` | 309 | 158 | 4 | 247 | 247 | constraints 1314, moves 417, liveness 140 |
| 1843 | 1900 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::calibrate` | 475 | 205 | 18 | 217 | 217 | constraints 766, moves 602, liveness 274 |
| 1801 | 2207 | hyperfine | `hyperfine::benchmark::measurement::<Measurements>::time_user_mean` | 27 | 51 | 0 | 79 | 79 | constraints 1713, moves 32, liveness 13 |
| 1792 | 2176 | hyperfine | `hyperfine::benchmark::measurement::<Measurements>::time_system_mean` | 27 | 51 | 0 | 79 | 79 | constraints 1708, moves 30, walk 12 |
| 1782 | 3549 | hyperfine | `hyperfine::metric::<MetricSelection>::parse_list` | 646 | 401 | 23 | 403 | 401 | constraints 952, moves 311, liveness 230 |
| 1622 | 2514 | hyperfine | `hyperfine::benchmark::executor::run_command_and_measure_common` | 767 | 305 | 24 | 254 | 254 | constraints 691, moves 338, liveness 282 |
| 1567 | 1965 | hyperfine | `hyperfine::run` | 680 | 223 | 24 | 215 | 215 | constraints 643, liveness 337, moves 286 |
| 1536 | 2243 | hyperfine | `hyperfine::quantity::statistics::modified_zscores_f64` | 156 | 101 | 4 | 114 | 114 | constraints 1315, moves 92, liveness 44 |
| 1516 | 1607 | hyperfine | `hyperfine::benchmark::measurement::<Measurements>::time_wall_clock_mean` | 11 | 15 | 0 | 30 | 30 | constraints 1463, moves 23, liveness 7 |
| 1417 | 2385 | hyperfine | `hyperfine::export::markup::MarkupExporter::table_results` | 717 | 393 | 32 | 362 | 362 | constraints 629, moves 279, liveness 236 |
| 1383 | 2124 | hyperfine | `hyperfine::quantity::statistics::median` | 164 | 59 | 7 | 59 | 59 | constraints 644, moves 608, liveness 50 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 13929 | 25527 | hyperfine | `hyperfine::benchmark::<Benchmark<'a>>::run` | 3222 | 1638 | 84 | 1954 | 1954 | constraints 8174, moves 2645, liveness 1658 |
| 7002 | 14938 | hyperfine | `hyperfine::output::report::print` | 2576 | 1158 | 92 | 1147 | 1147 | constraints 3664, liveness 1228, moves 1168 |
| 6401 | 11711 | hyperfine | `hyperfine::options::<Options>::from_cli_arguments` | 2082 | 723 | 19 | 675 | 673 | constraints 3266, moves 1294, liveness 942 |
| 5896 | 16912 | hyperfine | `hyperfine::command::<Commands<'a>>::build_from_cli_arguments` | 1238 | 925 | 31 | 1162 | 1156 | constraints 3920, liveness 680, moves 649 |
| 3015 | 5363 | hyperfine | `build_script_build::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 1184, walk 821, liveness 463 |
| 2953 | 9227 | hyperfine | `hyperfine::export::csv::<CsvExporter>::serialize` | 898 | 449 | 28 | 431 | 429 | constraints 1660, moves 544, liveness 378 |
| 2920 | 5174 | hyperfine | `hyperfine::cli::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 1116, walk 816, liveness 457 |
| 2280 | 4213 | hyperfine | `hyperfine::metric::<Metric>::summarize` | 260 | 179 | 10 | 210 | 210 | constraints 1868, moves 209, liveness 83 |
| 1879 | 1404 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::run_command_and_measure` | 309 | 158 | 4 | 247 | 247 | constraints 1213, moves 413, liveness 126 |
| 1862 | 1636 | hyperfine | `hyperfine::timer::execute_and_measure` | 251 | 98 | 4 | 152 | 152 | constraints 1099, moves 517, liveness 136 |
| 1823 | 2207 | hyperfine | `hyperfine::benchmark::measurement::<Measurements>::time_user_mean` | 27 | 51 | 0 | 79 | 79 | constraints 1746, moves 30, liveness 14 |
| 1761 | 2176 | hyperfine | `hyperfine::benchmark::measurement::<Measurements>::time_system_mean` | 27 | 51 | 0 | 79 | 79 | constraints 1689, moves 28, liveness 12 |
| 1688 | 1900 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::calibrate` | 475 | 205 | 18 | 217 | 217 | constraints 701, moves 578, liveness 247 |
| 1627 | 3549 | hyperfine | `hyperfine::metric::<MetricSelection>::parse_list` | 646 | 401 | 23 | 403 | 401 | constraints 884, moves 272, liveness 219 |
| 1451 | 1607 | hyperfine | `hyperfine::benchmark::measurement::<Measurements>::time_wall_clock_mean` | 11 | 15 | 0 | 30 | 30 | constraints 1411, moves 19, liveness 6 |
| 1422 | 2243 | hyperfine | `hyperfine::quantity::statistics::modified_zscores_f64` | 156 | 101 | 4 | 114 | 114 | constraints 1241, moves 73, liveness 41 |
| 1403 | 2514 | hyperfine | `hyperfine::benchmark::executor::run_command_and_measure_common` | 767 | 305 | 24 | 254 | 254 | constraints 566, moves 309, liveness 258 |
| 1348 | 1965 | hyperfine | `hyperfine::run` | 680 | 223 | 24 | 215 | 215 | constraints 549, liveness 293, moves 256 |
| 1305 | 2124 | hyperfine | `hyperfine::quantity::statistics::median` | 164 | 59 | 7 | 59 | 59 | moves 596, constraints 596, liveness 46 |
| 1266 | 2385 | hyperfine | `hyperfine::export::markup::MarkupExporter::table_results` | 717 | 393 | 32 | 362 | 362 | constraints 567, moves 260, liveness 225 |
