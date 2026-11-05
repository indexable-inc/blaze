| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| just | polonius | 1417 | 1701 | 1063 | 542 | 0 | 51.0% | 3540 | 76 | 5817 | 19110 |
| just | nll | 1417 | 1701 | 1063 | 483 | 0 | 45.4% | 3540 | 60 | 5435 | 18311 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 247 | 12 | 2 | 17 | 115 | 7 | 0 | 64 | 19 | 1 | 35 |
| nll | 223 | 12 | 2 | 16 | 109 | 7 | 0 | 59 | 1 | 1 | 33 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 19110 | 32092 | just | `just::error::<Error<'_>>::fmt` | 12141 | 6749 | 445 | 6966 | 6966 | constraints 7572, liveness 4570, moves 2568 |
| 14310 | 22830 | just | `just::analyzer::<Analyzer<'run, 'src>>::justfile` | 5092 | 3166 | 163 | 4679 | 4665 | constraints 5456, liveness 4530, moves 1511 |
| 13791 | 19918 | just | `just::subcommand::<Subcommand>::list_module` | 3831 | 2261 | 119 | 3208 | 3205 | constraints 8044, liveness 2612, moves 1051 |
| 12393 | 18713 | just | `just::error::<Error<'src>>::fmt` | 7953 | 7722 | 759 | 11785 | 11785 | constraints 3905, liveness 3101, moves 1565 |
| 11693 | 22262 | just | `just::arguments::<Arguments>::augment_args` | 5668 | 2927 | 329 | 4284 | 4284 | constraints 5795, liveness 2389, moves 1337 |
| 11627 | 23446 | just | `just::arguments::<Arguments>::augment_args_for_update` | 5868 | 2927 | 329 | 4284 | 4284 | constraints 5561, liveness 2536, moves 1411 |
| 10253 | 34184 | just | `just::arguments::<Arguments>::from_arg_matches_mut` | 4791 | 641 | 52 | 392 | 392 | liveness 4335, constraints 3221, moves 1135 |
| 9363 | 21422 | just | `just::compile_error::<CompileError<'_>>::fmt` | 5464 | 4895 | 327 | 5112 | 5112 | constraints 4099, liveness 1788, moves 1068 |
| 9354 | 14980 | just | `just::compile_error_kind::<CompileErrorKind<'src>>::fmt` | 6167 | 5933 | 586 | 9039 | 9039 | constraints 3151, liveness 2220, moves 1149 |
| 8488 | 14522 | just | `just::unresolved_recipe::<Recipe<'src, UnresolvedDependency<'src>>>::resolve` | 3693 | 1770 | 157 | 2452 | 2452 | constraints 3430, liveness 2314, moves 968 |
| 8414 | 36389 | just | `just::arguments::<Arguments>::update_from_arg_matches_mut` | 4417 | 1198 | 154 | 847 | 847 | constraints 4227, liveness 1686, moves 962 |
| 8362 | 19845 | just | `just::recipe::<Recipe<'src, Dependency<'src>>>::run_script` | 3304 | 1576 | 71 | 1815 | 1813 | constraints 3952, liveness 2085, moves 843 |
| 7779 | 10640 | just | `just::justfile::<Justfile<'src>>::run` | 2480 | 1332 | 70 | 1959 | 1959 | constraints 3345, liveness 1666, scopes 1131 |
| 6071 | 9400 | just | `just::parser::<Parser<'run, 'src>>::parse_recipe` | 2280 | 1090 | 73 | 1427 | 1427 | constraints 3252, liveness 1287, moves 526 |
| 5817 | 13773 | just | `just::attribute::<Attribute<'src>>::new_arg` | 2571 | 1475 | 55 | 1754 | 1754 | constraints 3052, liveness 1102, moves 637 |
| 5782 | 16292 | just | `just::invocation_parser::<InvocationParser<'src, 'run>>::parse_invocation` | 2109 | 1155 | 39 | 1610 | 1610 | constraints 3180, liveness 1136, moves 532 |
| 5083 | 5502 | just | `just::parser::<Parser<'run, 'src>>::parse_item` | 2388 | 1173 | 64 | 1652 | 1652 | constraints 2309, liveness 1198, moves 546 |
| 4980 | 7461 | just | `just::evaluator::<Evaluator<'src, 'run>>::evaluate_builtin_function` | 2757 | 1123 | 37 | 1426 | 1426 | constraints 2006, liveness 1395, moves 603 |
| 4979 | 5892 | just | `just::parser::<Parser<'run, 'src>>::parse_set` | 2657 | 989 | 54 | 1334 | 1334 | constraints 2222, liveness 1292, moves 589 |
| 4777 | 10270 | just | `just::expression::<Expression<'_>>::serialize` | 3297 | 513 | 47 | 541 | 541 | constraints 1772, liveness 1473, moves 668 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 18311 | 32092 | just | `just::error::<Error<'_>>::fmt` | 12141 | 6749 | 445 | 6966 | 6966 | constraints 7581, liveness 4493, moves 2527 |
| 13481 | 22830 | just | `just::analyzer::<Analyzer<'run, 'src>>::justfile` | 5092 | 3166 | 163 | 4679 | 4665 | constraints 5338, liveness 4561, moves 1486 |
| 13193 | 19918 | just | `just::subcommand::<Subcommand>::list_module` | 3831 | 2261 | 119 | 3208 | 3205 | constraints 7866, liveness 2555, moves 1039 |
| 11760 | 18713 | just | `just::error::<Error<'src>>::fmt` | 7953 | 7722 | 759 | 11785 | 11785 | constraints 4008, liveness 3071, moves 1562 |
| 11080 | 23446 | just | `just::arguments::<Arguments>::augment_args_for_update` | 5868 | 2927 | 329 | 4284 | 4284 | constraints 5495, liveness 2429, moves 1405 |
| 11003 | 22262 | just | `just::arguments::<Arguments>::augment_args` | 5668 | 2927 | 329 | 4284 | 4284 | constraints 5547, liveness 2455, moves 1319 |
| 10040 | 34184 | just | `just::arguments::<Arguments>::from_arg_matches_mut` | 4791 | 641 | 52 | 392 | 392 | liveness 4138, constraints 3243, moves 1116 |
| 8797 | 21422 | just | `just::compile_error::<CompileError<'_>>::fmt` | 5464 | 4895 | 327 | 5112 | 5112 | constraints 3833, liveness 1807, moves 1032 |
| 8462 | 14980 | just | `just::compile_error_kind::<CompileErrorKind<'src>>::fmt` | 6167 | 5933 | 586 | 9039 | 9039 | constraints 2884, liveness 2168, moves 1174 |
| 8106 | 36389 | just | `just::arguments::<Arguments>::update_from_arg_matches_mut` | 4417 | 1198 | 154 | 847 | 847 | constraints 4152, liveness 1635, moves 922 |
| 7919 | 14522 | just | `just::unresolved_recipe::<Recipe<'src, UnresolvedDependency<'src>>>::resolve` | 3693 | 1770 | 157 | 2452 | 2452 | constraints 3233, liveness 2308, moves 959 |
| 7859 | 19845 | just | `just::recipe::<Recipe<'src, Dependency<'src>>>::run_script` | 3304 | 1576 | 71 | 1815 | 1813 | constraints 3797, liveness 1950, moves 833 |
| 6381 | 10640 | just | `just::justfile::<Justfile<'src>>::run` | 2480 | 1332 | 70 | 1959 | 1959 | constraints 3236, liveness 1536, moves 706 |
| 5658 | 9400 | just | `just::parser::<Parser<'run, 'src>>::parse_recipe` | 2280 | 1090 | 73 | 1427 | 1427 | constraints 3129, liveness 1246, moves 507 |
| 5435 | 13773 | just | `just::attribute::<Attribute<'src>>::new_arg` | 2571 | 1475 | 55 | 1754 | 1754 | constraints 2864, liveness 1091, moves 631 |
| 5429 | 16292 | just | `just::invocation_parser::<InvocationParser<'src, 'run>>::parse_invocation` | 2109 | 1155 | 39 | 1610 | 1610 | constraints 3061, liveness 1086, moves 503 |
| 4772 | 7461 | just | `just::evaluator::<Evaluator<'src, 'run>>::evaluate_builtin_function` | 2757 | 1123 | 37 | 1426 | 1426 | constraints 1913, liveness 1372, moves 607 |
| 4769 | 5892 | just | `just::parser::<Parser<'run, 'src>>::parse_set` | 2657 | 989 | 54 | 1334 | 1334 | constraints 2112, liveness 1222, moves 574 |
| 4763 | 10270 | just | `just::expression::<Expression<'_>>::serialize` | 3297 | 513 | 47 | 541 | 541 | constraints 1755, liveness 1457, moves 687 |
| 4643 | 5502 | just | `just::parser::<Parser<'run, 'src>>::parse_item` | 2388 | 1173 | 64 | 1652 | 1652 | constraints 2150, liveness 1136, moves 526 |
