| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| helix | polonius | 7614 | 7861 | 2978 | 1391 | 1 | 46.7% | 3702 | 47 | 1989 | 103432 |
| helix | nll | 7614 | 7861 | 2978 | 1220 | 0 | 41.0% | 3702 | 40 | 1833 | 100176 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 592 | 29 | 7 | 34 | 299 | 25 | 1 | 180 | 52 | 3 | 89 |
| nll | 527 | 27 | 6 | 32 | 281 | 22 | 0 | 166 | 5 | 2 | 84 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 103432 | 95032 | helix | `helix_term::keymap::default::default` | 36964 | 15331 | 1593 | 12493 | 12493 | liveness 53730, constraints 22015, moves 12682 |
| 16194 | 44429 | helix | `helix_term::application::<Application>::handle_language_server_message` | 10536 | 2700 | 260 | 2480 | 2477 | liveness 5202, constraints 4913, moves 2950 |
| 15588 | 28949 | helix | `helix_view::handlers::dap::<Editor>::handle_debugger_message` | 9952 | 2142 | 250 | 2041 | 2040 | liveness 5969, constraints 3523, moves 3092 |
| 15250 | 11652 | helix | `helix_lsp::client::<Client>::initialize` | 15414 | 209 | 10 | 182 | 181 | liveness 7906, moves 3381, entry_sets 1382 |
| 8168 | 11338 | helix | `xtask::tasks::highlightcheck` | 2683 | 1089 | 95 | 1038 | 1034 | constraints 4333, liveness 1619, moves 876 |
| 8096 | 11878 | helix | `helix_term::health::languages` | 2672 | 1852 | 90 | 2108 | 2094 | constraints 4157, liveness 1312, moves 786 |
| 8042 | 45853 | helix | `helix_view::document::<Document>::save_impl` | 4520 | 1218 | 129 | 1269 | 1268 | constraints 3110, liveness 2068, moves 1237 |
| 7578 | 14420 | helix | `helix_lsp::transport::<Transport>::send` | 4386 | 1146 | 101 | 1386 | 1386 | liveness 2373, constraints 2251, moves 1145 |
| 6514 | 20630 | helix | `helix_term::commands::syntax::syntax_workspace_symbol_picker` | 2936 | 1166 | 103 | 1082 | 1068 | constraints 3386, liveness 1282, moves 715 |
| 6137 | 15479 | helix | `helix_view::document::<Document>::format` | 3243 | 945 | 81 | 924 | 924 | constraints 2854, liveness 1341, moves 834 |
| 5994 | 26309 | helix | `helix_term::application::<Application>::event_loop_until_idle` | 2969 | 684 | 70 | 1012 | 1010 | constraints 2480, liveness 1639, moves 782 |
| 5839 | 19141 | helix | `helix_term::commands::global_search` | 2864 | 1070 | 93 | 1109 | 1097 | constraints 3090, liveness 1044, moves 658 |
| 5703 | 15552 | helix | `xtask::tasks::indentcheck` | 2290 | 1190 | 101 | 1117 | 1117 | constraints 3047, liveness 1066, moves 578 |
| 4919 | 10740 | helix | `helix_term::commands::goto_file_impl` | 2189 | 782 | 81 | 750 | 750 | constraints 2416, liveness 1035, moves 585 |
| 4832 | 6353 | helix | `helix_term::commands::typed::TYPABLE_COMMAND_LIST` | 4643 | 2589 | 0 | 2437 | 2437 | liveness 1632, constraints 1610, moves 569 |
| 4811 | 6959 | helix | `helix_term::handlers::completion::request::request_completions` | 1616 | 483 | 60 | 503 | 501 | constraints 2831, liveness 809, moves 449 |
| 4520 | 26017 | helix | `helix_view::editor::<Editor>::wait_event` | 2600 | 625 | 54 | 849 | 848 | constraints 1999, liveness 1068, moves 609 |
| 4257 | 8586 | helix | `helix_term::commands::jumplist_picker` | 937 | 796 | 42 | 1020 | 1000 | constraints 3046, liveness 375, moves 267 |
| 4144 | 11227 | helix | `helix_term::commands::lsp::workspace_symbol_picker` | 1639 | 624 | 35 | 576 | 560 | constraints 2412, liveness 645, moves 456 |
| 4086 | 10727 | helix | `helix_term::application::<Application>::new` | 1654 | 428 | 65 | 355 | 351 | constraints 1830, liveness 1102, moves 484 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 100176 | 95032 | helix | `helix_term::keymap::default::default` | 36964 | 15331 | 1593 | 12493 | 12493 | liveness 51845, constraints 22045, moves 13098 |
| 15640 | 44429 | helix | `helix_term::application::<Application>::handle_language_server_message` | 10536 | 2700 | 260 | 2480 | 2477 | liveness 5154, constraints 4852, moves 2976 |
| 15337 | 11652 | helix | `helix_lsp::client::<Client>::initialize` | 15414 | 209 | 10 | 182 | 181 | liveness 8071, moves 3831, constraints 1033 |
| 14810 | 28949 | helix | `helix_view::handlers::dap::<Editor>::handle_debugger_message` | 9952 | 2142 | 250 | 2041 | 2040 | liveness 5783, constraints 3432, moves 3030 |
| 7962 | 11338 | helix | `xtask::tasks::highlightcheck` | 2683 | 1089 | 95 | 1038 | 1034 | constraints 4435, liveness 1604, moves 900 |
| 7585 | 45853 | helix | `helix_view::document::<Document>::save_impl` | 4520 | 1218 | 129 | 1269 | 1268 | constraints 3040, liveness 2065, moves 1179 |
| 7162 | 11878 | helix | `helix_term::health::languages` | 2672 | 1852 | 90 | 2108 | 2094 | constraints 4049, liveness 1269, moves 767 |
| 6836 | 14420 | helix | `helix_lsp::transport::<Transport>::send` | 4386 | 1146 | 101 | 1386 | 1386 | liveness 2348, constraints 2271, moves 1117 |
| 6117 | 20630 | helix | `helix_term::commands::syntax::syntax_workspace_symbol_picker` | 2936 | 1166 | 103 | 1082 | 1068 | constraints 3217, liveness 1262, moves 702 |
| 5766 | 15479 | helix | `helix_view::document::<Document>::format` | 3243 | 945 | 81 | 924 | 924 | constraints 2761, liveness 1280, moves 831 |
| 5455 | 19141 | helix | `helix_term::commands::global_search` | 2864 | 1070 | 93 | 1109 | 1097 | constraints 2980, liveness 999, moves 634 |
| 5439 | 26309 | helix | `helix_term::application::<Application>::event_loop_until_idle` | 2969 | 684 | 70 | 1012 | 1010 | constraints 2433, liveness 1455, moves 755 |
| 5310 | 15552 | helix | `xtask::tasks::indentcheck` | 2290 | 1190 | 101 | 1117 | 1117 | constraints 2964, liveness 1041, moves 535 |
| 4608 | 10740 | helix | `helix_term::commands::goto_file_impl` | 2189 | 782 | 81 | 750 | 750 | constraints 2327, liveness 1034, moves 573 |
| 4527 | 6353 | helix | `helix_term::commands::typed::TYPABLE_COMMAND_LIST` | 4643 | 2589 | 0 | 2437 | 2437 | liveness 1604, constraints 1497, moves 558 |
| 4507 | 6959 | helix | `helix_term::handlers::completion::request::request_completions` | 1616 | 483 | 60 | 503 | 501 | constraints 2732, liveness 813, moves 452 |
| 4207 | 26017 | helix | `helix_view::editor::<Editor>::wait_event` | 2600 | 625 | 54 | 849 | 848 | constraints 1947, liveness 998, moves 611 |
| 4007 | 8586 | helix | `helix_term::commands::jumplist_picker` | 937 | 796 | 42 | 1020 | 1000 | constraints 2947, liveness 378, moves 271 |
| 3941 | 11227 | helix | `helix_term::commands::lsp::workspace_symbol_picker` | 1639 | 624 | 35 | 576 | 560 | constraints 2347, liveness 650, moves 449 |
| 3826 | 10727 | helix | `helix_term::application::<Application>::new` | 1654 | 428 | 65 | 355 | 351 | constraints 1766, liveness 1028, moves 448 |
