| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| nushell | polonius | 35727 | 33345 | 14004 | 6455 | 2 | 46.1% | 23474 | 71 | 1825 | 96076 |
| nushell | nll | 35727 | 33345 | 14004 | 5682 | 1 | 40.6% | 23474 | 59 | 1656 | 92103 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 2896 | 106 | 24 | 122 | 1468 | 94 | 2 | 817 | 155 | 13 | 381 |
| nll | 2557 | 102 | 21 | 116 | 1368 | 82 | 1 | 743 | 12 | 9 | 353 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 96076 | 57114 | nushell | `nu_cmd_extra::extra::formats::to::html::theme_list::<HtmlTheme>::list` | 48805 | 30331 | 0 | 25997 | 25997 | liveness 29325, moves 28590, constraints 24640 |
| 52045 | 242991 | nushell | `nu_protocol::errors::parse_error::<ParseError>::labels` | 13457 | 5575 | 222 | 5265 | 5265 | constraints 38452, liveness 5179, moves 3119 |
| 51966 | 277380 | nushell | `nu_protocol::errors::shell_error::<ShellError>::labels` | 13112 | 5285 | 218 | 4981 | 4981 | constraints 38851, liveness 4858, moves 3059 |
| 35617 | 25843 | nushell | `nu_command::strings::ansi::ansi_::CODE_LIST` | 14354 | 4947 | 425 | 4014 | 4014 | liveness 22044, moves 5049, constraints 4459 |
| 24692 | 59283 | nushell | `nu::main` | 9351 | 4848 | 381 | 4318 | 4316 | constraints 12506, liveness 5916, moves 2297 |
| 19057 | 105612 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::typed_column_to_series` | 4603 | 1729 | 85 | 1469 | 1467 | constraints 13573, liveness 2196, moves 1167 |
| 18717 | 8151 | nushell | `nu_command::strings::format::date::expand_locale_specifiers` | 13715 | 5129 | 11 | 5130 | 5130 | liveness 6255, moves 3613, infer 3334 |
| 16025 | 215343 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::series_to_values` | 5483 | 1612 | 63 | 1944 | 1944 | constraints 9208, liveness 2954, moves 1392 |
| 15370 | 20058 | nushell | `nu_protocol::errors::shell_error::<ShellError>::fmt` | 10379 | 9986 | 996 | 15310 | 15310 | constraints 4461, liveness 4210, moves 2088 |
| 15132 | 25824 | nushell | `nu_command::default_context::add_shell_command_context` | 8913 | 2850 | 404 | 3636 | 3636 | constraints 5658, liveness 3931, moves 2316 |
| 14914 | 123465 | nushell | `nu_command::platform::input::list::<SelectWidget<'a>>::render` | 5769 | 2115 | 220 | 2356 | 2356 | constraints 8292, liveness 2448, moves 1525 |
| 13599 | 22466 | nushell | `nu_engine::eval_ir::eval_instruction` | 7960 | 1486 | 192 | 1640 | 1640 | constraints 4541, liveness 4188, moves 2139 |
| 12931 | 35364 | nushell | `nu_cli::repl::loop_iteration` | 7760 | 4463 | 350 | 4008 | 4008 | constraints 5214, liveness 3273, moves 1602 |
| 12528 | 17324 | nushell | `nu_protocol::errors::parse_error::<ParseError>::fmt` | 8443 | 8102 | 909 | 13310 | 13310 | constraints 3645, liveness 3327, moves 1696 |
| 12167 | 74414 | nushell | `nu_mcp::run_http_server` | 7041 | 1947 | 136 | 2017 | 2009 | constraints 4553, liveness 3248, moves 1789 |
| 11720 | 17284 | nushell | `nu::ide::hover` | 7328 | 3012 | 299 | 2471 | 2471 | liveness 3825, constraints 3666, moves 1618 |
| 11183 | 13263 | nushell | `nu_engine::compile::expression::compile_expression` | 6545 | 1584 | 157 | 1791 | 1777 | constraints 4353, liveness 2860, moves 1606 |
| 10913 | 23772 | nushell | `nu_plugin_polars::dataframe::values::nu_expression::expr_to_value` | 6329 | 1318 | 39 | 1169 | 1169 | constraints 4627, liveness 2944, moves 1653 |
| 10569 | 48433 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::input_type_list_to_series` | 4461 | 1245 | 76 | 1362 | 1362 | constraints 5374, liveness 2216, moves 1150 |
| 9778 | 10689 | nushell | `nu_parser::parse_signatures::parse_signature_helper` | 4696 | 1429 | 214 | 1473 | 1467 | constraints 3634, liveness 2791, moves 1234 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 92103 | 57114 | nushell | `nu_cmd_extra::extra::formats::to::html::theme_list::<HtmlTheme>::list` | 48805 | 30331 | 0 | 25997 | 25997 | liveness 28823, moves 27349, constraints 24433 |
| 51288 | 242991 | nushell | `nu_protocol::errors::parse_error::<ParseError>::labels` | 13457 | 5575 | 222 | 5265 | 5265 | constraints 38220, liveness 5203, moves 3091 |
| 51017 | 277380 | nushell | `nu_protocol::errors::shell_error::<ShellError>::labels` | 13112 | 5285 | 218 | 4981 | 4981 | constraints 38308, liveness 4858, moves 3099 |
| 34181 | 25843 | nushell | `nu_command::strings::ansi::ansi_::CODE_LIST` | 14354 | 4947 | 425 | 4014 | 4014 | liveness 21771, moves 4366, constraints 4255 |
| 23323 | 59283 | nushell | `nu::main` | 9351 | 4848 | 381 | 4318 | 4316 | constraints 12077, liveness 5640, moves 2274 |
| 18837 | 8151 | nushell | `nu_command::strings::format::date::expand_locale_specifiers` | 13715 | 5129 | 11 | 5130 | 5130 | liveness 6256, infer 3809, moves 3436 |
| 18508 | 105612 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::typed_column_to_series` | 4603 | 1729 | 85 | 1469 | 1467 | constraints 13375, liveness 2113, moves 1225 |
| 15123 | 215343 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::series_to_values` | 5483 | 1612 | 63 | 1944 | 1944 | constraints 8851, liveness 2850, moves 1333 |
| 14514 | 123465 | nushell | `nu_command::platform::input::list::<SelectWidget<'a>>::render` | 5769 | 2115 | 220 | 2356 | 2356 | constraints 8253, liveness 2352, moves 1535 |
| 14418 | 25824 | nushell | `nu_command::default_context::add_shell_command_context` | 8913 | 2850 | 404 | 3636 | 3636 | constraints 5366, liveness 4182, moves 2291 |
| 14092 | 20058 | nushell | `nu_protocol::errors::shell_error::<ShellError>::fmt` | 10379 | 9986 | 996 | 15310 | 15310 | constraints 4298, liveness 4092, moves 2146 |
| 13215 | 22466 | nushell | `nu_engine::eval_ir::eval_instruction` | 7960 | 1486 | 192 | 1640 | 1640 | constraints 4434, liveness 4157, moves 2126 |
| 12471 | 35364 | nushell | `nu_cli::repl::loop_iteration` | 7760 | 4463 | 350 | 4008 | 4008 | constraints 5078, liveness 3392, moves 1654 |
| 11401 | 17324 | nushell | `nu_protocol::errors::parse_error::<ParseError>::fmt` | 8443 | 8102 | 909 | 13310 | 13310 | constraints 3509, liveness 3310, moves 1685 |
| 11133 | 74414 | nushell | `nu_mcp::run_http_server` | 7041 | 1947 | 136 | 2017 | 2009 | constraints 4257, liveness 3267, moves 1857 |
| 11031 | 17284 | nushell | `nu::ide::hover` | 7328 | 3012 | 299 | 2471 | 2471 | liveness 3785, constraints 3505, moves 1562 |
| 10866 | 23772 | nushell | `nu_plugin_polars::dataframe::values::nu_expression::expr_to_value` | 6329 | 1318 | 39 | 1169 | 1169 | constraints 4497, liveness 3064, moves 1681 |
| 10609 | 13263 | nushell | `nu_engine::compile::expression::compile_expression` | 6545 | 1584 | 157 | 1791 | 1777 | constraints 4254, liveness 2887, moves 1584 |
| 9863 | 48433 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::input_type_list_to_series` | 4461 | 1245 | 76 | 1362 | 1362 | constraints 5184, liveness 2122, moves 1132 |
| 9155 | 10689 | nushell | `nu_parser::parse_signatures::parse_signature_helper` | 4696 | 1429 | 214 | 1473 | 1467 | constraints 3510, liveness 2791, moves 1258 |
