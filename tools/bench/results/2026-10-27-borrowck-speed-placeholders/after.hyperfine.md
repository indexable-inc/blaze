| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| hyperfine | polonius | 304 | 2240 | 197 | 79 | 0 | 40.1% | 221 | 83 | 3696 | 8270 |
| hyperfine | nll | 304 | 2240 | 197 | 70 | 0 | 35.5% | 221 | 65 | 3456 | 7410 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 37 | 1 | 0 | 2 | 13 | 1 | 0 | 9 | 3 | 0 | 8 |
| nll | 34 | 1 | 0 | 2 | 12 | 1 | 0 | 8 | 0 | 0 | 8 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 8270 | 21152 | hyperfine | `hyperfine::benchmark::<Benchmark<'a>>::run` | 3222 | 1638 | 84 | 1954 | 1954 | constraints 4011, liveness 1498, scopes 751 |
| 5097 | 12703 | hyperfine | `hyperfine::output::report::print` | 2576 | 1158 | 92 | 1147 | 1147 | constraints 2183, liveness 1175, moves 670 |
| 4873 | 9460 | hyperfine | `hyperfine::options::<Options>::from_cli_arguments` | 2088 | 723 | 19 | 675 | 673 | constraints 2366, liveness 866, walk 642 |
| 3696 | 14480 | hyperfine | `hyperfine::command::<Commands<'a>>::build_from_cli_arguments` | 1238 | 925 | 31 | 1162 | 1156 | constraints 1943, liveness 542, walk 487 |
| 2707 | 4443 | hyperfine | `hyperfine::cli::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 928, walk 818, liveness 459 |
| 2654 | 4395 | hyperfine | `build_script_build::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 925, walk 802, liveness 433 |
| 2439 | 7720 | hyperfine | `hyperfine::export::csv::<CsvExporter>::serialize` | 898 | 449 | 28 | 431 | 429 | constraints 1439, liveness 370, moves 230 |
| 1432 | 3058 | hyperfine | `hyperfine::metric::<MetricSelection>::parse_list` | 646 | 401 | 23 | 403 | 401 | constraints 776, liveness 229, moves 141 |
| 1179 | 2219 | hyperfine | `hyperfine::benchmark::executor::run_command_and_measure_common` | 767 | 305 | 24 | 254 | 254 | constraints 443, liveness 256, moves 175 |
| 1140 | 1342 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::run_command_and_measure` | 309 | 158 | 4 | 247 | 247 | constraints 782, liveness 108, moves 90 |
| 1084 | 1725 | hyperfine | `hyperfine::run` | 680 | 223 | 24 | 215 | 215 | constraints 350, liveness 301, moves 153 |
| 1014 | 2103 | hyperfine | `hyperfine::export::markup::MarkupExporter::table_results` | 717 | 393 | 32 | 362 | 362 | constraints 351, liveness 239, moves 161 |
| 1007 | 1449 | hyperfine | `hyperfine::timer::execute_and_measure` | 251 | 98 | 4 | 152 | 152 | constraints 702, liveness 116, walk 66 |
| 995 | 1954 | hyperfine | `hyperfine::quantity::statistics::median` | 164 | 59 | 7 | 59 | 59 | constraints 797, walk 59, liveness 56 |
| 927 | 1727 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::calibrate` | 475 | 205 | 18 | 217 | 217 | constraints 442, liveness 203, moves 108 |
| 871 | 1601 | hyperfine | `hyperfine::command::<Commands<'a>>::get_parameter_scan_commands` | 433 | 146 | 4 | 177 | 177 | constraints 354, walk 171, liveness 155 |
| 863 | 1118 | hyperfine | `hyperfine::export::<ExportManager>::write_results` | 554 | 181 | 13 | 190 | 190 | constraints 310, liveness 212, moves 134 |
| 862 | 1575 | hyperfine | `hyperfine::command::<Commands<'a>>::from_cli_arguments` | 391 | 142 | 11 | 190 | 190 | constraints 347, liveness 175, walk 136 |
| 842 | 3511 | hyperfine | `hyperfine::metric::<Metric>::summarize` | 260 | 179 | 10 | 210 | 210 | constraints 574, moves 73, liveness 73 |
| 783 | 1819 | hyperfine | `hyperfine::command::parse_environment` | 488 | 182 | 14 | 163 | 163 | constraints 317, liveness 161, moves 121 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 7410 | 21152 | hyperfine | `hyperfine::benchmark::<Benchmark<'a>>::run` | 3222 | 1638 | 84 | 1954 | 1954 | constraints 3885, liveness 1440, moves 742 |
| 4817 | 12703 | hyperfine | `hyperfine::output::report::print` | 2576 | 1158 | 92 | 1147 | 1147 | constraints 2154, liveness 1180, moves 657 |
| 4740 | 9460 | hyperfine | `hyperfine::options::<Options>::from_cli_arguments` | 2088 | 723 | 19 | 675 | 673 | constraints 2389, liveness 830, walk 614 |
| 3456 | 14480 | hyperfine | `hyperfine::command::<Commands<'a>>::build_from_cli_arguments` | 1238 | 925 | 31 | 1162 | 1156 | constraints 1874, liveness 534, walk 445 |
| 2576 | 4443 | hyperfine | `hyperfine::cli::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 891, walk 791, liveness 452 |
| 2523 | 4395 | hyperfine | `build_script_build::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 878, walk 803, liveness 418 |
| 2346 | 7720 | hyperfine | `hyperfine::export::csv::<CsvExporter>::serialize` | 898 | 449 | 28 | 431 | 429 | constraints 1415, liveness 358, moves 222 |
| 1309 | 3058 | hyperfine | `hyperfine::metric::<MetricSelection>::parse_list` | 646 | 401 | 23 | 403 | 401 | constraints 717, liveness 210, moves 135 |
| 1043 | 2219 | hyperfine | `hyperfine::benchmark::executor::run_command_and_measure_common` | 767 | 305 | 24 | 254 | 254 | constraints 397, liveness 236, moves 153 |
| 1039 | 1342 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::run_command_and_measure` | 309 | 158 | 4 | 247 | 247 | constraints 741, liveness 101, walk 80 |
| 964 | 1725 | hyperfine | `hyperfine::run` | 680 | 223 | 24 | 215 | 215 | constraints 327, liveness 267, moves 140 |
| 930 | 1954 | hyperfine | `hyperfine::quantity::statistics::median` | 164 | 59 | 7 | 59 | 59 | constraints 754, walk 65, liveness 41 |
| 897 | 1449 | hyperfine | `hyperfine::timer::execute_and_measure` | 251 | 98 | 4 | 152 | 152 | constraints 630, liveness 106, moves 61 |
| 848 | 2103 | hyperfine | `hyperfine::export::markup::MarkupExporter::table_results` | 717 | 393 | 32 | 362 | 362 | constraints 309, liveness 213, moves 128 |
| 824 | 1727 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::calibrate` | 475 | 205 | 18 | 217 | 217 | constraints 402, liveness 180, moves 99 |
| 768 | 1601 | hyperfine | `hyperfine::command::<Commands<'a>>::get_parameter_scan_commands` | 433 | 146 | 4 | 177 | 177 | constraints 319, walk 155, liveness 134 |
| 753 | 1118 | hyperfine | `hyperfine::export::<ExportManager>::write_results` | 554 | 181 | 13 | 190 | 190 | constraints 274, liveness 200, moves 123 |
| 746 | 1575 | hyperfine | `hyperfine::command::<Commands<'a>>::from_cli_arguments` | 391 | 142 | 11 | 190 | 190 | constraints 304, liveness 156, walk 121 |
| 725 | 3511 | hyperfine | `hyperfine::metric::<Metric>::summarize` | 260 | 179 | 10 | 210 | 210 | constraints 500, liveness 68, moves 62 |
| 672 | 1819 | hyperfine | `hyperfine::command::parse_environment` | 488 | 182 | 14 | 163 | 163 | constraints 274, liveness 152, moves 110 |
