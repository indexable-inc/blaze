| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| hyperfine | polonius | 304 | 2475 | 227 | 149 | 0 | 65.5% | 263 | 145 | 6860 | 15821 |
| hyperfine | nll | 304 | 2475 | 227 | 139 | 0 | 61.3% | 263 | 116 | 6634 | 15505 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 90 | 2 | 0 | 2 | 15 | 1 | 0 | 24 | 3 | 0 | 8 |
| nll | 86 | 1 | 0 | 2 | 14 | 1 | 0 | 23 | 0 | 0 | 7 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 15821 | 24454 | hyperfine | `hyperfine::benchmark::<Benchmark<'a>>::run` | 3222 | 1638 | 84 | 1954 | 1954 | constraints 9388, moves 2469, liveness 1723 |
| 7839 | 16618 | hyperfine | `hyperfine::output::report::print` | 2576 | 1158 | 92 | 1147 | 1147 | constraints 4247, liveness 1236, moves 1181 |
| 6927 | 11179 | hyperfine | `hyperfine::options::<Options>::from_cli_arguments` | 2082 | 723 | 19 | 675 | 673 | constraints 3706, moves 1262, liveness 921 |
| 6860 | 16853 | hyperfine | `hyperfine::command::<Commands<'a>>::build_from_cli_arguments` | 1238 | 925 | 31 | 1162 | 1156 | constraints 4595, liveness 730, moves 690 |
| 3683 | 5150 | hyperfine | `hyperfine::cli::build_command` | 1390 | 644 | 28 | 684 | 682 | constraints 1704, walk 858, liveness 491 |
| 3574 | 5433 | hyperfine | `build_script_build::build_command` | 1390 | 644 | 28 | 684 | 682 | constraints 1645, walk 848, liveness 467 |
| 3528 | 9079 | hyperfine | `hyperfine::export::csv::<CsvExporter>::serialize` | 898 | 449 | 28 | 431 | 429 | constraints 2200, moves 545, liveness 356 |
| 3150 | 4102 | hyperfine | `hyperfine::metric::<Metric>::summarize` | 260 | 179 | 10 | 210 | 210 | constraints 2724, moves 174, liveness 96 |
| 2343 | 1726 | hyperfine | `hyperfine::timer::execute_and_measure` | 251 | 98 | 4 | 152 | 152 | constraints 1527, moves 557, liveness 133 |
| 2325 | 2288 | hyperfine | `hyperfine::benchmark::measurement::<Measurements>::time_user_mean` | 27 | 51 | 0 | 79 | 79 | constraints 2238, moves 31, liveness 13 |
| 2292 | 2246 | hyperfine | `hyperfine::benchmark::measurement::<Measurements>::time_system_mean` | 27 | 51 | 0 | 79 | 79 | constraints 2200, moves 38, walk 12 |
| 2235 | 1454 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::run_command_and_measure` | 309 | 158 | 4 | 247 | 247 | constraints 1490, moves 469, liveness 132 |
| 1986 | 1986 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::calibrate` | 475 | 205 | 18 | 217 | 217 | constraints 908, moves 620, liveness 263 |
| 1958 | 3565 | hyperfine | `hyperfine::metric::<MetricSelection>::parse_list` | 646 | 400 | 23 | 403 | 401 | constraints 1155, moves 283, liveness 227 |
| 1865 | 2461 | hyperfine | `hyperfine::benchmark::executor::run_command_and_measure_common` | 767 | 305 | 24 | 254 | 254 | constraints 943, moves 334, liveness 275 |
| 1794 | 2015 | hyperfine | `hyperfine::run` | 680 | 223 | 24 | 215 | 215 | constraints 882, liveness 345, moves 265 |
| 1727 | 1653 | hyperfine | `hyperfine::benchmark::measurement::<Measurements>::time_wall_clock_mean` | 11 | 15 | 0 | 30 | 30 | constraints 1676, moves 21, liveness 7 |
| 1721 | 2286 | hyperfine | `hyperfine::quantity::statistics::modified_zscores_f64` | 156 | 101 | 4 | 114 | 114 | constraints 1506, moves 81, liveness 54 |
| 1627 | 2181 | hyperfine | `hyperfine::quantity::statistics::median` | 164 | 59 | 7 | 59 | 59 | constraints 859, moves 636, liveness 50 |
| 1589 | 2515 | hyperfine | `hyperfine::export::markup::MarkupExporter::table_results` | 717 | 393 | 32 | 362 | 362 | constraints 773, moves 294, liveness 245 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 15505 | 24454 | hyperfine | `hyperfine::benchmark::<Benchmark<'a>>::run` | 3222 | 1638 | 84 | 1954 | 1954 | constraints 9745, moves 2574, liveness 1702 |
| 7794 | 16618 | hyperfine | `hyperfine::output::report::print` | 2576 | 1158 | 92 | 1147 | 1147 | constraints 4288, liveness 1409, moves 1204 |
| 6700 | 11179 | hyperfine | `hyperfine::options::<Options>::from_cli_arguments` | 2082 | 723 | 19 | 675 | 673 | constraints 3578, moves 1259, liveness 930 |
| 6634 | 16853 | hyperfine | `hyperfine::command::<Commands<'a>>::build_from_cli_arguments` | 1238 | 925 | 31 | 1162 | 1156 | constraints 4535, liveness 744, moves 657 |
| 3478 | 5150 | hyperfine | `hyperfine::cli::build_command` | 1390 | 644 | 28 | 684 | 682 | constraints 1651, walk 845, liveness 451 |
| 3390 | 5433 | hyperfine | `build_script_build::build_command` | 1390 | 644 | 28 | 684 | 682 | constraints 1556, walk 840, liveness 446 |
| 3384 | 9079 | hyperfine | `hyperfine::export::csv::<CsvExporter>::serialize` | 898 | 449 | 28 | 431 | 429 | constraints 2134, moves 509, liveness 353 |
| 3057 | 4102 | hyperfine | `hyperfine::metric::<Metric>::summarize` | 260 | 179 | 10 | 210 | 210 | constraints 2680, moves 171, liveness 84 |
| 2272 | 2288 | hyperfine | `hyperfine::benchmark::measurement::<Measurements>::time_user_mean` | 27 | 51 | 0 | 79 | 79 | constraints 2187, moves 40, liveness 12 |
| 2272 | 2246 | hyperfine | `hyperfine::benchmark::measurement::<Measurements>::time_system_mean` | 27 | 51 | 0 | 79 | 79 | constraints 2195, moves 29, liveness 13 |
| 2212 | 1726 | hyperfine | `hyperfine::timer::execute_and_measure` | 251 | 98 | 4 | 152 | 152 | constraints 1460, moves 518, liveness 131 |
| 2152 | 1454 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::run_command_and_measure` | 309 | 158 | 4 | 247 | 247 | constraints 1442, moves 441, liveness 146 |
| 1892 | 1986 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::calibrate` | 475 | 205 | 18 | 217 | 217 | constraints 859, moves 593, liveness 278 |
| 1777 | 3565 | hyperfine | `hyperfine::metric::<MetricSelection>::parse_list` | 646 | 400 | 23 | 403 | 401 | constraints 1055, moves 277, liveness 214 |
| 1680 | 2461 | hyperfine | `hyperfine::benchmark::executor::run_command_and_measure_common` | 767 | 305 | 24 | 254 | 254 | constraints 845, moves 309, liveness 270 |
| 1676 | 1653 | hyperfine | `hyperfine::benchmark::measurement::<Measurements>::time_wall_clock_mean` | 11 | 15 | 0 | 30 | 30 | constraints 1631, moves 20, liveness 7 |
| 1638 | 2286 | hyperfine | `hyperfine::quantity::statistics::modified_zscores_f64` | 156 | 101 | 4 | 114 | 114 | constraints 1458, moves 74, liveness 40 |
| 1592 | 2015 | hyperfine | `hyperfine::run` | 680 | 223 | 24 | 215 | 215 | constraints 820, liveness 295, moves 239 |
| 1582 | 2181 | hyperfine | `hyperfine::quantity::statistics::median` | 164 | 59 | 7 | 59 | 59 | constraints 845, moves 602, liveness 47 |
| 1544 | 2515 | hyperfine | `hyperfine::export::markup::MarkupExporter::table_results` | 717 | 393 | 32 | 362 | 362 | constraints 753, moves 308, liveness 260 |
