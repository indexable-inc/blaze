| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| nushell | polonius | 35727 | 35218 | 14452 | 9687 | 3 | 67.0% | 27319 | 82 | 2506 | 213362 |
| nushell | nll | 35727 | 35218 | 14452 | 8819 | 2 | 61.0% | 27319 | 70 | 2358 | 211529 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 3146 | 108 | 27 | 125 | 2196 | 93 | 3 | 843 | 160 | 17 | 2577 |
| nll | 2824 | 104 | 24 | 120 | 2062 | 85 | 2 | 773 | 12 | 12 | 2467 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 213362 | 64468 | nushell | `nu_cmd_extra::extra::formats::to::html::theme_list::<HtmlTheme>::list` | 48805 | 30331 | 0 | 25997 | 25997 | liveness 138407, moves 28352, constraints 25305 |
| 127983 | 80768 | nushell | `nu_mcp::run_http_server` | 7041 | 1947 | 136 | 2017 | 2009 | walk 114776, constraints 5601, liveness 4097 |
| 107840 | 17431 | nushell | `nu_mcp::server::<NushellMcpServer>::tool_router` | 57 | 2 | 0 | 0 | 0 | constraints 107613, walk 79, liveness 77 |
| 56524 | 272662 | nushell | `nu_protocol::errors::parse_error::<ParseError>::labels` | 13457 | 5575 | 222 | 5265 | 5265 | constraints 41576, liveness 5703, moves 3489 |
| 56363 | 308382 | nushell | `nu_protocol::errors::shell_error::<ShellError>::labels` | 13112 | 5285 | 218 | 4981 | 4981 | constraints 41915, liveness 5313, moves 3363 |
| 45445 | 28216 | nushell | `nu_command::strings::ansi::ansi_::CODE_LIST` | 14354 | 4947 | 425 | 4014 | 4014 | liveness 29962, constraints 5420, moves 4488 |
| 28988 | 66087 | nushell | `nu::main` | 9351 | 4848 | 381 | 4318 | 4316 | constraints 14692, liveness 5908, walk 3345 |
| 22679 | 109148 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::typed_column_to_series` | 4603 | 1729 | 85 | 1469 | 1467 | constraints 13968, walk 3740, liveness 2377 |
| 20666 | 25078 | nushell | `nu_engine::eval_ir::eval_instruction` | 7960 | 1486 | 192 | 1640 | 1640 | walk 6865, constraints 5425, liveness 4482 |
| 20253 | 219489 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::series_to_values` | 5483 | 1612 | 63 | 1944 | 1944 | constraints 9698, walk 4072, liveness 3572 |
| 19680 | 8720 | nushell | `nu_command::strings::format::date::expand_locale_specifiers` | 13715 | 5129 | 11 | 5130 | 5130 | liveness 6173, infer 3980, moves 3667 |
| 17178 | 30214 | nushell | `nu_command::default_context::add_shell_command_context` | 8913 | 2850 | 404 | 3636 | 3636 | constraints 6455, liveness 4663, moves 2471 |
| 16323 | 22002 | nushell | `nu_protocol::errors::shell_error::<ShellError>::fmt` | 10379 | 9986 | 996 | 15310 | 15310 | constraints 5333, liveness 4174, moves 2115 |
| 16021 | 24628 | nushell | `nu_plugin_polars::dataframe::values::nu_expression::expr_to_value` | 6329 | 1318 | 39 | 1169 | 1169 | walk 5501, constraints 4748, liveness 3067 |
| 15679 | 137825 | nushell | `nu_command::platform::input::list::<SelectWidget<'a>>::render` | 5769 | 2115 | 220 | 2356 | 2356 | constraints 8861, liveness 2392, moves 1621 |
| 15014 | 39572 | nushell | `nu_cli::repl::loop_iteration` | 7760 | 4463 | 350 | 4008 | 4008 | constraints 6224, liveness 3697, moves 1664 |
| 13746 | 25625 | nushell | `nu_cli::reedline_config::edit_from_record` | 4934 | 836 | 2 | 774 | 774 | walk 5239, constraints 4620, liveness 1825 |
| 13590 | 50527 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::input_type_list_to_series` | 4461 | 1245 | 76 | 1362 | 1362 | constraints 5481, walk 3255, liveness 2441 |
| 13547 | 4312 | nushell | `nu_color_config::nu_style::lookup_style` | 11782 | 1627 | 0 | 0 | 0 | liveness 8641, moves 2607, constraints 1007 |
| 13290 | 19256 | nushell | `nu_protocol::errors::parse_error::<ParseError>::fmt` | 8443 | 8102 | 909 | 13310 | 13310 | constraints 4329, liveness 3332, moves 1709 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 211529 | 64468 | nushell | `nu_cmd_extra::extra::formats::to::html::theme_list::<HtmlTheme>::list` | 48805 | 30331 | 0 | 25997 | 25997 | liveness 138176, moves 27850, constraints 25296 |
| 127988 | 80768 | nushell | `nu_mcp::run_http_server` | 7041 | 1947 | 136 | 2017 | 2009 | walk 115078, constraints 5612, liveness 4096 |
| 107719 | 17431 | nushell | `nu_mcp::server::<NushellMcpServer>::tool_router` | 57 | 2 | 0 | 0 | 0 | constraints 107519, liveness 77, walk 64 |
| 56445 | 272662 | nushell | `nu_protocol::errors::parse_error::<ParseError>::labels` | 13457 | 5575 | 222 | 5265 | 5265 | constraints 41528, liveness 5973, moves 3431 |
| 55544 | 308382 | nushell | `nu_protocol::errors::shell_error::<ShellError>::labels` | 13112 | 5285 | 218 | 4981 | 4981 | constraints 41634, liveness 5252, moves 3255 |
| 44901 | 28216 | nushell | `nu_command::strings::ansi::ansi_::CODE_LIST` | 14354 | 4947 | 425 | 4014 | 4014 | liveness 29863, constraints 5371, moves 4637 |
| 26850 | 66087 | nushell | `nu::main` | 9351 | 4848 | 381 | 4318 | 4316 | constraints 13563, liveness 5806, walk 3309 |
| 22202 | 109148 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::typed_column_to_series` | 4603 | 1729 | 85 | 1469 | 1467 | constraints 13645, walk 3700, liveness 2485 |
| 20003 | 25078 | nushell | `nu_engine::eval_ir::eval_instruction` | 7960 | 1486 | 192 | 1640 | 1640 | walk 6738, constraints 5250, liveness 4387 |
| 19546 | 8720 | nushell | `nu_command::strings::format::date::expand_locale_specifiers` | 13715 | 5129 | 11 | 5130 | 5130 | liveness 5953, moves 4117, infer 3470 |
| 19410 | 219489 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::series_to_values` | 5483 | 1612 | 63 | 1944 | 1944 | constraints 9331, walk 3997, liveness 3594 |
| 15908 | 30214 | nushell | `nu_command::default_context::add_shell_command_context` | 8913 | 2850 | 404 | 3636 | 3636 | constraints 6785, liveness 4141, moves 2313 |
| 15750 | 24628 | nushell | `nu_plugin_polars::dataframe::values::nu_expression::expr_to_value` | 6329 | 1318 | 39 | 1169 | 1169 | walk 5484, constraints 4659, liveness 3033 |
| 15046 | 137825 | nushell | `nu_command::platform::input::list::<SelectWidget<'a>>::render` | 5769 | 2115 | 220 | 2356 | 2356 | constraints 8525, liveness 2500, moves 1525 |
| 14991 | 22002 | nushell | `nu_protocol::errors::shell_error::<ShellError>::fmt` | 10379 | 9986 | 996 | 15310 | 15310 | constraints 5152, liveness 4091, moves 2081 |
| 14243 | 39572 | nushell | `nu_cli::repl::loop_iteration` | 7760 | 4463 | 350 | 4008 | 4008 | constraints 6018, liveness 3590, moves 1652 |
| 13519 | 25625 | nushell | `nu_cli::reedline_config::edit_from_record` | 4934 | 836 | 2 | 774 | 774 | walk 5209, constraints 4561, liveness 1827 |
| 13073 | 50527 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::input_type_list_to_series` | 4461 | 1245 | 76 | 1362 | 1362 | constraints 5432, walk 3171, liveness 2437 |
| 12769 | 14812 | nushell | `nu_command::formats::from::kdl::<FromKdl>::examples` | 4132 | 520 | 2 | 288 | 288 | walk 6056, constraints 2654, liveness 2307 |
| 12515 | 4312 | nushell | `nu_color_config::nu_style::lookup_style` | 11782 | 1627 | 0 | 0 | 0 | liveness 7946, moves 2355, constraints 971 |
