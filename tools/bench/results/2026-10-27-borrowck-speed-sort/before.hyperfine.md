| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| hyperfine | polonius | 304 | 2248 | 198 | 75 | 0 | 37.9% | 222 | 76 | 3377 | 8036 |
| hyperfine | nll | 304 | 2248 | 198 | 65 | 0 | 33.0% | 222 | 60 | 3083 | 7181 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 38 | 1 | 0 | 2 | 13 | 1 | 0 | 9 | 3 | 0 | 4 |
| nll | 34 | 1 | 0 | 2 | 12 | 1 | 0 | 8 | 0 | 0 | 4 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 8036 | 21074 | hyperfine | `hyperfine::benchmark::<Benchmark<'a>>::run` | 3222 | 1638 | 84 | 1954 | 1954 | constraints 3938, liveness 1487, scopes 757 |
| 5028 | 12592 | hyperfine | `hyperfine::output::report::print` | 2576 | 1158 | 92 | 1147 | 1147 | constraints 2172, liveness 1173, moves 663 |
| 4397 | 9353 | hyperfine | `hyperfine::options::<Options>::from_cli_arguments` | 2088 | 723 | 19 | 675 | 673 | constraints 2342, liveness 750, moves 539 |
| 3377 | 14684 | hyperfine | `hyperfine::command::<Commands<'a>>::build_from_cli_arguments` | 1238 | 925 | 31 | 1162 | 1156 | constraints 1935, liveness 548, moves 330 |
| 2453 | 7789 | hyperfine | `hyperfine::export::csv::<CsvExporter>::serialize` | 898 | 449 | 28 | 431 | 429 | constraints 1479, liveness 354, moves 240 |
| 2039 | 4436 | hyperfine | `build_script_build::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 998, liveness 424, moves 263 |
| 1921 | 4473 | hyperfine | `hyperfine::cli::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 906, liveness 400, moves 270 |
| 1398 | 3109 | hyperfine | `hyperfine::metric::<MetricSelection>::parse_list` | 646 | 401 | 23 | 403 | 401 | constraints 785, liveness 213, moves 138 |
| 1085 | 2217 | hyperfine | `hyperfine::benchmark::executor::run_command_and_measure_common` | 767 | 305 | 24 | 254 | 254 | constraints 444, liveness 235, moves 159 |
| 1037 | 1323 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::run_command_and_measure` | 309 | 158 | 4 | 247 | 247 | constraints 766, liveness 93, moves 67 |
| 1017 | 1699 | hyperfine | `hyperfine::run` | 680 | 223 | 24 | 215 | 215 | constraints 351, liveness 273, moves 143 |
| 974 | 2085 | hyperfine | `hyperfine::export::markup::MarkupExporter::table_results` | 717 | 393 | 32 | 362 | 362 | constraints 357, liveness 235, moves 135 |
| 954 | 1462 | hyperfine | `hyperfine::timer::execute_and_measure` | 251 | 98 | 4 | 152 | 152 | constraints 680, liveness 113, moves 65 |
| 942 | 1916 | hyperfine | `hyperfine::quantity::statistics::median` | 164 | 59 | 7 | 59 | 59 | constraints 795, liveness 43, moves 38 |
| 881 | 1121 | hyperfine | `hyperfine::export::<ExportManager>::write_results` | 554 | 181 | 13 | 190 | 190 | constraints 328, liveness 223, moves 137 |
| 877 | 3520 | hyperfine | `hyperfine::metric::<Metric>::summarize` | 260 | 179 | 10 | 210 | 210 | constraints 616, moves 71, liveness 71 |
| 876 | 1738 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::calibrate` | 475 | 205 | 18 | 217 | 217 | constraints 427, liveness 175, moves 116 |
| 803 | 1806 | hyperfine | `hyperfine::command::parse_environment` | 488 | 182 | 14 | 163 | 163 | constraints 335, liveness 167, moves 120 |
| 755 | 1568 | hyperfine | `hyperfine::command::<Commands<'a>>::from_cli_arguments` | 391 | 142 | 11 | 190 | 190 | constraints 355, liveness 148, moves 95 |
| 714 | 1609 | hyperfine | `hyperfine::command::<Commands<'a>>::get_parameter_scan_commands` | 433 | 146 | 4 | 177 | 177 | constraints 344, liveness 135, moves 95 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 7181 | 21074 | hyperfine | `hyperfine::benchmark::<Benchmark<'a>>::run` | 3222 | 1638 | 84 | 1954 | 1954 | constraints 3878, liveness 1412, moves 722 |
| 4760 | 12592 | hyperfine | `hyperfine::output::report::print` | 2576 | 1158 | 92 | 1147 | 1147 | constraints 2161, liveness 1167, moves 660 |
| 4271 | 9353 | hyperfine | `hyperfine::options::<Options>::from_cli_arguments` | 2088 | 723 | 19 | 675 | 673 | constraints 2320, liveness 755, moves 515 |
| 3083 | 14684 | hyperfine | `hyperfine::command::<Commands<'a>>::build_from_cli_arguments` | 1238 | 925 | 31 | 1162 | 1156 | constraints 1842, liveness 516, moves 278 |
| 2285 | 7789 | hyperfine | `hyperfine::export::csv::<CsvExporter>::serialize` | 898 | 449 | 28 | 431 | 429 | constraints 1391, liveness 337, moves 233 |
| 1812 | 4436 | hyperfine | `build_script_build::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 877, liveness 400, moves 252 |
| 1791 | 4473 | hyperfine | `hyperfine::cli::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 871, liveness 394, moves 249 |
| 1265 | 3109 | hyperfine | `hyperfine::metric::<MetricSelection>::parse_list` | 646 | 401 | 23 | 403 | 401 | constraints 725, liveness 196, moves 131 |
| 978 | 2217 | hyperfine | `hyperfine::benchmark::executor::run_command_and_measure_common` | 767 | 305 | 24 | 254 | 254 | constraints 382, liveness 221, moves 174 |
| 944 | 1323 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::run_command_and_measure` | 309 | 158 | 4 | 247 | 247 | constraints 708, liveness 87, moves 63 |
| 918 | 1916 | hyperfine | `hyperfine::quantity::statistics::median` | 164 | 59 | 7 | 59 | 59 | constraints 787, liveness 41, moves 34 |
| 858 | 1699 | hyperfine | `hyperfine::run` | 680 | 223 | 24 | 215 | 215 | constraints 292, liveness 259, moves 129 |
| 847 | 1462 | hyperfine | `hyperfine::timer::execute_and_measure` | 251 | 98 | 4 | 152 | 152 | constraints 619, liveness 96, moves 58 |
| 829 | 2085 | hyperfine | `hyperfine::export::markup::MarkupExporter::table_results` | 717 | 393 | 32 | 362 | 362 | constraints 305, liveness 207, moves 125 |
| 789 | 1738 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::calibrate` | 475 | 205 | 18 | 217 | 217 | constraints 395, liveness 167, moves 100 |
| 745 | 1121 | hyperfine | `hyperfine::export::<ExportManager>::write_results` | 554 | 181 | 13 | 190 | 190 | constraints 272, liveness 202, moves 125 |
| 701 | 3520 | hyperfine | `hyperfine::metric::<Metric>::summarize` | 260 | 179 | 10 | 210 | 210 | constraints 488, liveness 64, moves 60 |
| 680 | 1806 | hyperfine | `hyperfine::command::parse_environment` | 488 | 182 | 14 | 163 | 163 | constraints 281, liveness 155, moves 109 |
| 642 | 962 | hyperfine | `hyperfine::shell_syntax::<Tokens<'a>>::next` | 533 | 106 | 12 | 147 | 147 | constraints 227, liveness 162, moves 113 |
| 627 | 1568 | hyperfine | `hyperfine::command::<Commands<'a>>::from_cli_arguments` | 391 | 142 | 11 | 190 | 190 | constraints 290, liveness 136, moves 84 |
