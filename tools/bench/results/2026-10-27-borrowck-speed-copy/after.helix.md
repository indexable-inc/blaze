| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| helix | polonius | 7614 | 8037 | 3124 | 1509 | 1 | 48.3% | 3808 | 49 | 2181 | 110520 |
| helix | nll | 7614 | 8037 | 3124 | 1334 | 0 | 42.7% | 3808 | 41 | 2041 | 104855 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 618 | 30 | 7 | 35 | 321 | 27 | 1 | 181 | 53 | 4 | 151 |
| nll | 554 | 28 | 7 | 33 | 301 | 24 | 0 | 166 | 5 | 3 | 143 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 110520 | 96681 | helix | `helix_term::keymap::default::default` | 36964 | 15331 | 1593 | 12493 | 12493 | liveness 56626, constraints 21273, moves 13335 |
| 17902 | 12288 | helix | `helix_lsp::client::<Client>::initialize` | 15414 | 209 | 10 | 182 | 181 | liveness 8256, moves 3543, walk 3094 |
| 17167 | 47401 | helix | `helix_term::application::<Application>::handle_language_server_message` | 10535 | 2700 | 260 | 2480 | 2477 | liveness 5458, constraints 5248, moves 2967 |
| 16847 | 29954 | helix | `helix_view::handlers::dap::<Editor>::handle_debugger_message` | 9952 | 2142 | 250 | 2041 | 2040 | liveness 6233, constraints 3622, moves 3039 |
| 9144 | 49169 | helix | `helix_view::document::<Document>::save_impl` | 4517 | 1218 | 129 | 1269 | 1268 | constraints 3290, liveness 2204, walk 1379 |
| 8575 | 12507 | helix | `helix_term::health::languages` | 2672 | 1852 | 90 | 2108 | 2094 | constraints 4359, liveness 1439, scopes 780 |
| 8324 | 14533 | helix | `helix_lsp::transport::<Transport>::send` | 4386 | 1146 | 101 | 1386 | 1386 | liveness 2478, constraints 2401, moves 1130 |
| 7613 | 21639 | helix | `helix_term::commands::syntax::syntax_workspace_symbol_picker` | 2936 | 1166 | 103 | 1082 | 1068 | constraints 3568, liveness 1631, walk 806 |
| 7323 | 11973 | helix | `xtask::tasks::highlightcheck` | 2682 | 1089 | 95 | 1038 | 1034 | constraints 3437, liveness 1607, moves 843 |
| 7181 | 27372 | helix | `helix_term::application::<Application>::event_loop_until_idle` | 2969 | 684 | 70 | 1012 | 1010 | constraints 2637, liveness 1731, walk 1286 |
| 7094 | 16056 | helix | `helix_view::document::<Document>::format` | 3243 | 945 | 81 | 924 | 924 | constraints 3007, liveness 1571, walk 897 |
| 6390 | 18517 | helix | `helix_term::commands::global_search` | 2864 | 1070 | 93 | 1109 | 1097 | constraints 3198, liveness 1235, moves 665 |
| 6088 | 16500 | helix | `xtask::tasks::indentcheck` | 2290 | 1190 | 101 | 1117 | 1117 | constraints 3249, liveness 1180, moves 558 |
| 5451 | 7286 | helix | `helix_term::handlers::completion::request::request_completions` | 1616 | 483 | 60 | 503 | 501 | constraints 2947, liveness 921, walk 620 |
| 5446 | 6658 | helix | `helix_term::commands::typed::TYPABLE_COMMAND_LIST` | 4643 | 2589 | 0 | 2437 | 2437 | liveness 2060, constraints 1616, moves 590 |
| 5304 | 11043 | helix | `helix_term::commands::goto_file_impl` | 2189 | 782 | 81 | 750 | 750 | constraints 2480, liveness 1105, moves 605 |
| 5104 | 27285 | helix | `helix_view::editor::<Editor>::wait_event` | 2600 | 625 | 54 | 849 | 848 | constraints 2117, liveness 1127, walk 655 |
| 4847 | 11197 | helix | `helix_term::application::<Application>::new` | 1653 | 428 | 65 | 355 | 351 | constraints 1921, liveness 1414, walk 630 |
| 4771 | 12034 | helix | `helix_term::commands::lsp::workspace_symbol_picker` | 1639 | 624 | 35 | 576 | 560 | constraints 2577, liveness 792, walk 502 |
| 4733 | 9126 | helix | `helix_term::commands::jumplist_picker` | 937 | 796 | 42 | 1020 | 1000 | constraints 3235, liveness 424, walk 383 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 104855 | 96681 | helix | `helix_term::keymap::default::default` | 36964 | 15331 | 1593 | 12493 | 12493 | liveness 54034, constraints 21698, moves 12254 |
| 17921 | 12288 | helix | `helix_lsp::client::<Client>::initialize` | 15414 | 209 | 10 | 182 | 181 | liveness 8177, moves 3955, walk 3108 |
| 16538 | 47401 | helix | `helix_term::application::<Application>::handle_language_server_message` | 10535 | 2700 | 260 | 2480 | 2477 | liveness 5394, constraints 5183, moves 2921 |
| 16209 | 29954 | helix | `helix_view::handlers::dap::<Editor>::handle_debugger_message` | 9952 | 2142 | 250 | 2041 | 2040 | liveness 6088, constraints 3560, moves 3020 |
| 8708 | 49169 | helix | `helix_view::document::<Document>::save_impl` | 4517 | 1218 | 129 | 1269 | 1268 | constraints 3160, liveness 2224, walk 1345 |
| 7684 | 12507 | helix | `helix_term::health::languages` | 2672 | 1852 | 90 | 2108 | 2094 | constraints 4291, liveness 1390, moves 769 |
| 7536 | 14533 | helix | `helix_lsp::transport::<Transport>::send` | 4386 | 1146 | 101 | 1386 | 1386 | liveness 2405, constraints 2371, moves 1147 |
| 7149 | 21639 | helix | `helix_term::commands::syntax::syntax_workspace_symbol_picker` | 2936 | 1166 | 103 | 1082 | 1068 | constraints 3432, liveness 1612, walk 797 |
| 6871 | 11973 | helix | `xtask::tasks::highlightcheck` | 2682 | 1089 | 95 | 1038 | 1034 | constraints 3402, liveness 1533, moves 804 |
| 6796 | 27372 | helix | `helix_term::application::<Application>::event_loop_until_idle` | 2969 | 684 | 70 | 1012 | 1010 | constraints 2593, liveness 1688, walk 1268 |
| 6684 | 16056 | helix | `helix_view::document::<Document>::format` | 3243 | 945 | 81 | 924 | 924 | constraints 3002, liveness 1425, walk 855 |
| 6166 | 18517 | helix | `helix_term::commands::global_search` | 2864 | 1070 | 93 | 1109 | 1097 | constraints 3211, liveness 1186, moves 644 |
| 5696 | 16500 | helix | `xtask::tasks::indentcheck` | 2290 | 1190 | 101 | 1117 | 1117 | constraints 3167, liveness 1144, moves 530 |
| 5283 | 6658 | helix | `helix_term::commands::typed::TYPABLE_COMMAND_LIST` | 4643 | 2589 | 0 | 2437 | 2437 | liveness 2180, constraints 1552, moves 569 |
| 5198 | 7286 | helix | `helix_term::handlers::completion::request::request_completions` | 1616 | 483 | 60 | 503 | 501 | constraints 2946, liveness 904, walk 564 |
| 5052 | 11043 | helix | `helix_term::commands::goto_file_impl` | 2189 | 782 | 81 | 750 | 750 | constraints 2467, liveness 1085, moves 607 |
| 4827 | 27285 | helix | `helix_view::editor::<Editor>::wait_event` | 2600 | 625 | 54 | 849 | 848 | constraints 2061, liveness 1091, walk 660 |
| 4635 | 11197 | helix | `helix_term::application::<Application>::new` | 1653 | 428 | 65 | 355 | 351 | constraints 1871, liveness 1377, walk 607 |
| 4621 | 12034 | helix | `helix_term::commands::lsp::workspace_symbol_picker` | 1639 | 624 | 35 | 576 | 560 | constraints 2558, liveness 730, walk 501 |
| 4496 | 5559 | helix | `helix_tui::backend::termina::<TerminaBackend>::new` | 2212 | 766 | 62 | 658 | 656 | constraints 1287, loans 1078, liveness 853 |
