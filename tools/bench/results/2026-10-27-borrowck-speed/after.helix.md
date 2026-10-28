| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| helix | polonius | 7614 | 8463 | 3178 | 1899 | 1 | 59.8% | 4045 | 62 | 2837 | 123970 |
| helix | nll | 7614 | 8463 | 3178 | 1709 | 0 | 53.8% | 4045 | 52 | 2633 | 117617 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 819 | 30 | 7 | 36 | 357 | 26 | 1 | 347 | 55 | 4 | 133 |
| nll | 747 | 29 | 7 | 34 | 337 | 24 | 0 | 323 | 5 | 3 | 127 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 123970 | 113096 | helix | `helix_term::keymap::default::default` | 36964 | 15331 | 1593 | 12493 | 12493 | liveness 62087, constraints 24322, moves 16217 |
| 19949 | 12132 | helix | `helix_lsp::client::<Client>::initialize` | 15414 | 209 | 10 | 182 | 181 | liveness 8523, moves 4207, walk 4153 |
| 19420 | 54179 | helix | `helix_term::application::<Application>::handle_language_server_message` | 10535 | 2700 | 260 | 2480 | 2477 | constraints 6515, liveness 5555, moves 3755 |
| 18499 | 30354 | helix | `helix_view::handlers::dap::<Editor>::handle_debugger_message` | 9952 | 2142 | 250 | 2041 | 2040 | liveness 6108, constraints 4990, moves 3863 |
| 11757 | 51536 | helix | `helix_view::document::<Document>::save_impl` | 4517 | 1218 | 129 | 1269 | 1268 | constraints 4670, moves 2644, liveness 2441 |
| 10809 | 14518 | helix | `helix_term::health::languages` | 2672 | 1852 | 90 | 2108 | 2094 | constraints 5611, liveness 1741, moves 1457 |
| 9442 | 29213 | helix | `helix_term::application::<Application>::event_loop_until_idle` | 2969 | 684 | 70 | 1012 | 1010 | constraints 3692, liveness 2090, moves 1802 |
| 9418 | 16051 | helix | `helix_lsp::transport::<Transport>::send` | 4386 | 1146 | 101 | 1386 | 1386 | constraints 2829, liveness 2589, moves 1727 |
| 9185 | 21253 | helix | `helix_term::commands::syntax::syntax_workspace_symbol_picker` | 2936 | 1166 | 103 | 1082 | 1068 | constraints 4287, liveness 1899, moves 1553 |
| 8897 | 18019 | helix | `helix_view::document::<Document>::format` | 3243 | 945 | 81 | 924 | 924 | constraints 4078, moves 1790, liveness 1633 |
| 8632 | 11977 | helix | `xtask::tasks::highlightcheck` | 2682 | 1089 | 95 | 1038 | 1034 | constraints 4172, liveness 1730, moves 1369 |
| 7961 | 30126 | helix | `helix_core::snippets::parser::format` | 322 | 817 | 0 | 1496 | 1455 | constraints 4912, moves 1142, walk 1105 |
| 7835 | 20555 | helix | `helix_term::commands::global_search` | 2864 | 1070 | 93 | 1109 | 1097 | constraints 3835, liveness 1426, moves 1309 |
| 7603 | 19707 | helix | `xtask::tasks::indentcheck` | 2290 | 1190 | 101 | 1117 | 1117 | constraints 4040, liveness 1292, moves 1133 |
| 7225 | 28508 | helix | `helix_view::editor::<Editor>::wait_event` | 2600 | 625 | 54 | 849 | 848 | constraints 3390, liveness 1387, moves 1218 |
| 6908 | 11754 | helix | `helix_term::ui::picker::<Picker<T, D>>::render_picker` | 1847 | 959 | 83 | 1097 | 1086 | constraints 3633, moves 1152, liveness 892 |
| 6641 | 12588 | helix | `helix_term::application::<Application>::new` | 1653 | 428 | 65 | 355 | 351 | constraints 2636, liveness 2163, moves 873 |
| 6233 | 11252 | helix | `helix_term::commands::goto_file_impl` | 2189 | 782 | 81 | 750 | 750 | constraints 2989, moves 1155, liveness 1115 |
| 6125 | 7990 | helix | `helix_term::handlers::completion::request::request_completions` | 1616 | 483 | 60 | 503 | 501 | constraints 3163, moves 1068, liveness 1008 |
| 5915 | 12074 | helix | `helix_term::commands::lsp::workspace_symbol_picker` | 1639 | 624 | 35 | 576 | 560 | constraints 3098, moves 1097, liveness 972 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 117617 | 113096 | helix | `helix_term::keymap::default::default` | 36964 | 15331 | 1593 | 12493 | 12493 | liveness 58247, constraints 24665, moves 15885 |
| 19734 | 12132 | helix | `helix_lsp::client::<Client>::initialize` | 15414 | 209 | 10 | 182 | 181 | liveness 8439, walk 4172, moves 4156 |
| 18529 | 54179 | helix | `helix_term::application::<Application>::handle_language_server_message` | 10535 | 2700 | 260 | 2480 | 2477 | constraints 5988, liveness 5665, moves 3638 |
| 18231 | 30354 | helix | `helix_view::handlers::dap::<Editor>::handle_debugger_message` | 9952 | 2142 | 250 | 2041 | 2040 | liveness 6091, constraints 5078, moves 3942 |
| 11228 | 51536 | helix | `helix_view::document::<Document>::save_impl` | 4517 | 1218 | 129 | 1269 | 1268 | constraints 4573, moves 2610, liveness 2346 |
| 9664 | 14518 | helix | `helix_term::health::languages` | 2672 | 1852 | 90 | 2108 | 2094 | constraints 5383, liveness 1645, moves 1435 |
| 8748 | 29213 | helix | `helix_term::application::<Application>::event_loop_until_idle` | 2969 | 684 | 70 | 1012 | 1010 | constraints 3438, liveness 1994, moves 1745 |
| 8648 | 21253 | helix | `helix_term::commands::syntax::syntax_workspace_symbol_picker` | 2936 | 1166 | 103 | 1082 | 1068 | constraints 4047, liveness 1897, moves 1504 |
| 8612 | 16051 | helix | `helix_lsp::transport::<Transport>::send` | 4386 | 1146 | 101 | 1386 | 1386 | constraints 2780, liveness 2599, moves 1693 |
| 8340 | 18019 | helix | `helix_view::document::<Document>::format` | 3243 | 945 | 81 | 924 | 924 | constraints 3859, moves 1698, liveness 1554 |
| 8119 | 11977 | helix | `xtask::tasks::highlightcheck` | 2682 | 1089 | 95 | 1038 | 1034 | constraints 4086, liveness 1707, moves 1307 |
| 7757 | 30126 | helix | `helix_core::snippets::parser::format` | 322 | 817 | 0 | 1496 | 1455 | constraints 4853, moves 1124, walk 1099 |
| 7328 | 20555 | helix | `helix_term::commands::global_search` | 2864 | 1070 | 93 | 1109 | 1097 | constraints 3640, liveness 1308, moves 1296 |
| 6986 | 19707 | helix | `xtask::tasks::indentcheck` | 2290 | 1190 | 101 | 1117 | 1117 | constraints 3730, liveness 1282, moves 1096 |
| 6821 | 28508 | helix | `helix_view::editor::<Editor>::wait_event` | 2600 | 625 | 54 | 849 | 848 | constraints 3316, liveness 1306, moves 1175 |
| 6517 | 11754 | helix | `helix_term::ui::picker::<Picker<T, D>>::render_picker` | 1847 | 959 | 83 | 1097 | 1086 | constraints 3529, moves 1117, liveness 901 |
| 6200 | 12588 | helix | `helix_term::application::<Application>::new` | 1653 | 428 | 65 | 355 | 351 | constraints 2488, liveness 2065, moves 802 |
| 6011 | 11252 | helix | `helix_term::commands::goto_file_impl` | 2189 | 782 | 81 | 750 | 750 | constraints 2898, liveness 1159, moves 1156 |
| 5819 | 7990 | helix | `helix_term::handlers::completion::request::request_completions` | 1616 | 483 | 60 | 503 | 501 | constraints 3066, moves 1049, liveness 990 |
| 5663 | 11664 | helix | `helix_term::commands::lsp::resolve_and_apply_code_actions_of_kind` | 1647 | 598 | 42 | 585 | 583 | constraints 2977, moves 1156, liveness 888 |
