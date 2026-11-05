| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| hyperfine | polonius | 304 | 2320 | 206 | 81 | 0 | 39.4% | 230 | 83 | 3807 | 8451 |
| hyperfine | nll | 304 | 2320 | 206 | 71 | 0 | 34.6% | 230 | 68 | 3535 | 7469 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 39 | 1 | 0 | 2 | 13 | 1 | 0 | 9 | 3 | 0 | 8 |
| nll | 35 | 1 | 0 | 2 | 13 | 1 | 0 | 8 | 0 | 0 | 8 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 8451 | 22120 | hyperfine | `hyperfine::benchmark::<Benchmark<'a>>::run` | 3222 | 1638 | 84 | 1954 | 1954 | constraints 4217, liveness 1481, scopes 753 |
| 5210 | 13083 | hyperfine | `hyperfine::output::report::print` | 2576 | 1158 | 92 | 1147 | 1147 | constraints 2239, liveness 1208, moves 712 |
| 4993 | 9793 | hyperfine | `hyperfine::options::<Options>::from_cli_arguments` | 2082 | 723 | 19 | 675 | 673 | constraints 2480, liveness 890, walk 628 |
| 3807 | 15164 | hyperfine | `hyperfine::command::<Commands<'a>>::build_from_cli_arguments` | 1238 | 925 | 31 | 1162 | 1156 | constraints 2028, liveness 568, walk 488 |
| 2736 | 4801 | hyperfine | `build_script_build::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 945, walk 813, liveness 457 |
| 2733 | 4660 | hyperfine | `hyperfine::cli::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 944, walk 826, liveness 469 |
| 2567 | 8216 | hyperfine | `hyperfine::export::csv::<CsvExporter>::serialize` | 898 | 449 | 28 | 431 | 429 | constraints 1525, liveness 365, moves 273 |
| 1478 | 3251 | hyperfine | `hyperfine::metric::<MetricSelection>::parse_list` | 646 | 401 | 23 | 403 | 401 | constraints 815, liveness 229, moves 139 |
| 1185 | 2331 | hyperfine | `hyperfine::benchmark::executor::run_command_and_measure_common` | 767 | 305 | 24 | 254 | 254 | constraints 442, liveness 248, moves 159 |
| 1168 | 1360 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::run_command_and_measure` | 309 | 158 | 4 | 247 | 247 | constraints 805, liveness 106, walk 90 |
| 1108 | 1850 | hyperfine | `hyperfine::run` | 680 | 223 | 24 | 215 | 215 | constraints 381, liveness 295, moves 140 |
| 1031 | 1532 | hyperfine | `hyperfine::timer::execute_and_measure` | 251 | 98 | 4 | 152 | 152 | constraints 713, liveness 114, walk 78 |
| 1005 | 1982 | hyperfine | `hyperfine::quantity::statistics::median` | 164 | 59 | 7 | 59 | 59 | constraints 817, walk 58, liveness 47 |
| 991 | 2179 | hyperfine | `hyperfine::export::markup::MarkupExporter::table_results` | 717 | 393 | 32 | 362 | 362 | constraints 360, liveness 240, moves 135 |
| 921 | 1788 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::calibrate` | 475 | 205 | 18 | 217 | 217 | constraints 437, liveness 197, moves 116 |
| 892 | 1206 | hyperfine | `hyperfine::export::<ExportManager>::write_results` | 554 | 181 | 13 | 190 | 190 | constraints 328, liveness 231, moves 138 |
| 887 | 1676 | hyperfine | `hyperfine::command::<Commands<'a>>::get_parameter_scan_commands` | 433 | 146 | 4 | 177 | 177 | constraints 365, walk 175, liveness 160 |
| 868 | 3725 | hyperfine | `hyperfine::metric::<Metric>::summarize` | 260 | 179 | 10 | 210 | 210 | constraints 592, liveness 81, moves 69 |
| 857 | 1631 | hyperfine | `hyperfine::command::<Commands<'a>>::from_cli_arguments` | 391 | 142 | 11 | 190 | 190 | constraints 367, liveness 156, walk 132 |
| 790 | 1841 | hyperfine | `hyperfine::command::parse_environment` | 488 | 182 | 14 | 163 | 163 | constraints 324, liveness 162, moves 131 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 7469 | 22120 | hyperfine | `hyperfine::benchmark::<Benchmark<'a>>::run` | 3222 | 1638 | 84 | 1954 | 1954 | constraints 3975, liveness 1465, moves 715 |
| 4860 | 13083 | hyperfine | `hyperfine::output::report::print` | 2576 | 1158 | 92 | 1147 | 1147 | constraints 2164, liveness 1197, moves 695 |
| 4718 | 9793 | hyperfine | `hyperfine::options::<Options>::from_cli_arguments` | 2082 | 723 | 19 | 675 | 673 | constraints 2363, liveness 809, walk 620 |
| 3535 | 15164 | hyperfine | `hyperfine::command::<Commands<'a>>::build_from_cli_arguments` | 1238 | 925 | 31 | 1162 | 1156 | constraints 1922, liveness 548, walk 462 |
| 2557 | 4660 | hyperfine | `hyperfine::cli::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 893, walk 783, liveness 444 |
| 2537 | 4801 | hyperfine | `build_script_build::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 885, walk 785, liveness 444 |
| 2376 | 8216 | hyperfine | `hyperfine::export::csv::<CsvExporter>::serialize` | 898 | 449 | 28 | 431 | 429 | constraints 1443, liveness 362, moves 215 |
| 1342 | 3251 | hyperfine | `hyperfine::metric::<MetricSelection>::parse_list` | 646 | 401 | 23 | 403 | 401 | constraints 747, liveness 214, moves 133 |
| 1187 | 1982 | hyperfine | `hyperfine::quantity::statistics::median` | 164 | 59 | 7 | 59 | 59 | constraints 1010, walk 59, liveness 48 |
| 1076 | 1360 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::run_command_and_measure` | 309 | 158 | 4 | 247 | 247 | constraints 761, liveness 113, walk 83 |
| 1064 | 2331 | hyperfine | `hyperfine::benchmark::executor::run_command_and_measure_common` | 767 | 305 | 24 | 254 | 254 | constraints 379, liveness 238, walk 159 |
| 959 | 1532 | hyperfine | `hyperfine::timer::execute_and_measure` | 251 | 98 | 4 | 152 | 152 | constraints 667, liveness 108, walk 82 |
| 937 | 1850 | hyperfine | `hyperfine::run` | 680 | 223 | 24 | 215 | 215 | constraints 315, liveness 264, moves 129 |
| 858 | 2179 | hyperfine | `hyperfine::export::markup::MarkupExporter::table_results` | 717 | 393 | 32 | 362 | 362 | constraints 314, liveness 217, moves 128 |
| 827 | 1788 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::calibrate` | 475 | 205 | 18 | 217 | 217 | constraints 407, liveness 178, moves 97 |
| 807 | 1676 | hyperfine | `hyperfine::command::<Commands<'a>>::get_parameter_scan_commands` | 433 | 146 | 4 | 177 | 177 | constraints 335, walk 174, liveness 138 |
| 771 | 3725 | hyperfine | `hyperfine::metric::<Metric>::summarize` | 260 | 179 | 10 | 210 | 210 | constraints 517, moves 87, liveness 72 |
| 760 | 1206 | hyperfine | `hyperfine::export::<ExportManager>::write_results` | 554 | 181 | 13 | 190 | 190 | constraints 285, liveness 198, moves 124 |
| 758 | 1631 | hyperfine | `hyperfine::command::<Commands<'a>>::from_cli_arguments` | 391 | 142 | 11 | 190 | 190 | constraints 310, liveness 161, walk 123 |
| 690 | 1841 | hyperfine | `hyperfine::command::parse_environment` | 488 | 182 | 14 | 163 | 163 | constraints 290, liveness 155, moves 107 |
