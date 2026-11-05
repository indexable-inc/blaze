| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| just | polonius | 1417 | 1675 | 1075 | 603 | 0 | 56.1% | 3568 | 77 | 7148 | 18868 |
| just | nll | 1417 | 1675 | 1075 | 547 | 0 | 50.9% | 3568 | 65 | 6793 | 17842 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 244 | 13 | 3 | 17 | 143 | 8 | 0 | 63 | 19 | 1 | 70 |
| nll | 224 | 12 | 2 | 16 | 137 | 7 | 0 | 58 | 1 | 1 | 68 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 18868 | 34135 | just | `just::error::<Error<'_>>::fmt` | 12141 | 6749 | 445 | 6966 | 6966 | constraints 7338, liveness 4508, moves 2478 |
| 16681 | 23490 | just | `just::analyzer::<Analyzer<'run, 'src>>::justfile` | 5092 | 3166 | 163 | 4679 | 4665 | constraints 5833, liveness 5817, moves 1495 |
| 13933 | 18317 | just | `just::subcommand::<Subcommand>::list_module` | 3831 | 2261 | 119 | 3208 | 3205 | constraints 7037, liveness 2993, walk 1472 |
| 13176 | 23051 | just | `just::arguments::<Arguments>::augment_args_for_update` | 5868 | 2927 | 329 | 4284 | 4284 | constraints 4829, walk 2995, liveness 2619 |
| 12682 | 22154 | just | `just::arguments::<Arguments>::augment_args` | 5668 | 2927 | 329 | 4284 | 4284 | constraints 4855, walk 2826, liveness 2342 |
| 12470 | 19624 | just | `just::error::<Error<'src>>::fmt` | 7953 | 7722 | 759 | 11785 | 11785 | constraints 3977, liveness 3060, moves 1498 |
| 10284 | 19662 | just | `just::recipe::<Recipe<'src, Dependency<'src>>>::run_script` | 3304 | 1576 | 71 | 1815 | 1813 | constraints 3954, liveness 2684, walk 1839 |
| 10000 | 33268 | just | `just::arguments::<Arguments>::from_arg_matches_mut` | 4791 | 641 | 52 | 392 | 392 | liveness 3766, constraints 3151, moves 1134 |
| 9353 | 13696 | just | `just::unresolved_recipe::<Recipe<'src, UnresolvedDependency<'src>>>::resolve` | 3693 | 1770 | 157 | 2452 | 2452 | constraints 3412, liveness 2997, moves 963 |
| 9299 | 15120 | just | `just::compile_error_kind::<CompileErrorKind<'src>>::fmt` | 6167 | 5933 | 586 | 9039 | 9039 | constraints 3066, liveness 2205, moves 1152 |
| 9209 | 10388 | just | `just::justfile::<Justfile<'src>>::run` | 2480 | 1332 | 70 | 1959 | 1959 | constraints 3261, liveness 2211, walk 1325 |
| 9001 | 20787 | just | `just::compile_error::<CompileError<'_>>::fmt` | 5464 | 4895 | 327 | 5112 | 5112 | constraints 3644, liveness 1721, moves 1028 |
| 8612 | 35114 | just | `just::arguments::<Arguments>::update_from_arg_matches_mut` | 4417 | 1198 | 154 | 847 | 847 | constraints 4091, liveness 1612, walk 999 |
| 7836 | 7778 | just | `just::evaluator::<Evaluator<'src, 'run>>::evaluate_builtin_function` | 2757 | 1123 | 37 | 1426 | 1426 | liveness 2429, constraints 2205, walk 2010 |
| 7148 | 13167 | just | `just::attribute::<Attribute<'src>>::new_arg` | 2571 | 1475 | 55 | 1754 | 1754 | constraints 2937, liveness 1926, walk 976 |
| 6898 | 8986 | just | `just::parser::<Parser<'run, 'src>>::parse_recipe` | 2280 | 1090 | 73 | 1427 | 1427 | constraints 3130, liveness 1947, walk 634 |
| 6664 | 17098 | just | `just::invocation_parser::<InvocationParser<'src, 'run>>::parse_invocation` | 2109 | 1155 | 39 | 1610 | 1610 | constraints 3269, liveness 1514, walk 769 |
| 6433 | 5703 | just | `just::parser::<Parser<'run, 'src>>::parse_set` | 2657 | 989 | 54 | 1334 | 1334 | liveness 2234, constraints 2169, walk 884 |
| 5939 | 5471 | just | `just::parser::<Parser<'run, 'src>>::parse_item` | 2388 | 1173 | 64 | 1652 | 1652 | liveness 2053, constraints 2031, walk 618 |
| 5766 | 7077 | just | `just::evaluator::<Evaluator<'src, 'run>>::evaluate_value` | 2154 | 1028 | 29 | 1334 | 1334 | constraints 1935, liveness 1624, walk 1262 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 17842 | 34135 | just | `just::error::<Error<'_>>::fmt` | 12141 | 6749 | 445 | 6966 | 6966 | constraints 7068, liveness 4415, moves 2431 |
| 15618 | 23490 | just | `just::analyzer::<Analyzer<'run, 'src>>::justfile` | 5092 | 3166 | 163 | 4679 | 4665 | liveness 5751, constraints 5625, moves 1453 |
| 13468 | 18317 | just | `just::subcommand::<Subcommand>::list_module` | 3831 | 2261 | 119 | 3208 | 3205 | constraints 6960, liveness 2878, walk 1443 |
| 12531 | 23051 | just | `just::arguments::<Arguments>::augment_args_for_update` | 5868 | 2927 | 329 | 4284 | 4284 | constraints 4729, walk 3002, liveness 2475 |
| 12141 | 22154 | just | `just::arguments::<Arguments>::augment_args` | 5668 | 2927 | 329 | 4284 | 4284 | constraints 4714, walk 2821, liveness 2287 |
| 11314 | 19624 | just | `just::error::<Error<'src>>::fmt` | 7953 | 7722 | 759 | 11785 | 11785 | constraints 3711, liveness 3121, moves 1465 |
| 9933 | 19662 | just | `just::recipe::<Recipe<'src, Dependency<'src>>>::run_script` | 3304 | 1576 | 71 | 1815 | 1813 | constraints 3856, liveness 2636, walk 1855 |
| 9861 | 33268 | just | `just::arguments::<Arguments>::from_arg_matches_mut` | 4791 | 641 | 52 | 392 | 392 | liveness 3693, constraints 3074, moves 1132 |
| 8783 | 13696 | just | `just::unresolved_recipe::<Recipe<'src, UnresolvedDependency<'src>>>::resolve` | 3693 | 1770 | 157 | 2452 | 2452 | constraints 3245, liveness 2966, moves 947 |
| 8353 | 35114 | just | `just::arguments::<Arguments>::update_from_arg_matches_mut` | 4417 | 1198 | 154 | 847 | 847 | constraints 4079, liveness 1613, walk 1001 |
| 8349 | 15120 | just | `just::compile_error_kind::<CompileErrorKind<'src>>::fmt` | 6167 | 5933 | 586 | 9039 | 9039 | constraints 2725, liveness 2291, moves 1136 |
| 8225 | 20787 | just | `just::compile_error::<CompileError<'_>>::fmt` | 5464 | 4895 | 327 | 5112 | 5112 | constraints 3494, liveness 1718, moves 967 |
| 7971 | 10388 | just | `just::justfile::<Justfile<'src>>::run` | 2480 | 1332 | 70 | 1959 | 1959 | constraints 3230, liveness 2144, walk 1307 |
| 7593 | 7778 | just | `just::evaluator::<Evaluator<'src, 'run>>::evaluate_builtin_function` | 2757 | 1123 | 37 | 1426 | 1426 | liveness 2382, constraints 2121, walk 2017 |
| 6793 | 13167 | just | `just::attribute::<Attribute<'src>>::new_arg` | 2571 | 1475 | 55 | 1754 | 1754 | constraints 2843, liveness 1865, walk 955 |
| 6550 | 8986 | just | `just::parser::<Parser<'run, 'src>>::parse_recipe` | 2280 | 1090 | 73 | 1427 | 1427 | constraints 3086, liveness 1890, walk 610 |
| 6331 | 17098 | just | `just::invocation_parser::<InvocationParser<'src, 'run>>::parse_invocation` | 2109 | 1155 | 39 | 1610 | 1610 | constraints 3162, liveness 1451, walk 756 |
| 6301 | 5703 | just | `just::parser::<Parser<'run, 'src>>::parse_set` | 2657 | 989 | 54 | 1334 | 1334 | liveness 2190, constraints 2144, walk 878 |
| 5602 | 5471 | just | `just::parser::<Parser<'run, 'src>>::parse_item` | 2388 | 1173 | 64 | 1652 | 1652 | constraints 1998, liveness 1990, walk 617 |
| 5572 | 10651 | just | `just::expression::<Expression<'_>>::serialize` | 3297 | 513 | 47 | 541 | 541 | constraints 1834, liveness 1493, walk 984 |
