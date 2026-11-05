| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| nushell | polonius | 35727 | 41892 | 17229 | 19134 | 3 | 111.1% | 31654 | 114 | 4234 | 811113 |
| nushell | nll | 35727 | 41892 | 17229 | 17615 | 2 | 102.2% | 31654 | 98 | 4023 | 811335 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 10747 | 114 | 33 | 137 | 2828 | 603 | 3 | 1778 | 181 | 25 | 2281 |
| nll | 10330 | 111 | 29 | 133 | 2676 | 92 | 2 | 1650 | 13 | 19 | 2208 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 811113 | 20353 | nushell | `nu_plugin::plugin::custom_value_op` | 698 | 127 | 6 | 104 | 104 | constraints 808897, walk 860, moves 592 |
| 533679 | 7907 | nushell | `nu_command::filesystem::ucp::<UCp>::run` | 2247 | 535 | 58 | 473 | 473 | loans 502529, constraints 26171, walk 1799 |
| 367645 | 56733 | nushell | `nu_plugin::plugin::serve_plugin_io` | 2229 | 869 | 48 | 966 | 964 | constraints 359133, liveness 2985, moves 2401 |
| 241860 | 72722 | nushell | `nu_cmd_extra::extra::formats::to::html::theme_list::<HtmlTheme>::list` | 48805 | 30331 | 0 | 25997 | 25997 | liveness 137867, moves 51095, constraints 30449 |
| 240585 | 86397 | nushell | `nu_mcp::run_http_server` | 7041 | 1943 | 136 | 2017 | 2009 | moves 116956, constraints 115320, liveness 4937 |
| 203935 | 20947 | nushell | `nu_plugin_test_support::spawn_fake_plugin::spawn_fake_plugin` | 674 | 237 | 16 | 200 | 200 | constraints 201669, liveness 942, moves 686 |
| 195709 | 29804 | nushell | `nu_command::filters::par_each::stream_parallel_values` | 317 | 53 | 6 | 32 | 32 | constraints 193363, liveness 1515, moves 344 |
| 164876 | 11741 | nushell | `nu_plugin::plugin::interface::<EngineInterface>::write_engine_call` | 581 | 68 | 2 | 56 | 56 | constraints 162721, walk 798, liveness 771 |
| 131478 | 13580 | nushell | `nu_plugin::plugin::interface::<EngineInterface>::write_response` | 522 | 43 | 1 | 37 | 37 | constraints 129785, walk 833, liveness 500 |
| 129883 | 13212 | nushell | `nu_plugin::plugin::interface::<EngineInterface>::write_ordering` | 163 | 15 | 0 | 9 | 9 | constraints 129208, walk 282, liveness 212 |
| 129684 | 13115 | nushell | `nu_plugin::plugin::interface::<EngineInterface>::write_ok` | 166 | 13 | 0 | 9 | 9 | constraints 129014, walk 305, liveness 215 |
| 108835 | 17665 | nushell | `nu_command::strings::str_::replace::<StrReplace>::run` | 784 | 139 | 10 | 124 | 124 | constraints 106132, liveness 1041, walk 973 |
| 107083 | 18280 | nushell | `nu_mcp::server::<NushellMcpServer>::tool_router` | 57 | 2 | 0 | 0 | 0 | constraints 106833, liveness 105, walk 66 |
| 106727 | 24785 | nushell | `nu_mcp::server::<NushellMcpServer>::list_tools` | 236 | 29 | 4 | 13 | 13 | constraints 105864, liveness 474, moves 192 |
| 106256 | 23663 | nushell | `nu_mcp::server::<NushellMcpServer>::call_tool` | 205 | 50 | 3 | 59 | 59 | constraints 105420, liveness 411, moves 219 |
| 105982 | 18441 | nushell | `nu_command::viewers::table::handle_row_stream` | 850 | 196 | 36 | 181 | 181 | constraints 103405, liveness 1108, moves 631 |
| 105548 | 14237 | nushell | `nu_mcp::server::<NushellMcpServer>::get_tool` | 19 | 13 | 1 | 7 | 7 | constraints 105364, liveness 83, moves 48 |
| 104816 | 16811 | nushell | `nu_command::strings::str_::replace::<StrReplace>::run_const` | 578 | 153 | 8 | 147 | 147 | constraints 102855, liveness 847, walk 674 |
| 104606 | 15190 | nushell | `nu_plugin_engine::context::<PluginExecutionCommandContext<'_>>::get_plugin_config` | 105 | 45 | 0 | 43 | 43 | constraints 104170, liveness 183, moves 105 |
| 104349 | 19118 | nushell | `nu_plugin_engine::context::<PluginGetDynamicCompletionContext<'_>>::get_plugin_config` | 105 | 45 | 0 | 43 | 43 | constraints 103923, liveness 177, moves 105 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 811335 | 20353 | nushell | `nu_plugin::plugin::custom_value_op` | 698 | 127 | 6 | 104 | 104 | constraints 809097, walk 894, moves 621 |
| 368768 | 56733 | nushell | `nu_plugin::plugin::serve_plugin_io` | 2229 | 869 | 48 | 966 | 964 | constraints 360449, liveness 2978, moves 2450 |
| 240016 | 86397 | nushell | `nu_mcp::run_http_server` | 7041 | 1943 | 136 | 2017 | 2009 | moves 116546, constraints 115404, liveness 5014 |
| 233718 | 72722 | nushell | `nu_cmd_extra::extra::formats::to::html::theme_list::<HtmlTheme>::list` | 48805 | 30331 | 0 | 25997 | 25997 | liveness 137933, moves 44393, constraints 29974 |
| 203448 | 20947 | nushell | `nu_plugin_test_support::spawn_fake_plugin::spawn_fake_plugin` | 674 | 237 | 16 | 200 | 200 | constraints 201315, liveness 946, moves 612 |
| 194830 | 29804 | nushell | `nu_command::filters::par_each::stream_parallel_values` | 317 | 53 | 6 | 32 | 32 | constraints 192596, liveness 1465, walk 323 |
| 164248 | 11741 | nushell | `nu_plugin::plugin::interface::<EngineInterface>::write_engine_call` | 581 | 68 | 2 | 56 | 56 | constraints 162193, walk 785, liveness 710 |
| 131056 | 13580 | nushell | `nu_plugin::plugin::interface::<EngineInterface>::write_response` | 522 | 43 | 1 | 37 | 37 | constraints 129382, walk 846, liveness 501 |
| 129635 | 13212 | nushell | `nu_plugin::plugin::interface::<EngineInterface>::write_ordering` | 163 | 15 | 0 | 9 | 9 | constraints 128961, walk 298, liveness 212 |
| 129491 | 13115 | nushell | `nu_plugin::plugin::interface::<EngineInterface>::write_ok` | 166 | 13 | 0 | 9 | 9 | constraints 128872, walk 277, liveness 208 |
| 109429 | 15190 | nushell | `nu_plugin_engine::context::<PluginExecutionCommandContext<'_>>::get_plugin_config` | 105 | 45 | 0 | 43 | 43 | constraints 108993, liveness 188, walk 108 |
| 107140 | 23663 | nushell | `nu_mcp::server::<NushellMcpServer>::call_tool` | 205 | 50 | 3 | 59 | 59 | constraints 106250, liveness 459, moves 234 |
| 106777 | 24785 | nushell | `nu_mcp::server::<NushellMcpServer>::list_tools` | 236 | 29 | 4 | 13 | 13 | constraints 106011, liveness 399, moves 189 |
| 106356 | 18280 | nushell | `nu_mcp::server::<NushellMcpServer>::tool_router` | 57 | 2 | 0 | 0 | 0 | constraints 106131, liveness 93, walk 64 |
| 106292 | 17665 | nushell | `nu_command::strings::str_::replace::<StrReplace>::run` | 784 | 139 | 10 | 124 | 124 | constraints 103744, liveness 1056, walk 928 |
| 106038 | 16811 | nushell | `nu_command::strings::str_::replace::<StrReplace>::run_const` | 578 | 153 | 8 | 147 | 147 | constraints 104109, liveness 803, walk 647 |
| 105672 | 14237 | nushell | `nu_mcp::server::<NushellMcpServer>::get_tool` | 19 | 13 | 1 | 7 | 7 | constraints 105472, liveness 90, moves 58 |
| 105273 | 18441 | nushell | `nu_command::viewers::table::handle_row_stream` | 850 | 196 | 36 | 181 | 181 | constraints 102880, liveness 1093, moves 586 |
| 104034 | 15904 | nushell | `nu_cmd_extra::extra::filters::update_cells::<UpdateCells>::run` | 654 | 105 | 13 | 87 | 87 | constraints 101805, walk 857, liveness 838 |
| 102830 | 19118 | nushell | `nu_plugin_engine::context::<PluginGetDynamicCompletionContext<'_>>::get_plugin_config` | 105 | 45 | 0 | 43 | 43 | constraints 102442, liveness 160, moves 97 |
