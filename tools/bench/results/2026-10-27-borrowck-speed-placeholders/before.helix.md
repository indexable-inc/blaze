| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| helix | polonius | 7614 | 7840 | 3007 | 1478 | 1 | 49.1% | 3737 | 48 | 2156 | 109629 |
| helix | nll | 7614 | 7840 | 3007 | 1311 | 0 | 43.6% | 3737 | 40 | 1975 | 106058 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 593 | 30 | 7 | 35 | 318 | 26 | 1 | 184 | 53 | 3 | 150 |
| nll | 533 | 28 | 6 | 33 | 301 | 24 | 0 | 168 | 5 | 2 | 143 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 109629 | 92668 | helix | `helix_term::keymap::default::default` | 36964 | 15331 | 1593 | 12493 | 12493 | liveness 55193, constraints 21280, moves 13687 |
| 18110 | 11726 | helix | `helix_lsp::client::<Client>::initialize` | 15414 | 209 | 10 | 182 | 181 | liveness 8238, moves 3553, walk 3059 |
| 17596 | 44576 | helix | `helix_term::application::<Application>::handle_language_server_message` | 10536 | 2700 | 260 | 2480 | 2477 | liveness 5930, constraints 5074, moves 3109 |
| 16858 | 29131 | helix | `helix_view::handlers::dap::<Editor>::handle_debugger_message` | 9952 | 2142 | 250 | 2041 | 2040 | liveness 6194, constraints 3545, moves 3128 |
| 8987 | 46488 | helix | `helix_view::document::<Document>::save_impl` | 4520 | 1218 | 129 | 1269 | 1268 | constraints 3191, liveness 2199, walk 1346 |
| 8434 | 12034 | helix | `helix_term::health::languages` | 2672 | 1852 | 90 | 2108 | 2094 | constraints 4237, liveness 1457, scopes 780 |
| 8194 | 13961 | helix | `helix_lsp::transport::<Transport>::send` | 4386 | 1146 | 101 | 1386 | 1386 | liveness 2450, constraints 2283, moves 1176 |
| 7390 | 20670 | helix | `helix_term::commands::syntax::syntax_workspace_symbol_picker` | 2936 | 1166 | 103 | 1082 | 1068 | constraints 3425, liveness 1570, walk 830 |
| 7173 | 11516 | helix | `xtask::tasks::highlightcheck` | 2683 | 1089 | 95 | 1038 | 1034 | constraints 3335, liveness 1568, moves 851 |
| 6965 | 26096 | helix | `helix_term::application::<Application>::event_loop_until_idle` | 2969 | 684 | 70 | 1012 | 1010 | constraints 2550, liveness 1639, walk 1245 |
| 6912 | 15681 | helix | `helix_view::document::<Document>::format` | 3243 | 945 | 81 | 924 | 924 | constraints 3030, liveness 1448, walk 856 |
| 6240 | 17749 | helix | `helix_term::commands::global_search` | 2864 | 1070 | 93 | 1109 | 1097 | constraints 3090, liveness 1181, moves 651 |
| 5913 | 15850 | helix | `xtask::tasks::indentcheck` | 2290 | 1190 | 101 | 1117 | 1117 | constraints 3115, liveness 1156, moves 551 |
| 5437 | 6512 | helix | `helix_term::commands::typed::TYPABLE_COMMAND_LIST` | 4643 | 2589 | 0 | 2437 | 2437 | liveness 2072, constraints 1641, moves 602 |
| 5319 | 7006 | helix | `helix_term::handlers::completion::request::request_completions` | 1616 | 483 | 60 | 503 | 501 | constraints 2877, liveness 894, walk 569 |
| 5202 | 10537 | helix | `helix_term::commands::goto_file_impl` | 2189 | 782 | 81 | 750 | 750 | constraints 2454, liveness 1048, moves 608 |
| 5147 | 26449 | helix | `helix_view::editor::<Editor>::wait_event` | 2600 | 625 | 54 | 849 | 848 | constraints 2082, liveness 1176, walk 681 |
| 4756 | 10822 | helix | `helix_term::application::<Application>::new` | 1654 | 428 | 65 | 355 | 351 | constraints 1884, liveness 1402, walk 597 |
| 4661 | 11386 | helix | `helix_term::commands::lsp::workspace_symbol_picker` | 1639 | 624 | 35 | 576 | 560 | constraints 2465, liveness 791, walk 496 |
| 4621 | 8729 | helix | `helix_term::commands::jumplist_picker` | 937 | 796 | 42 | 1020 | 1000 | constraints 3159, liveness 415, walk 372 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 106058 | 92668 | helix | `helix_term::keymap::default::default` | 36964 | 15331 | 1593 | 12493 | 12493 | liveness 54936, constraints 21217, moves 13338 |
| 17668 | 11726 | helix | `helix_lsp::client::<Client>::initialize` | 15414 | 209 | 10 | 182 | 181 | liveness 8347, moves 3649, walk 3011 |
| 16710 | 44576 | helix | `helix_term::application::<Application>::handle_language_server_message` | 10536 | 2700 | 260 | 2480 | 2477 | liveness 5696, constraints 4996, moves 2970 |
| 16290 | 29131 | helix | `helix_view::handlers::dap::<Editor>::handle_debugger_message` | 9952 | 2142 | 250 | 2041 | 2040 | liveness 6171, constraints 3513, moves 3089 |
| 8637 | 46488 | helix | `helix_view::document::<Document>::save_impl` | 4520 | 1218 | 129 | 1269 | 1268 | constraints 3167, liveness 2158, walk 1322 |
| 7563 | 12034 | helix | `helix_term::health::languages` | 2672 | 1852 | 90 | 2108 | 2094 | constraints 4154, liveness 1443, moves 762 |
| 7410 | 13961 | helix | `helix_lsp::transport::<Transport>::send` | 4386 | 1146 | 101 | 1386 | 1386 | liveness 2452, constraints 2254, moves 1178 |
| 6999 | 20670 | helix | `helix_term::commands::syntax::syntax_workspace_symbol_picker` | 2936 | 1166 | 103 | 1082 | 1068 | constraints 3360, liveness 1536, walk 796 |
| 6801 | 11516 | helix | `xtask::tasks::highlightcheck` | 2683 | 1089 | 95 | 1038 | 1034 | constraints 3260, liveness 1556, moves 844 |
| 6653 | 26096 | helix | `helix_term::application::<Application>::event_loop_until_idle` | 2969 | 684 | 70 | 1012 | 1010 | constraints 2474, liveness 1689, walk 1257 |
| 6587 | 15681 | helix | `helix_view::document::<Document>::format` | 3243 | 945 | 81 | 924 | 924 | constraints 2935, liveness 1406, walk 857 |
| 5951 | 17749 | helix | `helix_term::commands::global_search` | 2864 | 1070 | 93 | 1109 | 1097 | constraints 3041, liveness 1156, moves 663 |
| 5581 | 15850 | helix | `xtask::tasks::indentcheck` | 2290 | 1190 | 101 | 1117 | 1117 | constraints 3048, liveness 1142, moves 546 |
| 5144 | 6512 | helix | `helix_term::commands::typed::TYPABLE_COMMAND_LIST` | 4643 | 2589 | 0 | 2437 | 2437 | liveness 2053, constraints 1553, moves 580 |
| 5073 | 7006 | helix | `helix_term::handlers::completion::request::request_completions` | 1616 | 483 | 60 | 503 | 501 | constraints 2807, liveness 922, walk 574 |
| 5031 | 10537 | helix | `helix_term::commands::goto_file_impl` | 2189 | 782 | 81 | 750 | 750 | constraints 2398, liveness 1075, moves 655 |
| 4824 | 26449 | helix | `helix_view::editor::<Editor>::wait_event` | 2600 | 625 | 54 | 849 | 848 | constraints 2013, liveness 1142, walk 669 |
| 4506 | 10822 | helix | `helix_term::application::<Application>::new` | 1654 | 428 | 65 | 355 | 351 | constraints 1809, liveness 1322, walk 609 |
| 4464 | 11386 | helix | `helix_term::commands::lsp::workspace_symbol_picker` | 1639 | 624 | 35 | 576 | 560 | constraints 2453, liveness 715, walk 508 |
| 4422 | 5532 | helix | `helix_tui::backend::termina::<TerminaBackend>::new` | 2212 | 766 | 62 | 658 | 656 | constraints 1184, loans 1027, liveness 911 |
