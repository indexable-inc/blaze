| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| nushell | polonius | 35727 | 37609 | 15539 | 11981 | 4 | 77.1% | 28852 | 98 | 3122 | 239613 |
| nushell | nll | 35727 | 37609 | 15539 | 10953 | 2 | 70.5% | 28852 | 84 | 2930 | 224408 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 4371 | 112 | 30 | 135 | 2670 | 104 | 4 | 1734 | 168 | 20 | 2219 |
| nll | 3983 | 108 | 27 | 129 | 2501 | 95 | 2 | 1599 | 14 | 16 | 2128 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 239613 | 68084 | nushell | `nu_cmd_extra::extra::formats::to::html::theme_list::<HtmlTheme>::list` | 48805 | 30331 | 0 | 25997 | 25997 | liveness 137540, moves 52723, constraints 25816 |
| 186347 | 28084 | nushell | `nu_command::filters::par_each::stream_parallel_values` | 317 | 53 | 6 | 32 | 32 | constraints 184440, liveness 1214, moves 302 |
| 133216 | 86515 | nushell | `nu_mcp::run_http_server` | 7041 | 1947 | 136 | 2017 | 2009 | moves 118056, constraints 6765, liveness 5024 |
| 108802 | 19997 | nushell | `nu_mcp::server::<NushellMcpServer>::tool_router` | 57 | 2 | 0 | 0 | 0 | constraints 108533, liveness 111, walk 69 |
| 63652 | 301180 | nushell | `nu_protocol::errors::parse_error::<ParseError>::labels` | 13457 | 5575 | 222 | 5265 | 5265 | constraints 44201, moves 6919, liveness 5841 |
| 61717 | 337343 | nushell | `nu_protocol::errors::shell_error::<ShellError>::labels` | 13112 | 5285 | 218 | 4981 | 4981 | constraints 43242, moves 6549, liveness 5662 |
| 44811 | 30294 | nushell | `nu_command::strings::ansi::ansi_::CODE_LIST` | 14354 | 4947 | 425 | 4014 | 4014 | liveness 29069, constraints 5587, moves 5122 |
| 39135 | 76444 | nushell | `nu::main` | 9351 | 4848 | 381 | 4318 | 4316 | constraints 19642, liveness 7237, moves 4692 |
| 31267 | 256082 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::series_to_values` | 5483 | 1612 | 63 | 1944 | 1944 | constraints 17494, liveness 4502, walk 3889 |
| 30043 | 124469 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::typed_column_to_series` | 4603 | 1729 | 85 | 1469 | 1467 | constraints 18922, walk 3627, liveness 3071 |
| 24998 | 28285 | nushell | `nu_engine::eval_ir::eval_instruction` | 7958 | 1486 | 192 | 1640 | 1640 | constraints 7380, walk 7109, liveness 5303 |
| 21811 | 30254 | nushell | `nu_command::default_context::add_shell_command_context` | 8913 | 2850 | 404 | 3636 | 3636 | constraints 7467, moves 5993, liveness 4695 |
| 21632 | 9234 | nushell | `nu_command::strings::format::date::expand_locale_specifiers` | 13714 | 5129 | 11 | 5130 | 5130 | liveness 5961, moves 4431, infer 3827 |
| 20412 | 24836 | nushell | `nu_protocol::errors::shell_error::<ShellError>::fmt` | 10379 | 9986 | 996 | 15310 | 15310 | constraints 7318, liveness 4949, moves 2484 |
| 19470 | 141564 | nushell | `nu_command::platform::input::list::<SelectWidget<'a>>::render` | 5769 | 2115 | 220 | 2356 | 2356 | constraints 11243, moves 2639, liveness 2589 |
| 19331 | 58814 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::input_type_list_to_series` | 4461 | 1245 | 76 | 1362 | 1362 | constraints 9960, walk 3243, liveness 2825 |
| 19131 | 28259 | nushell | `nu_plugin_polars::dataframe::values::nu_expression::expr_to_value` | 6329 | 1318 | 39 | 1169 | 1169 | constraints 6585, walk 5796, liveness 3323 |
| 17485 | 41215 | nushell | `nu_cli::repl::loop_iteration` | 7760 | 4463 | 350 | 4008 | 4008 | constraints 7495, liveness 3968, moves 2477 |
| 17049 | 25127 | nushell | `nu_protocol::eval_base::Eval::eval` | 4801 | 1304 | 161 | 1360 | 1360 | constraints 7010, walk 3517, liveness 2972 |
| 15831 | 24976 | nushell | `nu_command::filesystem::ls::ls_for_one_pattern` | 2642 | 806 | 82 | 713 | 710 | constraints 10373, liveness 1717, walk 1513 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 224408 | 68084 | nushell | `nu_cmd_extra::extra::formats::to::html::theme_list::<HtmlTheme>::list` | 48805 | 30331 | 0 | 25997 | 25997 | liveness 135525, moves 42599, constraints 25813 |
| 186366 | 28084 | nushell | `nu_command::filters::par_each::stream_parallel_values` | 317 | 53 | 6 | 32 | 32 | constraints 184480, liveness 1257, moves 281 |
| 132562 | 86515 | nushell | `nu_mcp::run_http_server` | 7041 | 1947 | 136 | 2017 | 2009 | moves 118142, constraints 6700, liveness 4831 |
| 108681 | 19997 | nushell | `nu_mcp::server::<NushellMcpServer>::tool_router` | 57 | 2 | 0 | 0 | 0 | constraints 108435, liveness 103, walk 67 |
| 63464 | 301180 | nushell | `nu_protocol::errors::parse_error::<ParseError>::labels` | 13457 | 5575 | 222 | 5265 | 5265 | constraints 44044, moves 7039, liveness 6300 |
| 61454 | 337343 | nushell | `nu_protocol::errors::shell_error::<ShellError>::labels` | 13112 | 5285 | 218 | 4981 | 4981 | constraints 43383, moves 6542, liveness 5783 |
| 44452 | 30294 | nushell | `nu_command::strings::ansi::ansi_::CODE_LIST` | 14354 | 4947 | 425 | 4014 | 4014 | liveness 29217, constraints 5454, moves 4950 |
| 38154 | 76444 | nushell | `nu::main` | 9351 | 4848 | 381 | 4318 | 4316 | constraints 19116, liveness 7394, moves 4857 |
| 31218 | 256082 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::series_to_values` | 5483 | 1612 | 63 | 1944 | 1944 | constraints 17708, liveness 4660, moves 3806 |
| 29826 | 124469 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::typed_column_to_series` | 4603 | 1729 | 85 | 1469 | 1467 | constraints 18998, walk 3635, liveness 3008 |
| 24953 | 28285 | nushell | `nu_engine::eval_ir::eval_instruction` | 7958 | 1486 | 192 | 1640 | 1640 | constraints 7523, walk 6995, liveness 5462 |
| 22115 | 9234 | nushell | `nu_command::strings::format::date::expand_locale_specifiers` | 13714 | 5129 | 11 | 5130 | 5130 | liveness 6601, moves 4428, infer 3542 |
| 21495 | 30254 | nushell | `nu_command::default_context::add_shell_command_context` | 8913 | 2850 | 404 | 3636 | 3636 | constraints 7556, moves 5482, liveness 5121 |
| 19451 | 141564 | nushell | `nu_command::platform::input::list::<SelectWidget<'a>>::render` | 5769 | 2115 | 220 | 2356 | 2356 | constraints 11425, moves 2739, liveness 2501 |
| 19221 | 24836 | nushell | `nu_protocol::errors::shell_error::<ShellError>::fmt` | 10379 | 9986 | 996 | 15310 | 15310 | constraints 7470, liveness 5123, moves 2405 |
| 18621 | 28259 | nushell | `nu_plugin_polars::dataframe::values::nu_expression::expr_to_value` | 6329 | 1318 | 39 | 1169 | 1169 | constraints 6632, walk 5726, liveness 3169 |
| 18175 | 58814 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::input_type_list_to_series` | 4461 | 1245 | 76 | 1362 | 1362 | constraints 9612, walk 3201, liveness 2573 |
| 16622 | 41215 | nushell | `nu_cli::repl::loop_iteration` | 7760 | 4463 | 350 | 4008 | 4008 | constraints 7255, liveness 3915, moves 2404 |
| 16610 | 25127 | nushell | `nu_protocol::eval_base::Eval::eval` | 4801 | 1304 | 161 | 1360 | 1360 | constraints 6930, walk 3482, liveness 2837 |
| 15521 | 26620 | nushell | `nu_cli::reedline_config::edit_from_record` | 4934 | 836 | 2 | 774 | 774 | constraints 6023, walk 5193, liveness 1920 |
