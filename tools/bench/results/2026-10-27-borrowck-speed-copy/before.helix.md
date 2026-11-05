| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| helix | polonius | 7614 | 7837 | 3064 | 1852 | 1 | 60.5% | 3755 | 61 | 2783 | 110190 |
| helix | nll | 7614 | 7837 | 3064 | 1684 | 0 | 55.0% | 3755 | 52 | 2605 | 109087 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 795 | 30 | 8 | 36 | 346 | 26 | 1 | 340 | 54 | 3 | 131 |
| nll | 735 | 28 | 7 | 34 | 332 | 24 | 0 | 319 | 5 | 3 | 125 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 110190 | 98109 | helix | `helix_term::keymap::default::default` | 36964 | 15331 | 1593 | 12493 | 12493 | liveness 54736, constraints 22226, moves 13991 |
| 19388 | 12082 | helix | `helix_lsp::client::<Client>::initialize` | 15414 | 209 | 10 | 182 | 181 | liveness 8537, walk 4125, moves 3737 |
| 18303 | 29063 | helix | `helix_view::handlers::dap::<Editor>::handle_debugger_message` | 9952 | 2142 | 250 | 2041 | 2040 | liveness 6160, constraints 4655, moves 3958 |
| 17947 | 45858 | helix | `helix_term::application::<Application>::handle_language_server_message` | 10535 | 2700 | 260 | 2480 | 2477 | liveness 5456, constraints 5423, moves 3587 |
| 11104 | 47534 | helix | `helix_view::document::<Document>::save_impl` | 4517 | 1218 | 129 | 1269 | 1268 | constraints 4201, moves 2569, liveness 2391 |
| 9312 | 20950 | helix | `helix_term::commands::syntax::syntax_workspace_symbol_picker` | 2936 | 1166 | 103 | 1082 | 1068 | constraints 4346, liveness 1917, moves 1581 |
| 9205 | 12269 | helix | `helix_term::health::languages` | 2672 | 1852 | 90 | 2108 | 2094 | constraints 4386, liveness 1555, moves 1333 |
| 9193 | 14201 | helix | `helix_lsp::transport::<Transport>::send` | 4386 | 1146 | 101 | 1386 | 1386 | constraints 2697, liveness 2565, moves 1649 |
| 9131 | 26926 | helix | `helix_term::application::<Application>::event_loop_until_idle` | 2969 | 684 | 70 | 1012 | 1010 | constraints 3456, liveness 2077, moves 1792 |
| 8679 | 11673 | helix | `xtask::tasks::highlightcheck` | 2682 | 1089 | 95 | 1038 | 1034 | constraints 4174, liveness 1773, moves 1367 |
| 8361 | 15739 | helix | `helix_view::document::<Document>::format` | 3243 | 945 | 81 | 924 | 924 | constraints 3728, moves 1715, liveness 1565 |
| 7918 | 30455 | helix | `helix_core::snippets::parser::format` | 322 | 817 | 0 | 1496 | 1455 | constraints 4880, moves 1132, walk 1111 |
| 7657 | 18002 | helix | `helix_term::commands::global_search` | 2864 | 1070 | 93 | 1109 | 1097 | constraints 3666, liveness 1385, moves 1333 |
| 7246 | 16130 | helix | `xtask::tasks::indentcheck` | 2290 | 1190 | 101 | 1117 | 1117 | constraints 3700, liveness 1321, moves 1080 |
| 6985 | 26910 | helix | `helix_view::editor::<Editor>::wait_event` | 2600 | 625 | 54 | 849 | 848 | constraints 3261, liveness 1342, moves 1171 |
| 6691 | 11418 | helix | `helix_term::ui::picker::<Picker<T, D>>::render_picker` | 1847 | 959 | 83 | 1097 | 1086 | constraints 3447, moves 1127, liveness 869 |
| 6218 | 10754 | helix | `helix_term::commands::goto_file_impl` | 2189 | 782 | 81 | 750 | 750 | constraints 2978, moves 1168, liveness 1113 |
| 5977 | 6994 | helix | `helix_term::handlers::completion::request::request_completions` | 1616 | 483 | 60 | 503 | 501 | constraints 3051, moves 1068, liveness 988 |
| 5922 | 10914 | helix | `helix_term::application::<Application>::new` | 1653 | 428 | 65 | 355 | 351 | constraints 2293, liveness 1856, moves 810 |
| 5883 | 11661 | helix | `helix_term::commands::lsp::workspace_symbol_picker` | 1639 | 624 | 35 | 576 | 560 | constraints 3065, moves 1145, liveness 941 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 109087 | 98109 | helix | `helix_term::keymap::default::default` | 36964 | 15331 | 1593 | 12493 | 12493 | liveness 56233, constraints 22012, moves 13447 |
| 19468 | 12082 | helix | `helix_lsp::client::<Client>::initialize` | 15414 | 209 | 10 | 182 | 181 | liveness 8306, walk 4144, moves 4105 |
| 17652 | 29063 | helix | `helix_view::handlers::dap::<Editor>::handle_debugger_message` | 9952 | 2142 | 250 | 2041 | 2040 | liveness 6045, constraints 4593, moves 3925 |
| 17411 | 45858 | helix | `helix_term::application::<Application>::handle_language_server_message` | 10535 | 2700 | 260 | 2480 | 2477 | liveness 5380, constraints 5328, moves 3567 |
| 10649 | 47534 | helix | `helix_view::document::<Document>::save_impl` | 4517 | 1218 | 129 | 1269 | 1268 | constraints 4144, moves 2537, liveness 2304 |
| 8866 | 20950 | helix | `helix_term::commands::syntax::syntax_workspace_symbol_picker` | 2936 | 1166 | 103 | 1082 | 1068 | constraints 4167, liveness 1940, moves 1569 |
| 8708 | 26926 | helix | `helix_term::application::<Application>::event_loop_until_idle` | 2969 | 684 | 70 | 1012 | 1010 | constraints 3382, liveness 2045, moves 1768 |
| 8412 | 14201 | helix | `helix_lsp::transport::<Transport>::send` | 4386 | 1146 | 101 | 1386 | 1386 | constraints 2670, liveness 2549, moves 1673 |
| 8357 | 12269 | helix | `helix_term::health::languages` | 2672 | 1852 | 90 | 2108 | 2094 | constraints 4389, liveness 1503, moves 1317 |
| 8236 | 11673 | helix | `xtask::tasks::highlightcheck` | 2682 | 1089 | 95 | 1038 | 1034 | constraints 4100, liveness 1766, moves 1357 |
| 8094 | 15739 | helix | `helix_view::document::<Document>::format` | 3243 | 945 | 81 | 924 | 924 | constraints 3691, moves 1668, liveness 1552 |
| 7745 | 30455 | helix | `helix_core::snippets::parser::format` | 322 | 817 | 0 | 1496 | 1455 | constraints 4848, moves 1121, walk 1107 |
| 7250 | 18002 | helix | `helix_term::commands::global_search` | 2864 | 1070 | 93 | 1109 | 1097 | constraints 3545, liveness 1315, moves 1302 |
| 6952 | 16130 | helix | `xtask::tasks::indentcheck` | 2290 | 1190 | 101 | 1117 | 1117 | constraints 3674, liveness 1301, moves 1106 |
| 6712 | 26910 | helix | `helix_view::editor::<Editor>::wait_event` | 2600 | 625 | 54 | 849 | 848 | constraints 3184, liveness 1370, moves 1142 |
| 6417 | 11418 | helix | `helix_term::ui::picker::<Picker<T, D>>::render_picker` | 1847 | 959 | 83 | 1097 | 1086 | constraints 3420, moves 1158, liveness 858 |
| 5995 | 10754 | helix | `helix_term::commands::goto_file_impl` | 2189 | 782 | 81 | 750 | 750 | constraints 2921, moves 1195, liveness 1111 |
| 5792 | 6994 | helix | `helix_term::handlers::completion::request::request_completions` | 1616 | 483 | 60 | 503 | 501 | constraints 3090, moves 1063, liveness 980 |
| 5779 | 11661 | helix | `helix_term::commands::lsp::workspace_symbol_picker` | 1639 | 624 | 35 | 576 | 560 | constraints 3091, moves 1132, liveness 939 |
| 5695 | 10914 | helix | `helix_term::application::<Application>::new` | 1653 | 428 | 65 | 355 | 351 | constraints 2241, liveness 1865, moves 783 |
