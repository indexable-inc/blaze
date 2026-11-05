| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| nushell | polonius | 35727 | 32628 | 13771 | 6328 | 2 | 45.9% | 22709 | 70 | 1778 | 92361 |
| nushell | nll | 35727 | 32628 | 13771 | 5603 | 1 | 40.7% | 22709 | 58 | 1640 | 90389 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 2824 | 106 | 23 | 122 | 1441 | 92 | 2 | 804 | 154 | 11 | 378 |
| nll | 2512 | 102 | 20 | 116 | 1349 | 80 | 1 | 736 | 12 | 9 | 350 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 92361 | 56472 | nushell | `nu_cmd_extra::extra::formats::to::html::theme_list::<HtmlTheme>::list` | 48805 | 30331 | 0 | 25997 | 25997 | liveness 28420, moves 27817, constraints 23542 |
| 51952 | 279654 | nushell | `nu_protocol::errors::shell_error::<ShellError>::labels` | 13112 | 5285 | 218 | 4981 | 4981 | constraints 38825, liveness 4845, moves 3023 |
| 51713 | 246962 | nushell | `nu_protocol::errors::parse_error::<ParseError>::labels` | 13457 | 5575 | 222 | 5265 | 5265 | constraints 38282, liveness 5037, moves 3082 |
| 34887 | 25557 | nushell | `nu_command::strings::ansi::ansi_::CODE_LIST` | 14354 | 4947 | 425 | 4014 | 4014 | liveness 21943, moves 4697, constraints 4352 |
| 23739 | 57975 | nushell | `nu::main` | 9351 | 4848 | 381 | 4318 | 4316 | constraints 12128, liveness 5462, moves 2242 |
| 19336 | 8091 | nushell | `nu_command::strings::format::date::expand_locale_specifiers` | 13715 | 5129 | 11 | 5130 | 5130 | liveness 6197, infer 4033, moves 3430 |
| 19067 | 107247 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::typed_column_to_series` | 4603 | 1729 | 85 | 1469 | 1467 | constraints 13643, liveness 2076, moves 1201 |
| 15990 | 215185 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::series_to_values` | 5483 | 1612 | 63 | 1944 | 1944 | constraints 9210, liveness 2940, moves 1348 |
| 15133 | 20006 | nushell | `nu_protocol::errors::shell_error::<ShellError>::fmt` | 10379 | 9986 | 996 | 15310 | 15310 | constraints 4533, liveness 4014, moves 1985 |
| 15022 | 25908 | nushell | `nu_command::default_context::add_shell_command_context` | 8913 | 2850 | 404 | 3636 | 3636 | constraints 5583, liveness 3931, moves 2285 |
| 14865 | 121656 | nushell | `nu_command::platform::input::list::<SelectWidget<'a>>::render` | 5769 | 2115 | 220 | 2356 | 2356 | constraints 8366, liveness 2393, moves 1462 |
| 13698 | 22707 | nushell | `nu_engine::eval_ir::eval_instruction` | 7960 | 1486 | 192 | 1640 | 1640 | constraints 4529, liveness 4247, moves 2162 |
| 12679 | 35082 | nushell | `nu_cli::repl::loop_iteration` | 7760 | 4463 | 350 | 4008 | 4008 | constraints 5128, liveness 3176, moves 1568 |
| 12411 | 17666 | nushell | `nu_protocol::errors::parse_error::<ParseError>::fmt` | 8443 | 8102 | 909 | 13310 | 13310 | constraints 3726, liveness 3190, moves 1653 |
| 11898 | 74262 | nushell | `nu_mcp::run_http_server` | 7041 | 1947 | 136 | 2017 | 2009 | constraints 4399, liveness 3184, moves 1814 |
| 11309 | 13237 | nushell | `nu_engine::compile::expression::compile_expression` | 6545 | 1584 | 157 | 1791 | 1777 | constraints 4349, liveness 2991, moves 1611 |
| 11263 | 16399 | nushell | `nu::ide::hover` | 7328 | 3012 | 299 | 2471 | 2471 | liveness 3629, constraints 3501, moves 1566 |
| 11015 | 23856 | nushell | `nu_plugin_polars::dataframe::values::nu_expression::expr_to_value` | 6329 | 1318 | 39 | 1169 | 1169 | constraints 4592, liveness 3016, moves 1714 |
| 10605 | 49146 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::input_type_list_to_series` | 4461 | 1245 | 76 | 1362 | 1362 | constraints 5382, liveness 2190, moves 1191 |
| 9716 | 10623 | nushell | `nu_parser::parse_signatures::parse_signature_helper` | 4696 | 1429 | 214 | 1473 | 1467 | constraints 3606, liveness 2711, moves 1236 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 90389 | 56472 | nushell | `nu_cmd_extra::extra::formats::to::html::theme_list::<HtmlTheme>::list` | 48805 | 30331 | 0 | 25997 | 25997 | liveness 28562, moves 26781, constraints 24038 |
| 50981 | 279654 | nushell | `nu_protocol::errors::shell_error::<ShellError>::labels` | 13112 | 5285 | 218 | 4981 | 4981 | constraints 38298, liveness 4750, moves 3185 |
| 50884 | 246962 | nushell | `nu_protocol::errors::parse_error::<ParseError>::labels` | 13457 | 5575 | 222 | 5265 | 5265 | constraints 38088, liveness 4920, moves 3075 |
| 33085 | 25557 | nushell | `nu_command::strings::ansi::ansi_::CODE_LIST` | 14354 | 4947 | 425 | 4014 | 4014 | liveness 20933, moves 4204, constraints 4156 |
| 22619 | 57975 | nushell | `nu::main` | 9351 | 4848 | 381 | 4318 | 4316 | constraints 11695, liveness 5417, moves 2250 |
| 18583 | 107247 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::typed_column_to_series` | 4603 | 1729 | 85 | 1469 | 1467 | constraints 13482, liveness 2080, moves 1153 |
| 18472 | 8091 | nushell | `nu_command::strings::format::date::expand_locale_specifiers` | 13715 | 5129 | 11 | 5130 | 5130 | liveness 6194, infer 3680, moves 3511 |
| 15198 | 215185 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::series_to_values` | 5483 | 1612 | 63 | 1944 | 1944 | constraints 8945, liveness 2878, moves 1314 |
| 14472 | 121656 | nushell | `nu_command::platform::input::list::<SelectWidget<'a>>::render` | 5769 | 2115 | 220 | 2356 | 2356 | constraints 8247, liveness 2366, moves 1532 |
| 14101 | 20006 | nushell | `nu_protocol::errors::shell_error::<ShellError>::fmt` | 10379 | 9986 | 996 | 15310 | 15310 | constraints 4526, liveness 3971, moves 2036 |
| 14030 | 25908 | nushell | `nu_command::default_context::add_shell_command_context` | 8913 | 2850 | 404 | 3636 | 3636 | constraints 5326, liveness 3906, moves 2231 |
| 13159 | 22707 | nushell | `nu_engine::eval_ir::eval_instruction` | 7960 | 1486 | 192 | 1640 | 1640 | constraints 4454, liveness 4062, moves 2128 |
| 12108 | 35082 | nushell | `nu_cli::repl::loop_iteration` | 7760 | 4463 | 350 | 4008 | 4008 | constraints 4951, liveness 3233, moves 1613 |
| 11234 | 17666 | nushell | `nu_protocol::errors::parse_error::<ParseError>::fmt` | 8443 | 8102 | 909 | 13310 | 13310 | constraints 3517, liveness 3187, moves 1624 |
| 10827 | 23856 | nushell | `nu_plugin_polars::dataframe::values::nu_expression::expr_to_value` | 6329 | 1318 | 39 | 1169 | 1169 | constraints 4456, liveness 2895, moves 1752 |
| 10720 | 74262 | nushell | `nu_mcp::run_http_server` | 7041 | 1947 | 136 | 2017 | 2009 | constraints 4122, liveness 3070, moves 1805 |
| 10656 | 16399 | nushell | `nu::ide::hover` | 7328 | 3012 | 299 | 2471 | 2471 | liveness 3563, constraints 3415, moves 1528 |
| 10558 | 13237 | nushell | `nu_engine::compile::expression::compile_expression` | 6545 | 1584 | 157 | 1791 | 1777 | constraints 4253, liveness 2846, moves 1551 |
| 10096 | 49146 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::input_type_list_to_series` | 4461 | 1245 | 76 | 1362 | 1362 | constraints 5245, liveness 2184, moves 1195 |
| 9029 | 10623 | nushell | `nu_parser::parse_signatures::parse_signature_helper` | 4696 | 1429 | 214 | 1473 | 1467 | constraints 3515, liveness 2755, moves 1193 |
