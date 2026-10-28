| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| helix | polonius | 7614 | 10779 | 3567 | 2336 | 1 | 65.5% | 4661 | 72 | 3639 | 124844 |
| helix | nll | 7614 | 10779 | 3567 | 2126 | 1 | 59.6% | 4661 | 62 | 3369 | 122550 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 1200 | 31 | 8 | 37 | 369 | 28 | 1 | 373 | 58 | 5 | 142 |
| nll | 1111 | 29 | 7 | 35 | 351 | 26 | 1 | 348 | 5 | 4 | 136 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 124844 | 119247 | helix | `helix_term::keymap::default::default` | 36964 | 15331 | 1593 | 12493 | 12493 | liveness 57609, constraints 29466, moves 15716 |
| 24293 | 62791 | helix | `helix_term::application::<Application>::handle_language_server_message` | 10535 | 2700 | 260 | 2479 | 2477 | constraints 9159, liveness 6345, moves 4589 |
| 23852 | 38382 | helix | `helix_view::handlers::dap::<Editor>::handle_debugger_message` | 9952 | 2142 | 250 | 2040 | 2040 | constraints 8130, liveness 6802, moves 4838 |
| 20555 | 14431 | helix | `helix_lsp::client::<Client>::initialize` | 15414 | 209 | 10 | 181 | 181 | liveness 8423, moves 4360, walk 4268 |
| 15590 | 59999 | helix | `helix_view::document::<Document>::save_impl` | 4517 | 1218 | 129 | 1269 | 1268 | constraints 7587, moves 3119, liveness 2686 |
| 15188 | 28541 | helix | `helix_term::commands::syntax::syntax_workspace_symbol_picker` | 2936 | 1159 | 103 | 1082 | 1068 | constraints 9400, liveness 2269, moves 1947 |
| 11727 | 20371 | helix | `helix_view::document::<Document>::format` | 3243 | 945 | 81 | 924 | 924 | constraints 6251, moves 2113, liveness 1782 |
| 11704 | 15373 | helix | `helix_term::health::languages` | 2672 | 1848 | 90 | 2108 | 2094 | constraints 6268, liveness 1807, moves 1572 |
| 11605 | 17824 | helix | `helix_lsp::transport::<Transport>::send` | 4386 | 1146 | 101 | 1386 | 1386 | constraints 4075, liveness 2990, moves 1943 |
| 10866 | 31601 | helix | `helix_term::application::<Application>::event_loop_until_idle` | 2969 | 684 | 70 | 1010 | 1010 | constraints 4567, liveness 2291, moves 1955 |
| 10443 | 24779 | helix | `helix_term::commands::global_search` | 2864 | 1064 | 93 | 1109 | 1097 | constraints 5811, moves 1676, liveness 1560 |
| 10277 | 12800 | helix | `xtask::tasks::highlightcheck` | 2682 | 1088 | 95 | 1038 | 1034 | constraints 5568, liveness 1843, moves 1408 |
| 8954 | 34410 | helix | `helix_core::snippets::parser::format` | 322 | 817 | 0 | 1496 | 1455 | constraints 5647, moves 1223, walk 1202 |
| 8690 | 14872 | helix | `helix_term::commands::goto_file_impl` | 2189 | 782 | 81 | 750 | 750 | constraints 4958, moves 1390, liveness 1232 |
| 8568 | 13703 | helix | `helix_term::ui::picker::<Picker<T, D>>::render_picker` | 1847 | 959 | 83 | 1097 | 1086 | constraints 5133, moves 1248, liveness 902 |
| 8503 | 17351 | helix | `xtask::tasks::indentcheck` | 2290 | 1190 | 101 | 1117 | 1117 | constraints 4757, liveness 1334, moves 1163 |
| 8362 | 15100 | helix | `helix_term::commands::lsp::workspace_symbol_picker` | 1639 | 617 | 35 | 576 | 560 | constraints 5148, moves 1342, liveness 1061 |
| 8316 | 32464 | helix | `helix_view::editor::<Editor>::wait_event` | 2600 | 625 | 54 | 848 | 848 | constraints 4188, liveness 1508, moves 1300 |
| 8170 | 14299 | helix | `helix_term::application::<Application>::new` | 1653 | 426 | 65 | 355 | 351 | constraints 3803, liveness 2406, moves 922 |
| 7469 | 16180 | helix | `helix_term::commands::typed::toggle_option` | 1477 | 941 | 56 | 987 | 985 | constraints 5007, moves 889, liveness 665 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 122550 | 119247 | helix | `helix_term::keymap::default::default` | 36964 | 15331 | 1593 | 12493 | 12493 | liveness 58611, constraints 29126, moves 15861 |
| 24055 | 62791 | helix | `helix_term::application::<Application>::handle_language_server_message` | 10535 | 2700 | 260 | 2479 | 2477 | constraints 9243, liveness 6276, moves 4571 |
| 24000 | 38382 | helix | `helix_view::handlers::dap::<Editor>::handle_debugger_message` | 9952 | 2142 | 250 | 2040 | 2040 | constraints 8345, liveness 6676, moves 5254 |
| 20309 | 14431 | helix | `helix_lsp::client::<Client>::initialize` | 15414 | 209 | 10 | 181 | 181 | liveness 8393, walk 4260, moves 4193 |
| 15052 | 59999 | helix | `helix_view::document::<Document>::save_impl` | 4517 | 1218 | 129 | 1269 | 1268 | constraints 7482, moves 3092, liveness 2649 |
| 14557 | 28541 | helix | `helix_term::commands::syntax::syntax_workspace_symbol_picker` | 2936 | 1159 | 103 | 1082 | 1068 | constraints 9143, liveness 2230, moves 1817 |
| 11647 | 20371 | helix | `helix_view::document::<Document>::format` | 3243 | 945 | 81 | 924 | 924 | constraints 6329, moves 2194, liveness 1767 |
| 10534 | 17824 | helix | `helix_lsp::transport::<Transport>::send` | 4386 | 1146 | 101 | 1386 | 1386 | constraints 3925, liveness 2914, moves 2001 |
| 10278 | 31601 | helix | `helix_term::application::<Application>::event_loop_until_idle` | 2969 | 684 | 70 | 1010 | 1010 | constraints 4383, liveness 2235, moves 1916 |
| 10255 | 15373 | helix | `helix_term::health::languages` | 2672 | 1848 | 90 | 2108 | 2094 | constraints 5824, liveness 1674, moves 1498 |
| 10141 | 24779 | helix | `helix_term::commands::global_search` | 2864 | 1064 | 93 | 1109 | 1097 | constraints 5796, moves 1611, liveness 1523 |
| 9707 | 12800 | helix | `xtask::tasks::highlightcheck` | 2682 | 1088 | 95 | 1038 | 1034 | constraints 5424, liveness 1823, moves 1427 |
| 8571 | 34410 | helix | `helix_core::snippets::parser::format` | 322 | 817 | 0 | 1496 | 1455 | constraints 5491, moves 1182, walk 1180 |
| 8278 | 14872 | helix | `helix_term::commands::goto_file_impl` | 2189 | 782 | 81 | 750 | 750 | constraints 4832, moves 1362, liveness 1240 |
| 8119 | 13703 | helix | `helix_term::ui::picker::<Picker<T, D>>::render_picker` | 1847 | 959 | 83 | 1097 | 1086 | constraints 4890, moves 1286, liveness 929 |
| 8089 | 17351 | helix | `xtask::tasks::indentcheck` | 2290 | 1190 | 101 | 1117 | 1117 | constraints 4698, liveness 1305, moves 1139 |
| 8039 | 15100 | helix | `helix_term::commands::lsp::workspace_symbol_picker` | 1639 | 617 | 35 | 576 | 560 | constraints 5063, moves 1265, liveness 1009 |
| 7887 | 32464 | helix | `helix_view::editor::<Editor>::wait_event` | 2600 | 625 | 54 | 848 | 848 | constraints 4104, liveness 1434, moves 1268 |
| 7729 | 14299 | helix | `helix_term::application::<Application>::new` | 1653 | 426 | 65 | 355 | 351 | constraints 3565, liveness 2300, moves 898 |
| 7505 | 8958 | helix | `helix_term::handlers::completion::request::request_completions` | 1616 | 483 | 60 | 503 | 501 | constraints 4545, moves 1136, liveness 1105 |
