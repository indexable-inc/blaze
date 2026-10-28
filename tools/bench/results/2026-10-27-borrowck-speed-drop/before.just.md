| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| just | polonius | 1417 | 1665 | 1046 | 602 | 0 | 57.6% | 3578 | 79 | 7225 | 18529 |
| just | nll | 1417 | 1665 | 1046 | 545 | 0 | 52.1% | 3578 | 65 | 6866 | 17419 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 243 | 13 | 3 | 17 | 143 | 8 | 0 | 64 | 19 | 1 | 70 |
| nll | 222 | 12 | 2 | 16 | 137 | 7 | 0 | 60 | 1 | 1 | 68 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 18529 | 30107 | just | `just::error::<Error<'_>>::fmt` | 12141 | 6749 | 445 | 6966 | 6966 | constraints 6664, liveness 4785, moves 2577 |
| 16289 | 22622 | just | `just::analyzer::<Analyzer<'run, 'src>>::justfile` | 5092 | 3166 | 163 | 4679 | 4665 | liveness 5731, constraints 5595, moves 1515 |
| 14515 | 19662 | just | `just::subcommand::<Subcommand>::list_module` | 3831 | 2261 | 119 | 3208 | 3205 | constraints 7526, liveness 3046, walk 1479 |
| 13023 | 21482 | just | `just::arguments::<Arguments>::augment_args_for_update` | 5868 | 2927 | 329 | 4284 | 4284 | constraints 4654, walk 3004, liveness 2573 |
| 12793 | 21964 | just | `just::arguments::<Arguments>::augment_args` | 5668 | 2927 | 329 | 4284 | 4284 | constraints 4991, walk 2851, liveness 2294 |
| 12143 | 17949 | just | `just::error::<Error<'src>>::fmt` | 7953 | 7722 | 759 | 11785 | 11785 | constraints 3546, liveness 3158, moves 1590 |
| 10551 | 20065 | just | `just::recipe::<Recipe<'src, Dependency<'src>>>::run_script` | 3304 | 1576 | 71 | 1815 | 1813 | constraints 4140, liveness 2741, walk 1828 |
| 10097 | 33504 | just | `just::arguments::<Arguments>::from_arg_matches_mut` | 4791 | 641 | 52 | 392 | 392 | liveness 3906, constraints 3077, walk 1080 |
| 9780 | 14266 | just | `just::unresolved_recipe::<Recipe<'src, UnresolvedDependency<'src>>>::resolve` | 3693 | 1770 | 157 | 2452 | 2452 | constraints 3717, liveness 3083, moves 996 |
| 9095 | 10153 | just | `just::justfile::<Justfile<'src>>::run` | 2480 | 1332 | 70 | 1959 | 1959 | constraints 3231, liveness 2180, walk 1292 |
| 9008 | 14349 | just | `just::compile_error_kind::<CompileErrorKind<'src>>::fmt` | 6167 | 5933 | 586 | 9039 | 9039 | constraints 2831, liveness 2197, moves 1145 |
| 8933 | 20338 | just | `just::compile_error::<CompileError<'_>>::fmt` | 5464 | 4895 | 327 | 5112 | 5112 | constraints 3603, liveness 1893, moves 995 |
| 8701 | 34489 | just | `just::arguments::<Arguments>::update_from_arg_matches_mut` | 4417 | 1198 | 154 | 847 | 847 | constraints 4135, liveness 1637, walk 1038 |
| 7551 | 7282 | just | `just::evaluator::<Evaluator<'src, 'run>>::evaluate_builtin_function` | 2757 | 1123 | 37 | 1426 | 1426 | liveness 2434, constraints 2003, walk 1938 |
| 7225 | 9351 | just | `just::parser::<Parser<'run, 'src>>::parse_recipe` | 2280 | 1090 | 73 | 1427 | 1427 | constraints 3311, liveness 2031, walk 635 |
| 7191 | 12935 | just | `just::attribute::<Attribute<'src>>::new_arg` | 2571 | 1475 | 55 | 1754 | 1754 | constraints 2981, liveness 1912, walk 959 |
| 6687 | 5894 | just | `just::parser::<Parser<'run, 'src>>::parse_set` | 2657 | 989 | 54 | 1334 | 1334 | liveness 2339, constraints 2325, walk 880 |
| 6477 | 15720 | just | `just::invocation_parser::<InvocationParser<'src, 'run>>::parse_invocation` | 2109 | 1155 | 39 | 1610 | 1610 | constraints 3106, liveness 1501, walk 746 |
| 6061 | 5530 | just | `just::parser::<Parser<'run, 'src>>::parse_item` | 2388 | 1173 | 64 | 1652 | 1652 | constraints 2157, liveness 2094, walk 611 |
| 5638 | 10021 | just | `just::expression::<Expression<'_>>::serialize` | 3297 | 513 | 47 | 541 | 541 | constraints 1813, liveness 1476, walk 1017 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 17419 | 30107 | just | `just::error::<Error<'_>>::fmt` | 12141 | 6749 | 445 | 6966 | 6966 | constraints 6388, liveness 4820, moves 2399 |
| 15387 | 22622 | just | `just::analyzer::<Analyzer<'run, 'src>>::justfile` | 5092 | 3166 | 163 | 4679 | 4665 | liveness 5690, constraints 5460, moves 1515 |
| 13873 | 19662 | just | `just::subcommand::<Subcommand>::list_module` | 3831 | 2261 | 119 | 3208 | 3205 | constraints 7324, liveness 2967, walk 1464 |
| 12537 | 21482 | just | `just::arguments::<Arguments>::augment_args_for_update` | 5868 | 2927 | 329 | 4284 | 4284 | constraints 4575, walk 3024, liveness 2519 |
| 12062 | 21964 | just | `just::arguments::<Arguments>::augment_args` | 5668 | 2927 | 329 | 4284 | 4284 | constraints 4613, walk 2862, liveness 2281 |
| 10946 | 17949 | just | `just::error::<Error<'src>>::fmt` | 7953 | 7722 | 759 | 11785 | 11785 | constraints 3413, liveness 3080, moves 1517 |
| 10058 | 20065 | just | `just::recipe::<Recipe<'src, Dependency<'src>>>::run_script` | 3304 | 1576 | 71 | 1815 | 1813 | constraints 3932, liveness 2700, walk 1803 |
| 9865 | 33504 | just | `just::arguments::<Arguments>::from_arg_matches_mut` | 4791 | 641 | 52 | 392 | 392 | liveness 3876, constraints 3050, moves 1088 |
| 9092 | 14266 | just | `just::unresolved_recipe::<Recipe<'src, UnresolvedDependency<'src>>>::resolve` | 3693 | 1770 | 157 | 2452 | 2452 | constraints 3446, liveness 3005, moves 979 |
| 8557 | 34489 | just | `just::arguments::<Arguments>::update_from_arg_matches_mut` | 4417 | 1198 | 154 | 847 | 847 | constraints 4104, liveness 1678, walk 1059 |
| 8495 | 20338 | just | `just::compile_error::<CompileError<'_>>::fmt` | 5464 | 4895 | 327 | 5112 | 5112 | constraints 3414, liveness 1931, moves 1021 |
| 8088 | 14349 | just | `just::compile_error_kind::<CompileErrorKind<'src>>::fmt` | 6167 | 5933 | 586 | 9039 | 9039 | constraints 2675, liveness 2142, moves 1140 |
| 7890 | 10153 | just | `just::justfile::<Justfile<'src>>::run` | 2480 | 1332 | 70 | 1959 | 1959 | constraints 3166, liveness 2128, walk 1298 |
| 7379 | 7282 | just | `just::evaluator::<Evaluator<'src, 'run>>::evaluate_builtin_function` | 2757 | 1123 | 37 | 1426 | 1426 | liveness 2395, constraints 1962, walk 1940 |
| 6866 | 12935 | just | `just::attribute::<Attribute<'src>>::new_arg` | 2571 | 1475 | 55 | 1754 | 1754 | constraints 2848, liveness 1914, walk 955 |
| 6768 | 9351 | just | `just::parser::<Parser<'run, 'src>>::parse_recipe` | 2280 | 1090 | 73 | 1427 | 1427 | constraints 3228, liveness 1940, walk 616 |
| 6452 | 5894 | just | `just::parser::<Parser<'run, 'src>>::parse_set` | 2657 | 989 | 54 | 1334 | 1334 | liveness 2255, constraints 2208, walk 868 |
| 6165 | 15720 | just | `just::invocation_parser::<InvocationParser<'src, 'run>>::parse_invocation` | 2109 | 1155 | 39 | 1610 | 1610 | constraints 3025, liveness 1424, walk 746 |
| 5599 | 5530 | just | `just::parser::<Parser<'run, 'src>>::parse_item` | 2388 | 1173 | 64 | 1652 | 1652 | constraints 2017, liveness 1996, walk 605 |
| 5528 | 10021 | just | `just::expression::<Expression<'_>>::serialize` | 3297 | 513 | 47 | 541 | 541 | constraints 1773, liveness 1454, walk 1010 |
