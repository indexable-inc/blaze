| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| nushell | polonius | 35727 | 34997 | 14639 | 9686 | 3 | 66.2% | 27116 | 83 | 2486 | 202434 |
| nushell | nll | 35727 | 34997 | 14639 | 8842 | 1 | 60.4% | 27116 | 71 | 2353 | 202500 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 3168 | 108 | 27 | 128 | 2192 | 97 | 3 | 823 | 161 | 18 | 2565 |
| nll | 2861 | 104 | 24 | 123 | 2066 | 89 | 1 | 757 | 12 | 13 | 2455 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 202434 | 66125 | nushell | `nu_cmd_extra::extra::formats::to::html::theme_list::<HtmlTheme>::list` | 48805 | 30331 | 0 | 25997 | 25997 | liveness 129618, moves 27830, constraints 24573 |
| 127205 | 81074 | nushell | `nu_mcp::run_http_server` | 7041 | 1947 | 136 | 2017 | 2009 | walk 114241, constraints 5632, liveness 3931 |
| 107036 | 16519 | nushell | `nu_mcp::server::<NushellMcpServer>::tool_router` | 57 | 2 | 0 | 0 | 0 | constraints 106818, liveness 81, walk 70 |
| 54029 | 263644 | nushell | `nu_protocol::errors::parse_error::<ParseError>::labels` | 13457 | 5575 | 222 | 5265 | 5265 | constraints 39796, liveness 5517, moves 3010 |
| 53942 | 298680 | nushell | `nu_protocol::errors::shell_error::<ShellError>::labels` | 13112 | 5285 | 218 | 4981 | 4981 | constraints 39766, liveness 5297, moves 3165 |
| 42648 | 28643 | nushell | `nu_command::strings::ansi::ansi_::CODE_LIST` | 14354 | 4947 | 425 | 4014 | 4014 | liveness 28580, constraints 4932, moves 4463 |
| 29652 | 67023 | nushell | `nu::main` | 9351 | 4848 | 381 | 4318 | 4316 | constraints 15120, liveness 6013, walk 3456 |
| 23317 | 116477 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::typed_column_to_series` | 4603 | 1729 | 85 | 1469 | 1467 | constraints 14527, walk 3744, liveness 2483 |
| 20652 | 237449 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::series_to_values` | 5483 | 1612 | 63 | 1944 | 1944 | constraints 9980, walk 4037, liveness 3722 |
| 19496 | 23711 | nushell | `nu_engine::eval_ir::eval_instruction` | 7958 | 1486 | 192 | 1640 | 1640 | walk 6636, constraints 4866, liveness 4296 |
| 19129 | 8496 | nushell | `nu_command::strings::format::date::expand_locale_specifiers` | 13714 | 5129 | 11 | 5130 | 5130 | liveness 6319, infer 3703, moves 3227 |
| 16304 | 26453 | nushell | `nu_plugin_polars::dataframe::values::nu_expression::expr_to_value` | 6329 | 1318 | 39 | 1169 | 1169 | walk 5532, constraints 4946, liveness 3015 |
| 15906 | 21291 | nushell | `nu_protocol::errors::shell_error::<ShellError>::fmt` | 10379 | 9986 | 996 | 15310 | 15310 | constraints 4801, liveness 4337, moves 2012 |
| 15716 | 28054 | nushell | `nu_command::default_context::add_shell_command_context` | 8913 | 2850 | 404 | 3636 | 3636 | constraints 6036, liveness 4166, moves 2209 |
| 15422 | 138263 | nushell | `nu_command::platform::input::list::<SelectWidget<'a>>::render` | 5769 | 2115 | 220 | 2356 | 2356 | constraints 8737, liveness 2340, moves 1515 |
| 14109 | 38722 | nushell | `nu_cli::repl::loop_iteration` | 7760 | 4463 | 350 | 4008 | 4008 | constraints 5453, liveness 3622, moves 1643 |
| 13490 | 54413 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::input_type_list_to_series` | 4461 | 1245 | 76 | 1362 | 1362 | constraints 5745, walk 3189, liveness 2242 |
| 13197 | 24376 | nushell | `nu_cli::reedline_config::edit_from_record` | 4934 | 836 | 2 | 774 | 774 | walk 5085, constraints 4360, liveness 1799 |
| 12668 | 4617 | nushell | `nu_color_config::nu_style::lookup_style` | 11782 | 1627 | 0 | 0 | 0 | liveness 7812, moves 2596, constraints 962 |
| 12624 | 18455 | nushell | `nu_protocol::errors::parse_error::<ParseError>::fmt` | 8443 | 8102 | 909 | 13310 | 13310 | constraints 3857, liveness 3244, moves 1621 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 202500 | 66125 | nushell | `nu_cmd_extra::extra::formats::to::html::theme_list::<HtmlTheme>::list` | 48805 | 30331 | 0 | 25997 | 25997 | liveness 130167, moves 27817, constraints 24555 |
| 126527 | 81074 | nushell | `nu_mcp::run_http_server` | 7041 | 1947 | 136 | 2017 | 2009 | walk 113557, constraints 5790, liveness 4037 |
| 107152 | 16519 | nushell | `nu_mcp::server::<NushellMcpServer>::tool_router` | 57 | 2 | 0 | 0 | 0 | constraints 106927, walk 86, liveness 78 |
| 53165 | 263644 | nushell | `nu_protocol::errors::parse_error::<ParseError>::labels` | 13457 | 5575 | 222 | 5265 | 5265 | constraints 39387, liveness 5305, moves 3063 |
| 52943 | 298680 | nushell | `nu_protocol::errors::shell_error::<ShellError>::labels` | 13112 | 5285 | 218 | 4981 | 4981 | constraints 39407, liveness 5275, moves 3079 |
| 41780 | 28643 | nushell | `nu_command::strings::ansi::ansi_::CODE_LIST` | 14354 | 4947 | 425 | 4014 | 4014 | liveness 28187, constraints 4869, moves 4030 |
| 26775 | 67023 | nushell | `nu::main` | 9351 | 4848 | 381 | 4318 | 4316 | constraints 13513, liveness 5828, walk 3267 |
| 22703 | 116477 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::typed_column_to_series` | 4603 | 1729 | 85 | 1469 | 1467 | constraints 14237, walk 3678, liveness 2468 |
| 19572 | 237449 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::series_to_values` | 5483 | 1612 | 63 | 1944 | 1944 | constraints 9557, walk 3996, liveness 3604 |
| 19289 | 23711 | nushell | `nu_engine::eval_ir::eval_instruction` | 7958 | 1486 | 192 | 1640 | 1640 | walk 6597, constraints 4818, liveness 4263 |
| 18371 | 8496 | nushell | `nu_command::strings::format::date::expand_locale_specifiers` | 13714 | 5129 | 11 | 5130 | 5130 | liveness 6183, infer 3411, moves 3318 |
| 15925 | 26453 | nushell | `nu_plugin_polars::dataframe::values::nu_expression::expr_to_value` | 6329 | 1318 | 39 | 1169 | 1169 | walk 5492, constraints 4862, liveness 2995 |
| 14890 | 138263 | nushell | `nu_command::platform::input::list::<SelectWidget<'a>>::render` | 5769 | 2115 | 220 | 2356 | 2356 | constraints 8317, liveness 2476, moves 1527 |
| 14627 | 28054 | nushell | `nu_command::default_context::add_shell_command_context` | 8913 | 2850 | 404 | 3636 | 3636 | constraints 5671, liveness 4082, moves 2213 |
| 14262 | 21291 | nushell | `nu_protocol::errors::shell_error::<ShellError>::fmt` | 10379 | 9986 | 996 | 15310 | 15310 | constraints 4575, liveness 4058, moves 2009 |
| 13175 | 38722 | nushell | `nu_cli::repl::loop_iteration` | 7760 | 4463 | 350 | 4008 | 4008 | constraints 5181, liveness 3473, moves 1578 |
| 13059 | 24376 | nushell | `nu_cli::reedline_config::edit_from_record` | 4934 | 836 | 2 | 774 | 774 | walk 5106, constraints 4317, liveness 1779 |
| 12884 | 54413 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::input_type_list_to_series` | 4461 | 1245 | 76 | 1362 | 1362 | constraints 5567, walk 3183, liveness 2201 |
| 12391 | 13213 | nushell | `nu_command::formats::from::kdl::<FromKdl>::examples` | 4132 | 520 | 2 | 288 | 288 | walk 5892, constraints 2447, liveness 2292 |
| 12227 | 4617 | nushell | `nu_color_config::nu_style::lookup_style` | 11782 | 1627 | 0 | 0 | 0 | liveness 7647, moves 2411, constraints 931 |
