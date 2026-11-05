| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| nushell | polonius | 35727 | 35000 | 14714 | 11581 | 3 | 78.7% | 26859 | 99 | 2944 | 222300 |
| nushell | nll | 35727 | 35000 | 14714 | 10727 | 2 | 72.9% | 26859 | 86 | 2806 | 226457 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 4187 | 111 | 28 | 132 | 2579 | 99 | 3 | 1668 | 167 | 18 | 2189 |
| nll | 3878 | 108 | 25 | 127 | 2444 | 92 | 2 | 1572 | 13 | 14 | 2111 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 222300 | 65619 | nushell | `nu_cmd_extra::extra::formats::to::html::theme_list::<HtmlTheme>::list` | 48805 | 30331 | 0 | 25997 | 25997 | liveness 133516, moves 41179, constraints 25635 |
| 190737 | 26092 | nushell | `nu_command::filters::par_each::stream_parallel_values` | 317 | 53 | 6 | 32 | 32 | constraints 188876, liveness 1191, moves 282 |
| 127227 | 82119 | nushell | `nu_mcp::run_http_server` | 7041 | 1947 | 136 | 2017 | 2009 | moves 114529, constraints 5569, liveness 4214 |
| 107168 | 17982 | nushell | `nu_mcp::server::<NushellMcpServer>::tool_router` | 57 | 2 | 0 | 0 | 0 | constraints 106924, liveness 97, walk 64 |
| 57939 | 266296 | nushell | `nu_protocol::errors::parse_error::<ParseError>::labels` | 13457 | 5575 | 222 | 5265 | 5265 | constraints 40527, moves 5814, liveness 5745 |
| 57424 | 304506 | nushell | `nu_protocol::errors::shell_error::<ShellError>::labels` | 13112 | 5285 | 218 | 4981 | 4981 | constraints 40359, moves 5662, liveness 5544 |
| 46817 | 29466 | nushell | `nu_command::strings::ansi::ansi_::CODE_LIST` | 14354 | 4947 | 425 | 4014 | 4014 | liveness 30545, constraints 5837, moves 4970 |
| 33866 | 66206 | nushell | `nu::main` | 9351 | 4848 | 381 | 4318 | 4316 | constraints 16911, liveness 6485, moves 3860 |
| 29655 | 121062 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::typed_column_to_series` | 4603 | 1729 | 85 | 1469 | 1467 | constraints 18884, walk 3604, liveness 2942 |
| 28954 | 239815 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::series_to_values` | 5483 | 1612 | 63 | 1944 | 1944 | constraints 16249, liveness 4253, walk 3671 |
| 21793 | 23380 | nushell | `nu_engine::eval_ir::eval_instruction` | 7958 | 1486 | 192 | 1640 | 1640 | walk 6526, constraints 5792, liveness 4962 |
| 20744 | 8121 | nushell | `nu_command::strings::format::date::expand_locale_specifiers` | 13714 | 5129 | 11 | 5130 | 5130 | liveness 6156, moves 4741, infer 3741 |
| 20411 | 28510 | nushell | `nu_command::default_context::add_shell_command_context` | 8913 | 2850 | 404 | 3636 | 3636 | constraints 6934, moves 5231, liveness 4761 |
| 19054 | 26913 | nushell | `nu_plugin_polars::dataframe::values::nu_expression::expr_to_value` | 6329 | 1318 | 39 | 1169 | 1169 | constraints 6613, walk 5756, liveness 3397 |
| 17971 | 57585 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::input_type_list_to_series` | 4461 | 1245 | 76 | 1362 | 1362 | constraints 9207, walk 3198, liveness 2484 |
| 17619 | 130020 | nushell | `nu_command::platform::input::list::<SelectWidget<'a>>::render` | 5769 | 2115 | 220 | 2356 | 2356 | constraints 10013, moves 2380, liveness 2360 |
| 15753 | 21354 | nushell | `nu_protocol::errors::shell_error::<ShellError>::fmt` | 10379 | 9986 | 996 | 15310 | 15310 | constraints 4816, liveness 4183, moves 2029 |
| 15229 | 27068 | nushell | `nu_cli::reedline_config::edit_from_record` | 4934 | 836 | 2 | 774 | 774 | constraints 5870, walk 5210, liveness 1841 |
| 14805 | 35387 | nushell | `nu_cli::repl::loop_iteration` | 7760 | 4463 | 350 | 4008 | 4008 | constraints 5706, liveness 3615, moves 2175 |
| 14643 | 23630 | nushell | `nu_command::filesystem::ls::ls_for_one_pattern` | 2642 | 806 | 82 | 713 | 710 | constraints 9533, liveness 1588, walk 1462 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 226457 | 65619 | nushell | `nu_cmd_extra::extra::formats::to::html::theme_list::<HtmlTheme>::list` | 48805 | 30331 | 0 | 25997 | 25997 | liveness 132885, moves 48968, constraints 24859 |
| 190447 | 26092 | nushell | `nu_command::filters::par_each::stream_parallel_values` | 317 | 53 | 6 | 32 | 32 | constraints 188616, liveness 1219, moves 271 |
| 128894 | 82119 | nushell | `nu_mcp::run_http_server` | 7041 | 1947 | 136 | 2017 | 2009 | moves 115808, constraints 6026, liveness 4390 |
| 107136 | 17982 | nushell | `nu_mcp::server::<NushellMcpServer>::tool_router` | 57 | 2 | 0 | 0 | 0 | constraints 106911, liveness 94, walk 65 |
| 57222 | 266296 | nushell | `nu_protocol::errors::parse_error::<ParseError>::labels` | 13457 | 5575 | 222 | 5265 | 5265 | constraints 40330, moves 5864, liveness 5665 |
| 56509 | 304506 | nushell | `nu_protocol::errors::shell_error::<ShellError>::labels` | 13112 | 5285 | 218 | 4981 | 4981 | constraints 40261, moves 5638, liveness 5345 |
| 44023 | 29466 | nushell | `nu_command::strings::ansi::ansi_::CODE_LIST` | 14354 | 4947 | 425 | 4014 | 4014 | liveness 28570, constraints 5390, moves 5155 |
| 33527 | 66206 | nushell | `nu::main` | 9351 | 4848 | 381 | 4318 | 4316 | constraints 17262, liveness 6422, moves 4147 |
| 28836 | 239815 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::series_to_values` | 5483 | 1612 | 63 | 1944 | 1944 | constraints 16561, liveness 4206, walk 3593 |
| 28818 | 121062 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::typed_column_to_series` | 4603 | 1729 | 85 | 1469 | 1467 | constraints 18308, walk 3643, liveness 2956 |
| 21311 | 23380 | nushell | `nu_engine::eval_ir::eval_instruction` | 7958 | 1486 | 192 | 1640 | 1640 | walk 6554, constraints 5795, liveness 4809 |
| 20333 | 8121 | nushell | `nu_command::strings::format::date::expand_locale_specifiers` | 13714 | 5129 | 11 | 5130 | 5130 | liveness 6085, moves 4016, infer 3495 |
| 19552 | 28510 | nushell | `nu_command::default_context::add_shell_command_context` | 8913 | 2850 | 404 | 3636 | 3636 | constraints 7114, moves 5281, liveness 4380 |
| 17908 | 26913 | nushell | `nu_plugin_polars::dataframe::values::nu_expression::expr_to_value` | 6329 | 1318 | 39 | 1169 | 1169 | constraints 6182, walk 5703, liveness 3127 |
| 17377 | 130020 | nushell | `nu_command::platform::input::list::<SelectWidget<'a>>::render` | 5769 | 2115 | 220 | 2356 | 2356 | constraints 10094, liveness 2425, moves 2309 |
| 17071 | 57585 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::input_type_list_to_series` | 4461 | 1245 | 76 | 1362 | 1362 | constraints 8784, walk 3183, liveness 2507 |
| 15016 | 21354 | nushell | `nu_protocol::errors::shell_error::<ShellError>::fmt` | 10379 | 9986 | 996 | 15310 | 15310 | constraints 4887, liveness 4459, moves 2003 |
| 14619 | 27068 | nushell | `nu_cli::reedline_config::edit_from_record` | 4934 | 836 | 2 | 774 | 774 | constraints 5367, walk 5199, liveness 1800 |
| 14189 | 23630 | nushell | `nu_command::filesystem::ls::ls_for_one_pattern` | 2642 | 806 | 82 | 713 | 710 | constraints 9314, liveness 1545, walk 1441 |
| 14152 | 35387 | nushell | `nu_cli::repl::loop_iteration` | 7760 | 4463 | 350 | 4008 | 4008 | constraints 5557, liveness 3641, moves 2157 |
