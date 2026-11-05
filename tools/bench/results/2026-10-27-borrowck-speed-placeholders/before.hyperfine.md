| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| hyperfine | polonius | 304 | 2280 | 198 | 79 | 0 | 39.7% | 222 | 80 | 3683 | 8192 |
| hyperfine | nll | 304 | 2280 | 198 | 70 | 0 | 35.2% | 222 | 63 | 3509 | 7330 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 37 | 1 | 0 | 2 | 13 | 1 | 0 | 9 | 3 | 0 | 8 |
| nll | 34 | 1 | 0 | 2 | 13 | 1 | 0 | 8 | 0 | 0 | 8 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 8192 | 21107 | hyperfine | `hyperfine::benchmark::<Benchmark<'a>>::run` | 3222 | 1638 | 84 | 1954 | 1954 | constraints 3969, liveness 1459, scopes 746 |
| 5206 | 12555 | hyperfine | `hyperfine::output::report::print` | 2576 | 1158 | 92 | 1147 | 1147 | constraints 2184, liveness 1218, moves 735 |
| 4835 | 9542 | hyperfine | `hyperfine::options::<Options>::from_cli_arguments` | 2088 | 723 | 19 | 675 | 673 | constraints 2392, liveness 821, walk 629 |
| 3683 | 14739 | hyperfine | `hyperfine::command::<Commands<'a>>::build_from_cli_arguments` | 1238 | 925 | 31 | 1162 | 1156 | constraints 1946, liveness 563, walk 464 |
| 2697 | 4429 | hyperfine | `hyperfine::cli::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 909, walk 812, liveness 476 |
| 2634 | 4443 | hyperfine | `build_script_build::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 921, walk 794, liveness 433 |
| 2442 | 7758 | hyperfine | `hyperfine::export::csv::<CsvExporter>::serialize` | 898 | 449 | 28 | 431 | 429 | constraints 1445, liveness 354, moves 228 |
| 1437 | 3097 | hyperfine | `hyperfine::metric::<MetricSelection>::parse_list` | 646 | 401 | 23 | 403 | 401 | constraints 768, liveness 227, moves 140 |
| 1157 | 2214 | hyperfine | `hyperfine::benchmark::executor::run_command_and_measure_common` | 767 | 305 | 24 | 254 | 254 | constraints 448, liveness 247, moves 161 |
| 1128 | 1332 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::run_command_and_measure` | 309 | 158 | 4 | 247 | 247 | constraints 772, liveness 108, walk 95 |
| 1085 | 1739 | hyperfine | `hyperfine::run` | 680 | 223 | 24 | 215 | 215 | constraints 350, liveness 298, moves 152 |
| 1017 | 1450 | hyperfine | `hyperfine::timer::execute_and_measure` | 251 | 98 | 4 | 152 | 152 | constraints 701, liveness 123, walk 68 |
| 977 | 2081 | hyperfine | `hyperfine::export::markup::MarkupExporter::table_results` | 717 | 393 | 32 | 362 | 362 | constraints 337, liveness 254, moves 133 |
| 974 | 1942 | hyperfine | `hyperfine::quantity::statistics::median` | 164 | 59 | 7 | 59 | 59 | constraints 789, walk 57, liveness 46 |
| 912 | 1722 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::calibrate` | 475 | 205 | 18 | 217 | 217 | constraints 429, liveness 202, moves 107 |
| 867 | 1591 | hyperfine | `hyperfine::command::<Commands<'a>>::get_parameter_scan_commands` | 433 | 146 | 4 | 177 | 177 | constraints 355, walk 170, liveness 156 |
| 851 | 1183 | hyperfine | `hyperfine::export::<ExportManager>::write_results` | 554 | 181 | 13 | 190 | 190 | constraints 317, liveness 207, moves 135 |
| 842 | 1646 | hyperfine | `hyperfine::command::<Commands<'a>>::from_cli_arguments` | 391 | 142 | 11 | 190 | 190 | constraints 335, liveness 156, walk 139 |
| 829 | 3517 | hyperfine | `hyperfine::metric::<Metric>::summarize` | 260 | 179 | 10 | 210 | 210 | constraints 548, liveness 73, moves 69 |
| 781 | 1786 | hyperfine | `hyperfine::command::parse_environment` | 488 | 182 | 14 | 163 | 163 | constraints 312, liveness 164, moves 120 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 7330 | 21107 | hyperfine | `hyperfine::benchmark::<Benchmark<'a>>::run` | 3222 | 1638 | 84 | 1954 | 1954 | constraints 3864, liveness 1421, moves 739 |
| 4888 | 12555 | hyperfine | `hyperfine::output::report::print` | 2576 | 1158 | 92 | 1147 | 1147 | constraints 2136, liveness 1245, moves 684 |
| 4699 | 9542 | hyperfine | `hyperfine::options::<Options>::from_cli_arguments` | 2088 | 723 | 19 | 675 | 673 | constraints 2331, liveness 830, walk 626 |
| 3509 | 14739 | hyperfine | `hyperfine::command::<Commands<'a>>::build_from_cli_arguments` | 1238 | 925 | 31 | 1162 | 1156 | constraints 1864, liveness 569, walk 481 |
| 2527 | 4429 | hyperfine | `hyperfine::cli::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 878, walk 794, liveness 435 |
| 2500 | 4443 | hyperfine | `build_script_build::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 853, walk 810, liveness 416 |
| 2318 | 7758 | hyperfine | `hyperfine::export::csv::<CsvExporter>::serialize` | 898 | 449 | 28 | 431 | 429 | constraints 1381, liveness 370, moves 220 |
| 1357 | 3097 | hyperfine | `hyperfine::metric::<MetricSelection>::parse_list` | 646 | 401 | 23 | 403 | 401 | constraints 775, liveness 204, moves 138 |
| 1064 | 2214 | hyperfine | `hyperfine::benchmark::executor::run_command_and_measure_common` | 767 | 305 | 24 | 254 | 254 | constraints 386, liveness 269, moves 152 |
| 1036 | 1332 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::run_command_and_measure` | 309 | 158 | 4 | 247 | 247 | constraints 728, liveness 100, walk 79 |
| 941 | 1739 | hyperfine | `hyperfine::run` | 680 | 223 | 24 | 215 | 215 | constraints 309, liveness 265, moves 129 |
| 930 | 1942 | hyperfine | `hyperfine::quantity::statistics::median` | 164 | 59 | 7 | 59 | 59 | constraints 749, liveness 58, walk 56 |
| 928 | 1450 | hyperfine | `hyperfine::timer::execute_and_measure` | 251 | 98 | 4 | 152 | 152 | constraints 647, liveness 108, walk 71 |
| 852 | 2081 | hyperfine | `hyperfine::export::markup::MarkupExporter::table_results` | 717 | 393 | 32 | 362 | 362 | constraints 300, liveness 228, moves 126 |
| 822 | 1722 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::calibrate` | 475 | 205 | 18 | 217 | 217 | constraints 386, liveness 193, moves 99 |
| 782 | 1591 | hyperfine | `hyperfine::command::<Commands<'a>>::get_parameter_scan_commands` | 433 | 146 | 4 | 177 | 177 | constraints 315, walk 157, liveness 148 |
| 780 | 1183 | hyperfine | `hyperfine::export::<ExportManager>::write_results` | 554 | 181 | 13 | 190 | 190 | constraints 273, liveness 216, moves 135 |
| 725 | 3517 | hyperfine | `hyperfine::metric::<Metric>::summarize` | 260 | 179 | 10 | 210 | 210 | constraints 500, liveness 69, moves 61 |
| 716 | 1646 | hyperfine | `hyperfine::command::<Commands<'a>>::from_cli_arguments` | 391 | 142 | 11 | 190 | 190 | constraints 297, liveness 141, walk 116 |
| 677 | 1786 | hyperfine | `hyperfine::command::parse_environment` | 488 | 182 | 14 | 163 | 163 | constraints 275, liveness 155, moves 109 |
