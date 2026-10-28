| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| helix | polonius | 7614 | 7852 | 2986 | 1406 | 1 | 47.1% | 3719 | 47 | 2005 | 109033 |
| helix | nll | 7614 | 7852 | 2986 | 1232 | 0 | 41.3% | 3719 | 40 | 1842 | 105592 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 597 | 29 | 7 | 35 | 305 | 24 | 1 | 182 | 53 | 3 | 90 |
| nll | 529 | 29 | 6 | 32 | 288 | 22 | 0 | 167 | 5 | 2 | 84 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 109033 | 92268 | helix | `helix_term::keymap::default::default` | 36964 | 15331 | 1593 | 12493 | 12493 | liveness 57116, constraints 22228, moves 13751 |
| 16831 | 45360 | helix | `helix_term::application::<Application>::handle_language_server_message` | 10536 | 2700 | 260 | 2480 | 2477 | liveness 5537, constraints 5085, moves 3046 |
| 15861 | 28649 | helix | `helix_view::handlers::dap::<Editor>::handle_debugger_message` | 9952 | 2142 | 250 | 2041 | 2040 | liveness 6109, constraints 3617, moves 3104 |
| 15156 | 11413 | helix | `helix_lsp::client::<Client>::initialize` | 15414 | 209 | 10 | 182 | 181 | liveness 8336, moves 3384, constraints 1060 |
| 8093 | 45248 | helix | `helix_view::document::<Document>::save_impl` | 4520 | 1218 | 129 | 1269 | 1268 | constraints 3164, liveness 2122, moves 1219 |
| 8059 | 12006 | helix | `helix_term::health::languages` | 2672 | 1852 | 90 | 2108 | 2094 | constraints 4184, liveness 1308, scopes 783 |
| 7786 | 13729 | helix | `helix_lsp::transport::<Transport>::send` | 4386 | 1146 | 101 | 1386 | 1386 | liveness 2500, constraints 2297, moves 1158 |
| 6862 | 11658 | helix | `xtask::tasks::highlightcheck` | 2683 | 1089 | 95 | 1038 | 1034 | constraints 3281, liveness 1499, moves 851 |
| 6615 | 21440 | helix | `helix_term::commands::syntax::syntax_workspace_symbol_picker` | 2936 | 1166 | 103 | 1082 | 1068 | constraints 3453, liveness 1278, moves 730 |
| 6165 | 15421 | helix | `helix_view::document::<Document>::format` | 3243 | 945 | 81 | 924 | 924 | constraints 2886, liveness 1345, moves 851 |
| 5910 | 18227 | helix | `helix_term::commands::global_search` | 2864 | 1070 | 93 | 1109 | 1097 | constraints 3121, liveness 1097, moves 641 |
| 5895 | 26284 | helix | `helix_term::application::<Application>::event_loop_until_idle` | 2969 | 684 | 70 | 1012 | 1010 | constraints 2509, liveness 1538, moves 786 |
| 5711 | 15792 | helix | `xtask::tasks::indentcheck` | 2290 | 1190 | 101 | 1117 | 1117 | constraints 3113, liveness 1036, moves 556 |
| 5041 | 10766 | helix | `helix_term::commands::goto_file_impl` | 2189 | 782 | 81 | 750 | 750 | constraints 2468, liveness 1090, moves 597 |
| 4807 | 6975 | helix | `helix_term::handlers::completion::request::request_completions` | 1616 | 483 | 60 | 503 | 501 | constraints 2774, liveness 827, moves 476 |
| 4727 | 6332 | helix | `helix_term::commands::typed::TYPABLE_COMMAND_LIST` | 4643 | 2589 | 0 | 2437 | 2437 | constraints 1569, liveness 1512, moves 610 |
| 4627 | 26158 | helix | `helix_view::editor::<Editor>::wait_event` | 2600 | 625 | 54 | 849 | 848 | constraints 2031, liveness 1108, moves 631 |
| 4322 | 8881 | helix | `helix_term::commands::jumplist_picker` | 937 | 796 | 42 | 1020 | 1000 | constraints 3084, liveness 401, moves 291 |
| 4216 | 11516 | helix | `helix_term::commands::lsp::workspace_symbol_picker` | 1639 | 624 | 35 | 576 | 560 | constraints 2455, liveness 669, moves 474 |
| 4169 | 10925 | helix | `helix_term::application::<Application>::new` | 1654 | 428 | 65 | 355 | 351 | constraints 1853, liveness 1167, moves 488 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 105592 | 92268 | helix | `helix_term::keymap::default::default` | 36964 | 15331 | 1593 | 12493 | 12493 | liveness 56894, constraints 21995, moves 13180 |
| 16273 | 11413 | helix | `helix_lsp::client::<Client>::initialize` | 15414 | 209 | 10 | 182 | 181 | liveness 8430, moves 3930, entry_sets 1393 |
| 16038 | 45360 | helix | `helix_term::application::<Application>::handle_language_server_message` | 10536 | 2700 | 260 | 2480 | 2477 | liveness 5384, constraints 4918, moves 3003 |
| 15242 | 28649 | helix | `helix_view::handlers::dap::<Editor>::handle_debugger_message` | 9952 | 2142 | 250 | 2041 | 2040 | liveness 6044, constraints 3501, moves 3091 |
| 7836 | 45248 | helix | `helix_view::document::<Document>::save_impl` | 4520 | 1218 | 129 | 1269 | 1268 | constraints 3088, liveness 2117, moves 1163 |
| 7161 | 12006 | helix | `helix_term::health::languages` | 2672 | 1852 | 90 | 2108 | 2094 | constraints 4058, liveness 1307, moves 760 |
| 6996 | 13729 | helix | `helix_lsp::transport::<Transport>::send` | 4386 | 1146 | 101 | 1386 | 1386 | liveness 2482, constraints 2261, moves 1138 |
| 6409 | 11658 | helix | `xtask::tasks::highlightcheck` | 2683 | 1089 | 95 | 1038 | 1034 | constraints 3254, liveness 1465, moves 818 |
| 6135 | 21440 | helix | `helix_term::commands::syntax::syntax_workspace_symbol_picker` | 2936 | 1166 | 103 | 1082 | 1068 | constraints 3224, liveness 1265, moves 711 |
| 5797 | 15421 | helix | `helix_view::document::<Document>::format` | 3243 | 945 | 81 | 924 | 924 | constraints 2767, liveness 1316, moves 829 |
| 5564 | 18227 | helix | `helix_term::commands::global_search` | 2864 | 1070 | 93 | 1109 | 1097 | constraints 2998, liveness 1089, moves 621 |
| 5455 | 26284 | helix | `helix_term::application::<Application>::event_loop_until_idle` | 2969 | 684 | 70 | 1012 | 1010 | constraints 2455, liveness 1453, moves 772 |
| 5360 | 15792 | helix | `xtask::tasks::indentcheck` | 2290 | 1190 | 101 | 1117 | 1117 | constraints 2979, liveness 1040, moves 546 |
| 4726 | 10766 | helix | `helix_term::commands::goto_file_impl` | 2189 | 782 | 81 | 750 | 750 | constraints 2378, liveness 1076, moves 589 |
| 4607 | 6975 | helix | `helix_term::handlers::completion::request::request_completions` | 1616 | 483 | 60 | 503 | 501 | constraints 2786, liveness 858, moves 447 |
| 4347 | 6332 | helix | `helix_term::commands::typed::TYPABLE_COMMAND_LIST` | 4643 | 2589 | 0 | 2437 | 2437 | constraints 1484, liveness 1424, moves 560 |
| 4258 | 26158 | helix | `helix_view::editor::<Editor>::wait_event` | 2600 | 625 | 54 | 849 | 848 | constraints 1953, liveness 1036, moves 604 |
| 4084 | 8881 | helix | `helix_term::commands::jumplist_picker` | 937 | 796 | 42 | 1020 | 1000 | constraints 3028, liveness 374, moves 263 |
| 4016 | 11516 | helix | `helix_term::commands::lsp::workspace_symbol_picker` | 1639 | 624 | 35 | 576 | 560 | constraints 2387, liveness 649, moves 450 |
| 3844 | 10925 | helix | `helix_term::application::<Application>::new` | 1654 | 428 | 65 | 355 | 351 | constraints 1783, liveness 1068, moves 440 |
