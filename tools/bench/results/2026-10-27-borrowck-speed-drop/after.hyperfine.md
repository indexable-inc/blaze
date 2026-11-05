| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| hyperfine | polonius | 304 | 2361 | 208 | 78 | 0 | 37.4% | 243 | 79 | 3668 | 8564 |
| hyperfine | nll | 304 | 2361 | 208 | 67 | 0 | 32.0% | 243 | 61 | 3273 | 7564 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 40 | 1 | 0 | 2 | 13 | 1 | 0 | 9 | 3 | 0 | 4 |
| nll | 35 | 1 | 0 | 2 | 12 | 1 | 0 | 8 | 0 | 0 | 4 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 8564 | 22453 | hyperfine | `hyperfine::benchmark::<Benchmark<'a>>::run` | 3222 | 1638 | 84 | 1954 | 1954 | constraints 4429, liveness 1476, moves 758 |
| 5421 | 13693 | hyperfine | `hyperfine::output::report::print` | 2576 | 1158 | 92 | 1147 | 1147 | constraints 2459, liveness 1198, moves 721 |
| 4671 | 9822 | hyperfine | `hyperfine::options::<Options>::from_cli_arguments` | 2088 | 723 | 19 | 675 | 673 | constraints 2457, liveness 859, moves 572 |
| 3668 | 16957 | hyperfine | `hyperfine::command::<Commands<'a>>::build_from_cli_arguments` | 1238 | 925 | 31 | 1162 | 1156 | constraints 2165, liveness 568, moves 338 |
| 2481 | 8056 | hyperfine | `hyperfine::export::csv::<CsvExporter>::serialize` | 898 | 449 | 28 | 431 | 429 | constraints 1502, liveness 355, moves 243 |
| 2084 | 4752 | hyperfine | `hyperfine::cli::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 1035, liveness 442, moves 264 |
| 2004 | 4858 | hyperfine | `build_script_build::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 986, liveness 412, moves 261 |
| 1481 | 3269 | hyperfine | `hyperfine::metric::<MetricSelection>::parse_list` | 646 | 401 | 23 | 403 | 401 | constraints 845, liveness 227, moves 140 |
| 1137 | 2308 | hyperfine | `hyperfine::benchmark::executor::run_command_and_measure_common` | 767 | 305 | 24 | 254 | 254 | constraints 460, liveness 264, moves 162 |
| 1123 | 1836 | hyperfine | `hyperfine::run` | 680 | 223 | 24 | 215 | 215 | constraints 411, liveness 290, moves 168 |
| 1079 | 1352 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::run_command_and_measure` | 309 | 158 | 4 | 247 | 247 | constraints 790, liveness 99, moves 67 |
| 1014 | 1535 | hyperfine | `hyperfine::timer::execute_and_measure` | 251 | 98 | 4 | 152 | 152 | constraints 728, liveness 106, moves 83 |
| 991 | 1989 | hyperfine | `hyperfine::quantity::statistics::median` | 164 | 59 | 7 | 59 | 59 | constraints 840, liveness 45, moves 38 |
| 981 | 2129 | hyperfine | `hyperfine::export::markup::MarkupExporter::table_results` | 717 | 393 | 32 | 362 | 362 | constraints 374, liveness 224, moves 137 |
| 896 | 1807 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::calibrate` | 475 | 205 | 18 | 217 | 217 | constraints 436, liveness 191, moves 107 |
| 880 | 1191 | hyperfine | `hyperfine::export::<ExportManager>::write_results` | 554 | 181 | 13 | 190 | 190 | constraints 328, liveness 213, moves 138 |
| 871 | 3632 | hyperfine | `hyperfine::metric::<Metric>::summarize` | 260 | 179 | 10 | 210 | 210 | constraints 596, liveness 83, moves 71 |
| 832 | 2002 | hyperfine | `hyperfine::command::parse_environment` | 488 | 182 | 14 | 163 | 163 | constraints 361, liveness 172, moves 122 |
| 799 | 1660 | hyperfine | `hyperfine::command::<Commands<'a>>::from_cli_arguments` | 391 | 142 | 11 | 190 | 190 | constraints 367, liveness 153, moves 120 |
| 745 | 1720 | hyperfine | `hyperfine::command::<Commands<'a>>::get_parameter_scan_commands` | 433 | 146 | 4 | 177 | 177 | constraints 359, liveness 139, moves 100 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 7564 | 22453 | hyperfine | `hyperfine::benchmark::<Benchmark<'a>>::run` | 3222 | 1638 | 84 | 1954 | 1954 | constraints 4093, liveness 1472, moves 822 |
| 5000 | 13693 | hyperfine | `hyperfine::output::report::print` | 2576 | 1158 | 92 | 1147 | 1147 | constraints 2331, liveness 1187, moves 684 |
| 4420 | 9822 | hyperfine | `hyperfine::options::<Options>::from_cli_arguments` | 2088 | 723 | 19 | 675 | 673 | constraints 2435, liveness 767, moves 521 |
| 3273 | 16957 | hyperfine | `hyperfine::command::<Commands<'a>>::build_from_cli_arguments` | 1238 | 925 | 31 | 1162 | 1156 | constraints 1995, liveness 519, moves 293 |
| 2353 | 8056 | hyperfine | `hyperfine::export::csv::<CsvExporter>::serialize` | 898 | 449 | 28 | 431 | 429 | constraints 1440, liveness 341, moves 243 |
| 1860 | 4858 | hyperfine | `build_script_build::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 897, liveness 424, moves 250 |
| 1809 | 4752 | hyperfine | `hyperfine::cli::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 881, liveness 399, moves 250 |
| 1328 | 3269 | hyperfine | `hyperfine::metric::<MetricSelection>::parse_list` | 646 | 401 | 23 | 403 | 401 | constraints 758, liveness 211, moves 138 |
| 969 | 1352 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::run_command_and_measure` | 309 | 158 | 4 | 247 | 247 | constraints 726, liveness 92, moves 63 |
| 960 | 2308 | hyperfine | `hyperfine::benchmark::executor::run_command_and_measure_common` | 767 | 305 | 24 | 254 | 254 | constraints 379, liveness 236, moves 147 |
| 916 | 1989 | hyperfine | `hyperfine::quantity::statistics::median` | 164 | 59 | 7 | 59 | 59 | constraints 776, liveness 40, moves 34 |
| 880 | 1836 | hyperfine | `hyperfine::run` | 680 | 223 | 24 | 215 | 215 | constraints 305, liveness 255, moves 138 |
| 862 | 2129 | hyperfine | `hyperfine::export::markup::MarkupExporter::table_results` | 717 | 393 | 32 | 362 | 362 | constraints 320, liveness 221, moves 130 |
| 852 | 1535 | hyperfine | `hyperfine::timer::execute_and_measure` | 251 | 98 | 4 | 152 | 152 | constraints 621, liveness 96, moves 60 |
| 798 | 1807 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::calibrate` | 475 | 205 | 18 | 217 | 217 | constraints 391, liveness 165, moves 100 |
| 751 | 1191 | hyperfine | `hyperfine::export::<ExportManager>::write_results` | 554 | 181 | 13 | 190 | 190 | constraints 279, liveness 199, moves 128 |
| 712 | 3632 | hyperfine | `hyperfine::metric::<Metric>::summarize` | 260 | 179 | 10 | 210 | 210 | constraints 495, liveness 65, moves 61 |
| 679 | 2002 | hyperfine | `hyperfine::command::parse_environment` | 488 | 182 | 14 | 163 | 163 | constraints 281, liveness 155, moves 109 |
| 649 | 1660 | hyperfine | `hyperfine::command::<Commands<'a>>::from_cli_arguments` | 391 | 142 | 11 | 190 | 190 | constraints 292, liveness 150, moves 86 |
| 638 | 974 | hyperfine | `hyperfine::shell_syntax::<Tokens<'a>>::next` | 533 | 106 | 12 | 147 | 147 | constraints 234, liveness 162, moves 102 |
