| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| helix | polonius | 7614 | 7879 | 3001 | 1398 | 1 | 46.6% | 3715 | 47 | 1976 | 108116 |
| helix | nll | 7614 | 7879 | 3001 | 1227 | 0 | 40.9% | 3715 | 40 | 1829 | 105579 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 592 | 29 | 6 | 34 | 307 | 24 | 1 | 181 | 52 | 3 | 89 |
| nll | 523 | 28 | 6 | 32 | 291 | 22 | 0 | 166 | 5 | 2 | 83 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 108116 | 92423 | helix | `helix_term::keymap::default::default` | 36964 | 15331 | 1593 | 12493 | 12493 | liveness 58080, constraints 21337, moves 13546 |
| 16657 | 29081 | helix | `helix_view::handlers::dap::<Editor>::handle_debugger_message` | 9952 | 2142 | 250 | 2041 | 2040 | liveness 6371, constraints 3968, moves 3217 |
| 16546 | 46422 | helix | `helix_term::application::<Application>::handle_language_server_message` | 10536 | 2700 | 260 | 2480 | 2477 | liveness 5294, constraints 5038, moves 3065 |
| 15639 | 11982 | helix | `helix_lsp::client::<Client>::initialize` | 15414 | 209 | 10 | 182 | 181 | liveness 8309, moves 3471, constraints 1323 |
| 8486 | 51557 | helix | `helix_view::document::<Document>::save_impl` | 4520 | 1218 | 129 | 1269 | 1268 | constraints 3411, liveness 2198, moves 1257 |
| 8015 | 11762 | helix | `helix_term::health::languages` | 2672 | 1852 | 90 | 2108 | 2094 | constraints 4139, liveness 1341, scopes 780 |
| 7965 | 16024 | helix | `helix_lsp::transport::<Transport>::send` | 4386 | 1146 | 101 | 1386 | 1386 | constraints 2488, liveness 2471, moves 1149 |
| 6810 | 11339 | helix | `xtask::tasks::highlightcheck` | 2683 | 1089 | 95 | 1038 | 1034 | constraints 3274, liveness 1495, moves 808 |
| 6498 | 20731 | helix | `helix_term::commands::syntax::syntax_workspace_symbol_picker` | 2936 | 1166 | 103 | 1082 | 1068 | constraints 3310, liveness 1293, moves 740 |
| 6481 | 17595 | helix | `helix_view::document::<Document>::format` | 3243 | 945 | 81 | 924 | 924 | constraints 3186, liveness 1351, moves 846 |
| 6185 | 28199 | helix | `helix_term::application::<Application>::event_loop_until_idle` | 2969 | 684 | 70 | 1012 | 1010 | constraints 2697, liveness 1574, moves 815 |
| 5952 | 19249 | helix | `helix_term::commands::global_search` | 2864 | 1070 | 93 | 1109 | 1097 | constraints 3147, liveness 1119, moves 640 |
| 5708 | 15627 | helix | `xtask::tasks::indentcheck` | 2290 | 1190 | 101 | 1117 | 1117 | constraints 3021, liveness 1087, moves 570 |
| 5020 | 11027 | helix | `helix_term::commands::goto_file_impl` | 2189 | 782 | 81 | 750 | 750 | constraints 2463, liveness 1055, moves 629 |
| 4846 | 7035 | helix | `helix_term::handlers::completion::request::request_completions` | 1616 | 483 | 60 | 503 | 501 | constraints 2838, liveness 837, moves 451 |
| 4743 | 6381 | helix | `helix_term::commands::typed::TYPABLE_COMMAND_LIST` | 4643 | 2589 | 0 | 2437 | 2437 | constraints 1618, liveness 1471, moves 621 |
| 4673 | 27645 | helix | `helix_view::editor::<Editor>::wait_event` | 2600 | 625 | 54 | 849 | 848 | constraints 2099, liveness 1042, moves 643 |
| 4402 | 11203 | helix | `helix_term::application::<Application>::new` | 1654 | 428 | 65 | 355 | 351 | constraints 1947, liveness 1280, moves 488 |
| 4324 | 4600 | helix | `helix_lsp::transport::<Transport>::recv` | 2654 | 598 | 56 | 670 | 670 | liveness 1727, constraints 950, moves 717 |
| 4253 | 8613 | helix | `helix_term::commands::jumplist_picker` | 937 | 796 | 42 | 1020 | 1000 | constraints 3000, liveness 412, moves 285 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 105579 | 92423 | helix | `helix_term::keymap::default::default` | 36964 | 15331 | 1593 | 12493 | 12493 | liveness 58751, constraints 21179, moves 13052 |
| 16561 | 11982 | helix | `helix_lsp::client::<Client>::initialize` | 15414 | 209 | 10 | 182 | 181 | liveness 8352, moves 3939, entry_sets 1400 |
| 15906 | 46422 | helix | `helix_term::application::<Application>::handle_language_server_message` | 10536 | 2700 | 260 | 2480 | 2477 | liveness 5416, constraints 4788, moves 3008 |
| 15579 | 29081 | helix | `helix_view::handlers::dap::<Editor>::handle_debugger_message` | 9952 | 2142 | 250 | 2041 | 2040 | liveness 6189, constraints 3677, moves 3102 |
| 7888 | 51557 | helix | `helix_view::document::<Document>::save_impl` | 4520 | 1218 | 129 | 1269 | 1268 | constraints 3229, liveness 2116, moves 1195 |
| 7161 | 11762 | helix | `helix_term::health::languages` | 2672 | 1852 | 90 | 2108 | 2094 | constraints 4070, liveness 1315, moves 736 |
| 6987 | 16024 | helix | `helix_lsp::transport::<Transport>::send` | 4386 | 1146 | 101 | 1386 | 1386 | liveness 2442, constraints 2303, moves 1138 |
| 6390 | 11339 | helix | `xtask::tasks::highlightcheck` | 2683 | 1089 | 95 | 1038 | 1034 | constraints 3182, liveness 1500, moves 827 |
| 6105 | 20731 | helix | `helix_term::commands::syntax::syntax_workspace_symbol_picker` | 2936 | 1166 | 103 | 1082 | 1068 | constraints 3141, liveness 1272, moves 731 |
| 5938 | 17595 | helix | `helix_view::document::<Document>::format` | 3243 | 945 | 81 | 924 | 924 | constraints 2863, liveness 1332, moves 842 |
| 5628 | 19249 | helix | `helix_term::commands::global_search` | 2864 | 1070 | 93 | 1109 | 1097 | constraints 3060, liveness 1076, moves 627 |
| 5533 | 28199 | helix | `helix_term::application::<Application>::event_loop_until_idle` | 2969 | 684 | 70 | 1012 | 1010 | constraints 2494, liveness 1451, moves 801 |
| 5327 | 15627 | helix | `xtask::tasks::indentcheck` | 2290 | 1190 | 101 | 1117 | 1117 | constraints 2947, liveness 1045, moves 546 |
| 4659 | 11027 | helix | `helix_term::commands::goto_file_impl` | 2189 | 782 | 81 | 750 | 750 | constraints 2334, liveness 1073, moves 582 |
| 4541 | 7035 | helix | `helix_term::handlers::completion::request::request_completions` | 1616 | 483 | 60 | 503 | 501 | constraints 2731, liveness 825, moves 466 |
| 4354 | 6381 | helix | `helix_term::commands::typed::TYPABLE_COMMAND_LIST` | 4643 | 2589 | 0 | 2437 | 2437 | constraints 1471, liveness 1436, moves 582 |
| 4296 | 27645 | helix | `helix_view::editor::<Editor>::wait_event` | 2600 | 625 | 54 | 849 | 848 | constraints 1977, liveness 1029, moves 622 |
| 4005 | 8613 | helix | `helix_term::commands::jumplist_picker` | 937 | 796 | 42 | 1020 | 1000 | constraints 2940, liveness 364, moves 274 |
| 3949 | 11310 | helix | `helix_term::commands::lsp::workspace_symbol_picker` | 1639 | 624 | 35 | 576 | 560 | constraints 2326, liveness 662, moves 463 |
| 3936 | 11203 | helix | `helix_term::application::<Application>::new` | 1654 | 428 | 65 | 355 | 351 | constraints 1795, liveness 1108, moves 461 |
