| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| nushell | polonius | 35727 | 33696 | 14016 | 6500 | 2 | 46.4% | 23589 | 70 | 1815 | 107596 |
| nushell | nll | 35727 | 33696 | 14016 | 5732 | 1 | 40.9% | 23589 | 58 | 1652 | 107815 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 2956 | 106 | 23 | 122 | 1471 | 93 | 2 | 808 | 154 | 12 | 375 |
| nll | 2615 | 101 | 20 | 114 | 1372 | 81 | 1 | 737 | 12 | 9 | 349 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 107596 | 16594 | nushell | `nu_mcp::server::<NushellMcpServer>::tool_router` | 57 | 2 | 0 | 0 | 0 | constraints 107462, liveness 51, moves 34 |
| 91943 | 56849 | nushell | `nu_cmd_extra::extra::formats::to::html::theme_list::<HtmlTheme>::list` | 48805 | 30331 | 0 | 25997 | 25997 | liveness 28328, moves 28063, constraints 23350 |
| 52602 | 247946 | nushell | `nu_protocol::errors::parse_error::<ParseError>::labels` | 13457 | 5575 | 222 | 5265 | 5265 | constraints 38838, liveness 5278, moves 3120 |
| 52077 | 283197 | nushell | `nu_protocol::errors::shell_error::<ShellError>::labels` | 13112 | 5285 | 218 | 4981 | 4981 | constraints 38712, liveness 4897, moves 3174 |
| 34388 | 25790 | nushell | `nu_command::strings::ansi::ansi_::CODE_LIST` | 14354 | 4947 | 425 | 4014 | 4014 | liveness 21778, moves 4273, constraints 4217 |
| 24534 | 61816 | nushell | `nu::main` | 9351 | 4848 | 381 | 4318 | 4316 | constraints 12542, liveness 5780, moves 2279 |
| 19895 | 8567 | nushell | `nu_command::strings::format::date::expand_locale_specifiers` | 13715 | 5129 | 11 | 5130 | 5130 | liveness 6346, infer 4042, moves 3453 |
| 18966 | 106891 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::typed_column_to_series` | 4603 | 1729 | 85 | 1469 | 1467 | constraints 13478, liveness 2219, moves 1177 |
| 17522 | 230989 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::series_to_values` | 5483 | 1612 | 63 | 1944 | 1944 | constraints 10221, liveness 3195, moves 1468 |
| 15730 | 133122 | nushell | `nu_command::platform::input::list::<SelectWidget<'a>>::render` | 5769 | 2115 | 220 | 2356 | 2356 | constraints 8939, liveness 2555, moves 1575 |
| 15382 | 19880 | nushell | `nu_protocol::errors::shell_error::<ShellError>::fmt` | 10379 | 9986 | 996 | 15310 | 15310 | constraints 4417, liveness 4286, moves 2089 |
| 14775 | 24989 | nushell | `nu_command::default_context::add_shell_command_context` | 8913 | 2850 | 404 | 3636 | 3636 | constraints 5350, liveness 4044, moves 2167 |
| 13822 | 22481 | nushell | `nu_engine::eval_ir::eval_instruction` | 7960 | 1486 | 192 | 1640 | 1640 | constraints 4633, liveness 4261, moves 2147 |
| 13079 | 81428 | nushell | `nu_mcp::run_http_server` | 7041 | 1947 | 136 | 2017 | 2009 | constraints 5468, liveness 3271, moves 1843 |
| 12830 | 34815 | nushell | `nu_cli::repl::loop_iteration` | 7760 | 4463 | 350 | 4008 | 4008 | constraints 5091, liveness 3368, moves 1613 |
| 12631 | 17421 | nushell | `nu_protocol::errors::parse_error::<ParseError>::fmt` | 8443 | 8102 | 909 | 13310 | 13310 | constraints 3670, liveness 3378, moves 1731 |
| 11763 | 17060 | nushell | `nu::ide::hover` | 7328 | 3012 | 299 | 2471 | 2471 | liveness 3898, constraints 3613, moves 1573 |
| 11754 | 27573 | nushell | `nu_plugin_polars::dataframe::values::nu_expression::expr_to_value` | 6329 | 1318 | 39 | 1169 | 1169 | constraints 5024, liveness 3103, moves 1854 |
| 10977 | 12912 | nushell | `nu_engine::compile::expression::compile_expression` | 6545 | 1584 | 157 | 1791 | 1777 | constraints 4223, liveness 2880, moves 1549 |
| 10760 | 49836 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::input_type_list_to_series` | 4461 | 1245 | 76 | 1362 | 1362 | constraints 5456, liveness 2255, moves 1160 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 107815 | 16594 | nushell | `nu_mcp::server::<NushellMcpServer>::tool_router` | 57 | 2 | 0 | 0 | 0 | constraints 107692, liveness 51, moves 29 |
| 88350 | 56849 | nushell | `nu_cmd_extra::extra::formats::to::html::theme_list::<HtmlTheme>::list` | 48805 | 30331 | 0 | 25997 | 25997 | liveness 28256, moves 26646, constraints 22973 |
| 51676 | 247946 | nushell | `nu_protocol::errors::parse_error::<ParseError>::labels` | 13457 | 5575 | 222 | 5265 | 5265 | constraints 38606, liveness 5299, moves 3025 |
| 50979 | 283197 | nushell | `nu_protocol::errors::shell_error::<ShellError>::labels` | 13112 | 5285 | 218 | 4981 | 4981 | constraints 38414, liveness 4912, moves 2951 |
| 33530 | 25790 | nushell | `nu_command::strings::ansi::ansi_::CODE_LIST` | 14354 | 4947 | 425 | 4014 | 4014 | liveness 21130, moves 4293, constraints 4053 |
| 23280 | 61816 | nushell | `nu::main` | 9351 | 4848 | 381 | 4318 | 4316 | constraints 11973, liveness 5722, moves 2237 |
| 19371 | 230989 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::series_to_values` | 5483 | 1612 | 63 | 1944 | 1944 | constraints 11391, liveness 3622, moves 1639 |
| 18332 | 106891 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::typed_column_to_series` | 4603 | 1729 | 85 | 1469 | 1467 | constraints 13314, liveness 2034, moves 1152 |
| 18191 | 8567 | nushell | `nu_command::strings::format::date::expand_locale_specifiers` | 13715 | 5129 | 11 | 5130 | 5130 | liveness 6278, infer 3405, moves 3389 |
| 15005 | 133122 | nushell | `nu_command::platform::input::list::<SelectWidget<'a>>::render` | 5769 | 2115 | 220 | 2356 | 2356 | constraints 8654, liveness 2501, moves 1474 |
| 14370 | 19880 | nushell | `nu_protocol::errors::shell_error::<ShellError>::fmt` | 10379 | 9986 | 996 | 15310 | 15310 | liveness 4496, constraints 4333, moves 1991 |
| 13946 | 24989 | nushell | `nu_command::default_context::add_shell_command_context` | 8913 | 2850 | 404 | 3636 | 3636 | constraints 5122, liveness 3905, moves 2216 |
| 13428 | 22481 | nushell | `nu_engine::eval_ir::eval_instruction` | 7960 | 1486 | 192 | 1640 | 1640 | constraints 4663, liveness 4159, moves 2125 |
| 12252 | 34815 | nushell | `nu_cli::repl::loop_iteration` | 7760 | 4463 | 350 | 4008 | 4008 | constraints 4982, liveness 3338, moves 1609 |
| 11527 | 17421 | nushell | `nu_protocol::errors::parse_error::<ParseError>::fmt` | 8443 | 8102 | 909 | 13310 | 13310 | constraints 3564, liveness 3384, moves 1677 |
| 11384 | 27573 | nushell | `nu_plugin_polars::dataframe::values::nu_expression::expr_to_value` | 6329 | 1318 | 39 | 1169 | 1169 | constraints 4827, liveness 3029, moves 1782 |
| 11047 | 81428 | nushell | `nu_mcp::run_http_server` | 7041 | 1947 | 136 | 2017 | 2009 | constraints 4316, liveness 3175, moves 1805 |
| 10977 | 17060 | nushell | `nu::ide::hover` | 7328 | 3012 | 299 | 2471 | 2471 | liveness 3796, constraints 3483, moves 1575 |
| 10440 | 12912 | nushell | `nu_engine::compile::expression::compile_expression` | 6545 | 1584 | 157 | 1791 | 1777 | constraints 4142, liveness 2937, moves 1510 |
| 10209 | 49836 | nushell | `nu_plugin_polars::dataframe::values::nu_dataframe::conversion::input_type_list_to_series` | 4461 | 1245 | 76 | 1362 | 1362 | constraints 5235, liveness 2290, moves 1191 |
