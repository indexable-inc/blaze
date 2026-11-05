| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| helix | polonius | 7614 | 7796 | 2943 | 1453 | 1 | 49.4% | 3664 | 48 | 2045 | 105819 |
| helix | nll | 7614 | 7796 | 2943 | 1291 | 0 | 43.9% | 3664 | 40 | 1906 | 103707 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 582 | 29 | 7 | 34 | 311 | 26 | 1 | 179 | 53 | 3 | 148 |
| nll | 522 | 28 | 6 | 33 | 296 | 23 | 0 | 167 | 5 | 2 | 140 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 105819 | 89187 | helix | `helix_term::keymap::default::default` | 36964 | 15331 | 1593 | 12493 | 12493 | liveness 53106, constraints 21192, moves 12490 |
| 18446 | 13048 | helix | `helix_lsp::client::<Client>::initialize` | 15414 | 209 | 10 | 182 | 181 | liveness 8296, moves 3672, walk 3273 |
| 16907 | 45640 | helix | `helix_term::application::<Application>::handle_language_server_message` | 10536 | 2700 | 260 | 2480 | 2477 | liveness 5324, constraints 5169, moves 2999 |
| 16615 | 31762 | helix | `helix_view::handlers::dap::<Editor>::handle_debugger_message` | 9952 | 2142 | 250 | 2041 | 2040 | liveness 6023, constraints 3675, moves 3052 |
| 8980 | 46155 | helix | `helix_view::document::<Document>::save_impl` | 4520 | 1218 | 129 | 1269 | 1268 | constraints 3238, liveness 2122, walk 1347 |
| 8412 | 16019 | helix | `helix_lsp::transport::<Transport>::send` | 4386 | 1146 | 101 | 1386 | 1386 | liveness 2526, constraints 2462, moves 1144 |
| 8084 | 11464 | helix | `helix_term::health::languages` | 2672 | 1852 | 90 | 2108 | 2094 | constraints 3956, liveness 1401, scopes 794 |
| 7107 | 19953 | helix | `helix_term::commands::syntax::syntax_workspace_symbol_picker` | 2936 | 1166 | 103 | 1082 | 1068 | constraints 3260, liveness 1547, walk 779 |
| 6989 | 11058 | helix | `xtask::tasks::highlightcheck` | 2683 | 1089 | 95 | 1038 | 1034 | constraints 3231, liveness 1516, moves 817 |
| 6944 | 15983 | helix | `helix_view::document::<Document>::format` | 3243 | 945 | 81 | 924 | 924 | constraints 3094, liveness 1423, walk 869 |
| 6823 | 25297 | helix | `helix_term::application::<Application>::event_loop_until_idle` | 2969 | 684 | 70 | 1012 | 1010 | constraints 2463, liveness 1625, walk 1223 |
| 6048 | 17086 | helix | `helix_term::commands::global_search` | 2864 | 1070 | 93 | 1109 | 1097 | constraints 2983, liveness 1144, moves 655 |
| 5704 | 15382 | helix | `xtask::tasks::indentcheck` | 2290 | 1190 | 101 | 1117 | 1117 | constraints 2980, liveness 1106, moves 533 |
| 5452 | 6169 | helix | `helix_term::commands::typed::TYPABLE_COMMAND_LIST` | 4643 | 2589 | 0 | 2437 | 2437 | liveness 2225, constraints 1523, moves 601 |
| 5084 | 6606 | helix | `helix_term::handlers::completion::request::request_completions` | 1616 | 483 | 60 | 503 | 501 | constraints 2703, liveness 870, walk 543 |
| 5029 | 10733 | helix | `helix_term::commands::goto_file_impl` | 2189 | 782 | 81 | 750 | 750 | constraints 2292, liveness 1062, moves 598 |
| 4984 | 25942 | helix | `helix_view::editor::<Editor>::wait_event` | 2600 | 625 | 54 | 849 | 848 | constraints 1948, liveness 1181, walk 651 |
| 4572 | 10402 | helix | `helix_term::application::<Application>::new` | 1654 | 428 | 65 | 355 | 351 | constraints 1807, liveness 1315, walk 587 |
| 4555 | 5490 | helix | `helix_tui::backend::termina::<TerminaBackend>::new` | 2212 | 766 | 62 | 658 | 656 | constraints 1276, loans 1060, liveness 824 |
| 4510 | 4848 | helix | `helix_lsp::transport::<Transport>::recv` | 2654 | 598 | 56 | 670 | 670 | liveness 1544, constraints 970, moves 715 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 103707 | 89187 | helix | `helix_term::keymap::default::default` | 36964 | 15331 | 1593 | 12493 | 12493 | liveness 53041, constraints 21076, moves 12871 |
| 18993 | 13048 | helix | `helix_lsp::client::<Client>::initialize` | 15414 | 209 | 10 | 182 | 181 | liveness 8341, moves 4134, walk 3318 |
| 16115 | 45640 | helix | `helix_term::application::<Application>::handle_language_server_message` | 10536 | 2700 | 260 | 2480 | 2477 | liveness 5286, constraints 4966, moves 2907 |
| 15786 | 31762 | helix | `helix_view::handlers::dap::<Editor>::handle_debugger_message` | 9952 | 2142 | 250 | 2041 | 2040 | liveness 5946, constraints 3510, moves 2983 |
| 8433 | 46155 | helix | `helix_view::document::<Document>::save_impl` | 4520 | 1218 | 129 | 1269 | 1268 | constraints 3103, liveness 2103, walk 1289 |
| 7500 | 16019 | helix | `helix_lsp::transport::<Transport>::send` | 4386 | 1146 | 101 | 1386 | 1386 | liveness 2451, constraints 2406, moves 1133 |
| 7230 | 11464 | helix | `helix_term::health::languages` | 2672 | 1852 | 90 | 2108 | 2094 | constraints 3909, liveness 1381, moves 753 |
| 6688 | 19953 | helix | `helix_term::commands::syntax::syntax_workspace_symbol_picker` | 2936 | 1166 | 103 | 1082 | 1068 | constraints 3198, liveness 1490, walk 735 |
| 6581 | 11058 | helix | `xtask::tasks::highlightcheck` | 2683 | 1089 | 95 | 1038 | 1034 | constraints 3147, liveness 1543, moves 823 |
| 6491 | 15983 | helix | `helix_view::document::<Document>::format` | 3243 | 945 | 81 | 924 | 924 | constraints 2858, liveness 1367, walk 867 |
| 6460 | 25297 | helix | `helix_term::application::<Application>::event_loop_until_idle` | 2969 | 684 | 70 | 1012 | 1010 | constraints 2430, liveness 1580, walk 1206 |
| 5733 | 17086 | helix | `helix_term::commands::global_search` | 2864 | 1070 | 93 | 1109 | 1097 | constraints 2891, liveness 1132, moves 615 |
| 5414 | 15382 | helix | `xtask::tasks::indentcheck` | 2290 | 1190 | 101 | 1117 | 1117 | constraints 2910, liveness 1123, moves 537 |
| 5216 | 6169 | helix | `helix_term::commands::typed::TYPABLE_COMMAND_LIST` | 4643 | 2589 | 0 | 2437 | 2437 | liveness 2196, constraints 1505, moves 549 |
| 4814 | 6606 | helix | `helix_term::handlers::completion::request::request_completions` | 1616 | 483 | 60 | 503 | 501 | constraints 2652, liveness 859, walk 535 |
| 4801 | 10733 | helix | `helix_term::commands::goto_file_impl` | 2189 | 782 | 81 | 750 | 750 | constraints 2247, liveness 1055, moves 629 |
| 4696 | 25942 | helix | `helix_view::editor::<Editor>::wait_event` | 2600 | 625 | 54 | 849 | 848 | constraints 1950, liveness 1098, walk 653 |
| 4420 | 10402 | helix | `helix_term::application::<Application>::new` | 1654 | 428 | 65 | 355 | 351 | constraints 1743, liveness 1329, walk 575 |
| 4309 | 5490 | helix | `helix_tui::backend::termina::<TerminaBackend>::new` | 2212 | 766 | 62 | 658 | 656 | constraints 1149, loans 1038, liveness 863 |
| 4306 | 4848 | helix | `helix_lsp::transport::<Transport>::recv` | 2654 | 598 | 56 | 670 | 670 | liveness 1503, constraints 937, moves 752 |
