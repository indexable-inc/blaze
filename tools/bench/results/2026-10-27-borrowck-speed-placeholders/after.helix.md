| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| helix | polonius | 7614 | 7610 | 2922 | 1439 | 0 | 49.2% | 3597 | 46 | 2077 | 106621 |
| helix | nll | 7614 | 7610 | 2922 | 1283 | 0 | 43.9% | 3597 | 40 | 1926 | 102003 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 574 | 29 | 7 | 34 | 311 | 25 | 0 | 179 | 52 | 3 | 146 |
| nll | 520 | 28 | 6 | 32 | 294 | 23 | 0 | 165 | 5 | 2 | 139 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 106621 | 90760 | helix | `helix_term::keymap::default::default` | 36964 | 15331 | 1593 | 12493 | 12493 | liveness 53287, constraints 21800, moves 12996 |
| 17376 | 11332 | helix | `helix_lsp::client::<Client>::initialize` | 15414 | 209 | 10 | 182 | 181 | liveness 8220, moves 3484, walk 3008 |
| 16754 | 43634 | helix | `helix_term::application::<Application>::handle_language_server_message` | 10536 | 2700 | 260 | 2480 | 2477 | liveness 5507, constraints 4889, moves 2988 |
| 16410 | 27887 | helix | `helix_view::handlers::dap::<Editor>::handle_debugger_message` | 9952 | 2142 | 250 | 2041 | 2040 | liveness 6028, constraints 3442, moves 3128 |
| 8685 | 44809 | helix | `helix_view::document::<Document>::save_impl` | 4520 | 1218 | 129 | 1269 | 1268 | constraints 3089, liveness 2111, walk 1290 |
| 8192 | 11622 | helix | `helix_term::health::languages` | 2672 | 1852 | 90 | 2108 | 2094 | constraints 4044, liveness 1426, scopes 787 |
| 8105 | 13544 | helix | `helix_lsp::transport::<Transport>::send` | 4386 | 1146 | 101 | 1386 | 1386 | liveness 2470, constraints 2246, moves 1135 |
| 7135 | 20154 | helix | `helix_term::commands::syntax::syntax_workspace_symbol_picker` | 2936 | 1166 | 103 | 1082 | 1068 | constraints 3307, liveness 1531, walk 754 |
| 6968 | 11116 | helix | `xtask::tasks::highlightcheck` | 2683 | 1089 | 95 | 1038 | 1034 | constraints 3217, liveness 1506, moves 831 |
| 6905 | 25543 | helix | `helix_term::application::<Application>::event_loop_until_idle` | 2969 | 684 | 70 | 1012 | 1010 | constraints 2486, liveness 1675, walk 1222 |
| 6554 | 14863 | helix | `helix_view::document::<Document>::format` | 3243 | 945 | 81 | 924 | 924 | constraints 2820, liveness 1366, moves 834 |
| 6067 | 17126 | helix | `helix_term::commands::global_search` | 2864 | 1070 | 93 | 1109 | 1097 | constraints 3001, liveness 1157, moves 654 |
| 5706 | 15281 | helix | `xtask::tasks::indentcheck` | 2290 | 1190 | 101 | 1117 | 1117 | constraints 2970, liveness 1113, moves 549 |
| 5329 | 6176 | helix | `helix_term::commands::typed::TYPABLE_COMMAND_LIST` | 4643 | 2589 | 0 | 2437 | 2437 | liveness 2097, constraints 1546, moves 588 |
| 5134 | 6731 | helix | `helix_term::handlers::completion::request::request_completions` | 1616 | 483 | 60 | 503 | 501 | constraints 2719, liveness 889, walk 560 |
| 5051 | 10188 | helix | `helix_term::commands::goto_file_impl` | 2189 | 782 | 81 | 750 | 750 | constraints 2312, liveness 1065, moves 605 |
| 4922 | 25651 | helix | `helix_view::editor::<Editor>::wait_event` | 2600 | 625 | 54 | 849 | 848 | constraints 1982, liveness 1107, walk 655 |
| 4734 | 10378 | helix | `helix_term::application::<Application>::new` | 1654 | 428 | 65 | 355 | 351 | constraints 1801, liveness 1453, walk 596 |
| 4459 | 11211 | helix | `helix_term::commands::lsp::workspace_symbol_picker` | 1639 | 624 | 35 | 576 | 560 | constraints 2327, liveness 718, walk 503 |
| 4448 | 5205 | helix | `helix_tui::backend::termina::<TerminaBackend>::new` | 2212 | 766 | 62 | 658 | 656 | constraints 1178, loans 1050, liveness 853 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 102003 | 90760 | helix | `helix_term::keymap::default::default` | 36964 | 15331 | 1593 | 12493 | 12493 | liveness 52345, constraints 21189, moves 12420 |
| 17699 | 11332 | helix | `helix_lsp::client::<Client>::initialize` | 15414 | 209 | 10 | 182 | 181 | liveness 8169, moves 3915, walk 3003 |
| 15983 | 43634 | helix | `helix_term::application::<Application>::handle_language_server_message` | 10536 | 2700 | 260 | 2480 | 2477 | liveness 5253, constraints 4755, moves 2991 |
| 15622 | 27887 | helix | `helix_view::handlers::dap::<Editor>::handle_debugger_message` | 9952 | 2142 | 250 | 2041 | 2040 | liveness 5867, constraints 3405, moves 3016 |
| 8324 | 44809 | helix | `helix_view::document::<Document>::save_impl` | 4520 | 1218 | 129 | 1269 | 1268 | constraints 3003, liveness 2110, walk 1276 |
| 7257 | 13544 | helix | `helix_lsp::transport::<Transport>::send` | 4386 | 1146 | 101 | 1386 | 1386 | liveness 2391, constraints 2222, moves 1130 |
| 7251 | 11622 | helix | `helix_term::health::languages` | 2672 | 1852 | 90 | 2108 | 2094 | constraints 3929, liveness 1365, moves 764 |
| 6810 | 20154 | helix | `helix_term::commands::syntax::syntax_workspace_symbol_picker` | 2936 | 1166 | 103 | 1082 | 1068 | constraints 3223, liveness 1508, walk 776 |
| 6569 | 11116 | helix | `xtask::tasks::highlightcheck` | 2683 | 1089 | 95 | 1038 | 1034 | constraints 3199, liveness 1501, moves 806 |
| 6445 | 25543 | helix | `helix_term::application::<Application>::event_loop_until_idle` | 2969 | 684 | 70 | 1012 | 1010 | constraints 2442, liveness 1607, walk 1194 |
| 6308 | 14863 | helix | `helix_view::document::<Document>::format` | 3243 | 945 | 81 | 924 | 924 | constraints 2762, liveness 1330, moves 860 |
| 5778 | 17126 | helix | `helix_term::commands::global_search` | 2864 | 1070 | 93 | 1109 | 1097 | constraints 2939, liveness 1146, moves 620 |
| 5400 | 15281 | helix | `xtask::tasks::indentcheck` | 2290 | 1190 | 101 | 1117 | 1117 | constraints 2949, liveness 1093, moves 534 |
| 5086 | 6176 | helix | `helix_term::commands::typed::TYPABLE_COMMAND_LIST` | 4643 | 2589 | 0 | 2437 | 2437 | liveness 2092, constraints 1482, moves 574 |
| 4898 | 6731 | helix | `helix_term::handlers::completion::request::request_completions` | 1616 | 483 | 60 | 503 | 501 | constraints 2690, liveness 881, walk 564 |
| 4782 | 10188 | helix | `helix_term::commands::goto_file_impl` | 2189 | 782 | 81 | 750 | 750 | constraints 2251, liveness 1060, moves 618 |
| 4743 | 25651 | helix | `helix_view::editor::<Editor>::wait_event` | 2600 | 625 | 54 | 849 | 848 | constraints 1969, liveness 1109, walk 668 |
| 4423 | 10378 | helix | `helix_term::application::<Application>::new` | 1654 | 428 | 65 | 355 | 351 | constraints 1752, liveness 1314, walk 584 |
| 4299 | 11211 | helix | `helix_term::commands::lsp::workspace_symbol_picker` | 1639 | 624 | 35 | 576 | 560 | constraints 2338, liveness 725, moves 470 |
| 4293 | 5205 | helix | `helix_tui::backend::termina::<TerminaBackend>::new` | 2212 | 766 | 62 | 658 | 656 | constraints 1158, loans 1032, liveness 826 |
