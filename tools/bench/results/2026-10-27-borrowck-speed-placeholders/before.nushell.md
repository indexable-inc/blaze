| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| nushell | polonius | 35727 | 32617 | 13891 | 9405 | 3 | 67.7% | 25760 | 80 | 2428 | 207679 |
| nushell | nll | 35727 | 32617 | 13891 | 8645 | 1 | 62.2% | 25760 | 70 | 2303 | 205894 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 3013 | 109 | 25 | 124 | 2145 | 93 | 3 | 830 | 158 | 15 | 2509 |
| nll | 2749 | 104 | 23 | 119 | 2023 | 85 | 1 | 764 | 12 | 11 | 2423 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 207679 | 55169 | nushell | `nu_cmd_extra::extra::formats::to::html::theme_list::<HtmlTheme>::list` | 48805 | 30331 | 0 | 25997 | 25997 | liveness 136697, moves 27733, constraints 23337 |
| 125078 | 72756 | nushell | `nu_mcp::run_http_server` | 7041 | 1947 | 136 | 2017 | 2009 | walk 113184, constraints 4791, liveness 3826 |
| 106870 | 16146 | nushell | `nu_mcp::server::<NushellMcpServer>::tool_router` | 57 | 2 | 0 | 0 | 0 | constraints 106711, walk 58, liveness 52 |
| 53105 | 247016 | nushell | `nu_protocol::errors::parse_error::<ParseError>::labels` | 13457 | 5575 | 222 | 5265 | 5265 | constraints 38925, liveness 5419, moves 3088 |
| 52656 | 281395 | nushell | `nu_protocol::errors::shell_error::<ShellError>::labels` | 13112 | 5285 | 218 | 4981 | 4981 | constraints 38737, liveness 4970, moves 3256 |
| 42960 | 25435 | nushell | `nu_command::strings::ansi::ansi_::CODE_LIST` | 14354 | 4947 | 425 | 4014 | 4014 | liveness 29579, moves 4249, constraints 4197 |
| 25861 | 57492 | nushell | `nu::main` | 9351 | 4848 | 381 | 4318 | 4316 | constraints 12127, liveness 5671, walk 3218 |
| 22354 | 105209 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::typed_column_to_series` | 4603 | 1729 | 85 | 1469 | 1467 | constraints 13707, walk 3657, liveness 2434 |
| 19573 | 22851 | nushell | `nu_engine::eval_ir::eval_instruction` | 7960 | 1486 | 192 | 1640 | 1640 | walk 6552, constraints 4627, liveness 4581 |
| 19523 | 213848 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::series_to_values` | 5483 | 1612 | 63 | 1944 | 1944 | constraints 9225, walk 3955, liveness 3476 |
| 18999 | 8016 | nushell | `nu_command::strings::format::date::expand_locale_specifiers` | 13715 | 5129 | 11 | 5130 | 5130 | liveness 6196, infer 3843, moves 3383 |
| 15909 | 23770 | nushell | `nu_plugin_polars::dataframe::values::nu_expression::expr_to_value` | 6329 | 1318 | 39 | 1169 | 1169 | walk 5440, constraints 4575, liveness 3145 |
| 15389 | 25324 | nushell | `nu_command::default_context::add_shell_command_context` | 8913 | 2850 | 404 | 3636 | 3636 | constraints 5541, liveness 4155, moves 2332 |
| 15339 | 20236 | nushell | `nu_protocol::errors::shell_error::<ShellError>::fmt` | 10379 | 9986 | 996 | 15310 | 15310 | constraints 4445, liveness 4111, moves 2079 |
| 15281 | 132723 | nushell | `nu_command::platform::input::list::<SelectWidget<'a>>::render` | 5769 | 2115 | 220 | 2356 | 2356 | constraints 8490, liveness 2475, moves 1542 |
| 13340 | 4337 | nushell | `nu_color_config::nu_style::lookup_style` | 11782 | 1627 | 0 | 0 | 0 | liveness 8414, moves 2713, constraints 927 |
| 13330 | 33922 | nushell | `nu_cli::repl::loop_iteration` | 7760 | 4463 | 350 | 4008 | 4008 | constraints 4993, liveness 3425, moves 1593 |
| 13206 | 48713 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::input_type_list_to_series` | 4461 | 1245 | 76 | 1362 | 1362 | constraints 5373, walk 3139, liveness 2315 |
| 13168 | 22682 | nushell | `nu_cli::reedline_config::edit_from_record` | 4934 | 836 | 2 | 774 | 774 | walk 5080, constraints 4282, liveness 1808 |
| 12653 | 17622 | nushell | `nu_protocol::errors::parse_error::<ParseError>::fmt` | 8443 | 8102 | 909 | 13310 | 13310 | constraints 3702, liveness 3329, moves 1694 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 205894 | 55169 | nushell | `nu_cmd_extra::extra::formats::to::html::theme_list::<HtmlTheme>::list` | 48805 | 30331 | 0 | 25997 | 25997 | liveness 136675, moves 26674, constraints 23151 |
| 123936 | 72756 | nushell | `nu_mcp::run_http_server` | 7041 | 1947 | 136 | 2017 | 2009 | walk 112928, constraints 4422, liveness 3686 |
| 106718 | 16146 | nushell | `nu_mcp::server::<NushellMcpServer>::tool_router` | 57 | 2 | 0 | 0 | 0 | constraints 106572, walk 56, liveness 50 |
| 52669 | 281395 | nushell | `nu_protocol::errors::shell_error::<ShellError>::labels` | 13112 | 5285 | 218 | 4981 | 4981 | constraints 38785, liveness 5471, moves 3297 |
| 52360 | 247016 | nushell | `nu_protocol::errors::parse_error::<ParseError>::labels` | 13457 | 5575 | 222 | 5265 | 5265 | constraints 38920, liveness 5205, moves 3096 |
| 42447 | 25435 | nushell | `nu_command::strings::ansi::ansi_::CODE_LIST` | 14354 | 4947 | 425 | 4014 | 4014 | liveness 29528, moves 4259, constraints 4021 |
| 24814 | 57492 | nushell | `nu::main` | 9351 | 4848 | 381 | 4318 | 4316 | constraints 11801, liveness 5713, walk 3171 |
| 22222 | 105209 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::typed_column_to_series` | 4603 | 1729 | 85 | 1469 | 1467 | constraints 13684, walk 3734, liveness 2457 |
| 18991 | 213848 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::series_to_values` | 5483 | 1612 | 63 | 1944 | 1944 | constraints 9092, walk 3934, liveness 3530 |
| 18970 | 22851 | nushell | `nu_engine::eval_ir::eval_instruction` | 7960 | 1486 | 192 | 1640 | 1640 | walk 6590, constraints 4583, liveness 4321 |
| 18263 | 8016 | nushell | `nu_command::strings::format::date::expand_locale_specifiers` | 13715 | 5129 | 11 | 5130 | 5130 | liveness 6249, infer 3538, moves 3409 |
| 15717 | 23770 | nushell | `nu_plugin_polars::dataframe::values::nu_expression::expr_to_value` | 6329 | 1318 | 39 | 1169 | 1169 | walk 5413, constraints 4534, liveness 3124 |
| 14760 | 132723 | nushell | `nu_command::platform::input::list::<SelectWidget<'a>>::render` | 5769 | 2115 | 220 | 2356 | 2356 | constraints 8323, liveness 2479, moves 1480 |
| 14552 | 25324 | nushell | `nu_command::default_context::add_shell_command_context` | 8913 | 2850 | 404 | 3636 | 3636 | constraints 5416, liveness 4180, moves 2297 |
| 14144 | 20236 | nushell | `nu_protocol::errors::shell_error::<ShellError>::fmt` | 10379 | 9986 | 996 | 15310 | 15310 | constraints 4381, liveness 4081, moves 2040 |
| 12961 | 22682 | nushell | `nu_cli::reedline_config::edit_from_record` | 4934 | 836 | 2 | 774 | 774 | walk 5031, constraints 4223, liveness 1844 |
| 12865 | 33922 | nushell | `nu_cli::repl::loop_iteration` | 7760 | 4463 | 350 | 4008 | 4008 | constraints 4941, liveness 3494, moves 1585 |
| 12739 | 48713 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::input_type_list_to_series` | 4461 | 1245 | 76 | 1362 | 1362 | constraints 5275, walk 3135, liveness 2275 |
| 12233 | 12185 | nushell | `nu_command::formats::from::kdl::<FromKdl>::examples` | 4132 | 520 | 2 | 288 | 288 | walk 5820, constraints 2372, liveness 2280 |
| 12111 | 4337 | nushell | `nu_color_config::nu_style::lookup_style` | 11782 | 1627 | 0 | 0 | 0 | liveness 7644, moves 2328, constraints 915 |
