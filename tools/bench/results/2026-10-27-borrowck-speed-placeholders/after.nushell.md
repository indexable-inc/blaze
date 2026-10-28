| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| nushell | polonius | 35727 | 33798 | 14104 | 9378 | 3 | 66.5% | 26375 | 79 | 2457 | 214657 |
| nushell | nll | 35727 | 33798 | 14104 | 8603 | 1 | 61.0% | 26375 | 69 | 2325 | 213556 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 2956 | 107 | 25 | 125 | 2171 | 93 | 3 | 833 | 158 | 14 | 2509 |
| nll | 2672 | 104 | 23 | 120 | 2050 | 86 | 1 | 768 | 12 | 11 | 2424 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 214657 | 65103 | nushell | `nu_cmd_extra::extra::formats::to::html::theme_list::<HtmlTheme>::list` | 48805 | 30331 | 0 | 25997 | 25997 | liveness 140244, moves 27943, constraints 24987 |
| 128276 | 79791 | nushell | `nu_mcp::run_http_server` | 7041 | 1947 | 136 | 2017 | 2009 | walk 115083, constraints 5601, liveness 4092 |
| 53130 | 248551 | nushell | `nu_protocol::errors::parse_error::<ParseError>::labels` | 13457 | 5575 | 222 | 5265 | 5265 | constraints 38814, liveness 5515, moves 3147 |
| 52552 | 280832 | nushell | `nu_protocol::errors::shell_error::<ShellError>::labels` | 13112 | 5285 | 218 | 4981 | 4981 | constraints 38754, liveness 5035, moves 3082 |
| 43846 | 26640 | nushell | `nu_command::strings::ansi::ansi_::CODE_LIST` | 14354 | 4947 | 425 | 4014 | 4014 | liveness 30030, constraints 4629, moves 4341 |
| 29056 | 64635 | nushell | `nu::main` | 9351 | 4848 | 381 | 4318 | 4316 | constraints 14609, liveness 5974, walk 3431 |
| 22591 | 104826 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::typed_column_to_series` | 4603 | 1729 | 85 | 1469 | 1467 | constraints 13875, walk 3661, liveness 2433 |
| 20288 | 25172 | nushell | `nu_engine::eval_ir::eval_instruction` | 7960 | 1486 | 192 | 1640 | 1640 | walk 6692, constraints 5093, liveness 4731 |
| 19709 | 214554 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::series_to_values` | 5483 | 1612 | 63 | 1944 | 1944 | constraints 9305, walk 3941, liveness 3631 |
| 18318 | 8286 | nushell | `nu_command::strings::format::date::expand_locale_specifiers` | 13715 | 5129 | 11 | 5130 | 5130 | liveness 5967, moves 3457, infer 3396 |
| 16012 | 23513 | nushell | `nu_plugin_polars::dataframe::values::nu_expression::expr_to_value` | 6329 | 1318 | 39 | 1169 | 1169 | walk 5455, constraints 4692, liveness 3159 |
| 15621 | 25776 | nushell | `nu_command::default_context::add_shell_command_context` | 8913 | 2850 | 404 | 3636 | 3636 | constraints 5551, liveness 4470, moves 2275 |
| 15477 | 20381 | nushell | `nu_protocol::errors::shell_error::<ShellError>::fmt` | 10379 | 9986 | 996 | 15310 | 15310 | constraints 4624, liveness 4104, moves 2110 |
| 15299 | 129601 | nushell | `nu_command::platform::input::list::<SelectWidget<'a>>::render` | 5769 | 2115 | 220 | 2356 | 2356 | constraints 8497, liveness 2472, moves 1555 |
| 13511 | 36333 | nushell | `nu_cli::repl::loop_iteration` | 7760 | 4463 | 350 | 4008 | 4008 | constraints 5126, liveness 3464, moves 1586 |
| 13222 | 49149 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::input_type_list_to_series` | 4461 | 1245 | 76 | 1362 | 1362 | constraints 5440, walk 3175, liveness 2278 |
| 13198 | 24521 | nushell | `nu_cli::reedline_config::edit_from_record` | 4934 | 836 | 2 | 774 | 774 | walk 5108, constraints 4358, liveness 1780 |
| 13057 | 4361 | nushell | `nu_color_config::nu_style::lookup_style` | 11782 | 1627 | 0 | 0 | 0 | liveness 8224, moves 2500, constraints 1051 |
| 12886 | 13556 | nushell | `nu_command::formats::from::kdl::<FromKdl>::examples` | 4132 | 520 | 2 | 288 | 288 | walk 6035, constraints 2725, liveness 2365 |
| 12837 | 18594 | nushell | `nu::ide::hover` | 7328 | 3012 | 299 | 2471 | 2471 | constraints 4159, liveness 3859, moves 1625 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 213556 | 65103 | nushell | `nu_cmd_extra::extra::formats::to::html::theme_list::<HtmlTheme>::list` | 48805 | 30331 | 0 | 25997 | 25997 | liveness 141192, moves 27480, constraints 24559 |
| 127614 | 79791 | nushell | `nu_mcp::run_http_server` | 7041 | 1947 | 136 | 2017 | 2009 | walk 114828, constraints 5562, liveness 4092 |
| 52784 | 248551 | nushell | `nu_protocol::errors::parse_error::<ParseError>::labels` | 13457 | 5575 | 222 | 5265 | 5265 | constraints 38951, liveness 5544, moves 3152 |
| 52277 | 280832 | nushell | `nu_protocol::errors::shell_error::<ShellError>::labels` | 13112 | 5285 | 218 | 4981 | 4981 | constraints 38655, liveness 5241, moves 3152 |
| 43211 | 26640 | nushell | `nu_command::strings::ansi::ansi_::CODE_LIST` | 14354 | 4947 | 425 | 4014 | 4014 | liveness 29842, constraints 4518, moves 4253 |
| 27633 | 64635 | nushell | `nu::main` | 9351 | 4848 | 381 | 4318 | 4316 | constraints 13967, liveness 5943, walk 3428 |
| 22168 | 104826 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::typed_column_to_series` | 4603 | 1729 | 85 | 1469 | 1467 | constraints 13738, walk 3689, liveness 2390 |
| 19941 | 25172 | nushell | `nu_engine::eval_ir::eval_instruction` | 7960 | 1486 | 192 | 1640 | 1640 | walk 6640, constraints 4911, liveness 4787 |
| 18975 | 214554 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::series_to_values` | 5483 | 1612 | 63 | 1944 | 1944 | constraints 9106, walk 3903, liveness 3531 |
| 17989 | 8286 | nushell | `nu_command::strings::format::date::expand_locale_specifiers` | 13715 | 5129 | 11 | 5130 | 5130 | liveness 6140, infer 3445, moves 3295 |
| 15639 | 23513 | nushell | `nu_plugin_polars::dataframe::values::nu_expression::expr_to_value` | 6329 | 1318 | 39 | 1169 | 1169 | walk 5424, constraints 4622, liveness 3041 |
| 14776 | 129601 | nushell | `nu_command::platform::input::list::<SelectWidget<'a>>::render` | 5769 | 2115 | 220 | 2356 | 2356 | constraints 8450, liveness 2367, moves 1477 |
| 14376 | 25776 | nushell | `nu_command::default_context::add_shell_command_context` | 8913 | 2850 | 404 | 3636 | 3636 | constraints 5392, liveness 4103, moves 2196 |
| 14307 | 20381 | nushell | `nu_protocol::errors::shell_error::<ShellError>::fmt` | 10379 | 9986 | 996 | 15310 | 15310 | constraints 4490, liveness 4127, moves 2094 |
| 13029 | 24521 | nushell | `nu_cli::reedline_config::edit_from_record` | 4934 | 836 | 2 | 774 | 774 | walk 5098, constraints 4250, liveness 1795 |
| 12924 | 36333 | nushell | `nu_cli::repl::loop_iteration` | 7760 | 4463 | 350 | 4008 | 4008 | constraints 4995, liveness 3444, moves 1643 |
| 12881 | 4361 | nushell | `nu_color_config::nu_style::lookup_style` | 11782 | 1627 | 0 | 0 | 0 | liveness 8267, moves 2360, constraints 998 |
| 12787 | 49149 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::input_type_list_to_series` | 4461 | 1245 | 76 | 1362 | 1362 | constraints 5347, walk 3153, liveness 2282 |
| 12643 | 13556 | nushell | `nu_command::formats::from::kdl::<FromKdl>::examples` | 4132 | 520 | 2 | 288 | 288 | walk 6021, constraints 2635, liveness 2323 |
| 12005 | 18594 | nushell | `nu::ide::hover` | 7328 | 3012 | 299 | 2471 | 2471 | constraints 3882, liveness 3742, moves 1602 |
