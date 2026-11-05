| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| hyperfine | polonius | 304 | 2237 | 197 | 75 | 0 | 37.9% | 220 | 81 | 3368 | 7995 |
| hyperfine | nll | 304 | 2237 | 197 | 65 | 0 | 33.1% | 220 | 60 | 3108 | 7153 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 37 | 1 | 0 | 2 | 13 | 1 | 0 | 9 | 3 | 0 | 4 |
| nll | 34 | 1 | 0 | 2 | 12 | 1 | 0 | 8 | 0 | 0 | 4 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 7995 | 20983 | hyperfine | `hyperfine::benchmark::<Benchmark<'a>>::run` | 3222 | 1638 | 84 | 1954 | 1954 | constraints 3921, liveness 1447, scopes 769 |
| 5108 | 12686 | hyperfine | `hyperfine::output::report::print` | 2576 | 1158 | 92 | 1147 | 1147 | constraints 2167, liveness 1204, moves 704 |
| 4430 | 9451 | hyperfine | `hyperfine::options::<Options>::from_cli_arguments` | 2088 | 723 | 19 | 675 | 673 | constraints 2342, liveness 789, moves 530 |
| 3368 | 14390 | hyperfine | `hyperfine::command::<Commands<'a>>::build_from_cli_arguments` | 1238 | 925 | 31 | 1162 | 1156 | constraints 1921, liveness 552, moves 314 |
| 2412 | 7676 | hyperfine | `hyperfine::export::csv::<CsvExporter>::serialize` | 898 | 449 | 28 | 431 | 429 | constraints 1442, liveness 340, moves 229 |
| 2044 | 4451 | hyperfine | `build_script_build::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 994, liveness 423, moves 281 |
| 1879 | 4441 | hyperfine | `hyperfine::cli::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 898, liveness 393, moves 256 |
| 1387 | 3059 | hyperfine | `hyperfine::metric::<MetricSelection>::parse_list` | 646 | 401 | 23 | 403 | 401 | constraints 764, liveness 220, moves 137 |
| 1090 | 2179 | hyperfine | `hyperfine::benchmark::executor::run_command_and_measure_common` | 767 | 305 | 24 | 254 | 254 | constraints 435, liveness 247, moves 159 |
| 1051 | 1315 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::run_command_and_measure` | 309 | 158 | 4 | 247 | 247 | constraints 780, liveness 94, moves 67 |
| 997 | 1675 | hyperfine | `hyperfine::run` | 680 | 223 | 24 | 215 | 215 | constraints 363, liveness 257, moves 142 |
| 972 | 1436 | hyperfine | `hyperfine::timer::execute_and_measure` | 251 | 98 | 4 | 152 | 152 | constraints 701, liveness 106, moves 69 |
| 968 | 2056 | hyperfine | `hyperfine::export::markup::MarkupExporter::table_results` | 717 | 393 | 32 | 362 | 362 | constraints 347, liveness 239, moves 138 |
| 950 | 1914 | hyperfine | `hyperfine::quantity::statistics::median` | 164 | 59 | 7 | 59 | 59 | constraints 803, liveness 43, moves 38 |
| 897 | 1134 | hyperfine | `hyperfine::export::<ExportManager>::write_results` | 554 | 181 | 13 | 190 | 190 | constraints 342, liveness 220, moves 138 |
| 880 | 1724 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::calibrate` | 475 | 205 | 18 | 217 | 217 | constraints 438, liveness 177, moves 105 |
| 825 | 3493 | hyperfine | `hyperfine::metric::<Metric>::summarize` | 260 | 179 | 10 | 210 | 210 | constraints 570, liveness 69, moves 68 |
| 774 | 1769 | hyperfine | `hyperfine::command::parse_environment` | 488 | 182 | 14 | 163 | 163 | constraints 314, liveness 172, moves 119 |
| 746 | 1604 | hyperfine | `hyperfine::command::<Commands<'a>>::from_cli_arguments` | 391 | 142 | 11 | 190 | 190 | constraints 346, liveness 151, moves 93 |
| 713 | 933 | hyperfine | `hyperfine::shell_syntax::<Tokens<'a>>::next` | 533 | 106 | 12 | 147 | 147 | constraints 254, liveness 182, moves 109 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 7153 | 20983 | hyperfine | `hyperfine::benchmark::<Benchmark<'a>>::run` | 3222 | 1638 | 84 | 1954 | 1954 | constraints 3847, liveness 1392, moves 732 |
| 4726 | 12686 | hyperfine | `hyperfine::output::report::print` | 2576 | 1158 | 92 | 1147 | 1147 | constraints 2118, liveness 1136, moves 679 |
| 4273 | 9451 | hyperfine | `hyperfine::options::<Options>::from_cli_arguments` | 2088 | 723 | 19 | 675 | 673 | constraints 2305, liveness 766, moves 508 |
| 3108 | 14390 | hyperfine | `hyperfine::command::<Commands<'a>>::build_from_cli_arguments` | 1238 | 925 | 31 | 1162 | 1156 | constraints 1844, liveness 533, moves 280 |
| 2276 | 7676 | hyperfine | `hyperfine::export::csv::<CsvExporter>::serialize` | 898 | 449 | 28 | 431 | 429 | constraints 1410, liveness 333, moves 224 |
| 1811 | 4451 | hyperfine | `build_script_build::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 869, liveness 405, moves 255 |
| 1771 | 4441 | hyperfine | `hyperfine::cli::build_command` | 1390 | 645 | 28 | 684 | 682 | constraints 854, liveness 378, moves 246 |
| 1269 | 3059 | hyperfine | `hyperfine::metric::<MetricSelection>::parse_list` | 646 | 401 | 23 | 403 | 401 | constraints 731, liveness 193, moves 130 |
| 976 | 2179 | hyperfine | `hyperfine::benchmark::executor::run_command_and_measure_common` | 767 | 305 | 24 | 254 | 254 | constraints 374, liveness 237, moves 162 |
| 964 | 1315 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::run_command_and_measure` | 309 | 158 | 4 | 247 | 247 | constraints 726, liveness 90, moves 62 |
| 915 | 1914 | hyperfine | `hyperfine::quantity::statistics::median` | 164 | 59 | 7 | 59 | 59 | constraints 785, liveness 40, moves 33 |
| 864 | 1436 | hyperfine | `hyperfine::timer::execute_and_measure` | 251 | 98 | 4 | 152 | 152 | constraints 623, liveness 97, moves 61 |
| 853 | 1675 | hyperfine | `hyperfine::run` | 680 | 223 | 24 | 215 | 215 | constraints 288, liveness 256, moves 130 |
| 839 | 2056 | hyperfine | `hyperfine::export::markup::MarkupExporter::table_results` | 717 | 393 | 32 | 362 | 362 | constraints 303, liveness 215, moves 129 |
| 784 | 1724 | hyperfine | `hyperfine::benchmark::executor::<ShellExecutor<'_>>::calibrate` | 475 | 205 | 18 | 217 | 217 | constraints 383, liveness 174, moves 98 |
| 745 | 1134 | hyperfine | `hyperfine::export::<ExportManager>::write_results` | 554 | 181 | 13 | 190 | 190 | constraints 279, liveness 194, moves 126 |
| 715 | 3493 | hyperfine | `hyperfine::metric::<Metric>::summarize` | 260 | 179 | 10 | 210 | 210 | constraints 486, liveness 65, moves 60 |
| 689 | 1769 | hyperfine | `hyperfine::command::parse_environment` | 488 | 182 | 14 | 163 | 163 | constraints 294, liveness 151, moves 109 |
| 637 | 933 | hyperfine | `hyperfine::shell_syntax::<Tokens<'a>>::next` | 533 | 106 | 12 | 147 | 147 | constraints 229, liveness 158, moves 101 |
| 636 | 1604 | hyperfine | `hyperfine::command::<Commands<'a>>::from_cli_arguments` | 391 | 142 | 11 | 190 | 190 | constraints 286, liveness 145, moves 86 |
