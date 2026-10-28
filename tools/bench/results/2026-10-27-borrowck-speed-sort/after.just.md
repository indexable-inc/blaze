| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| just | polonius | 1417 | 1526 | 1011 | 520 | 0 | 51.4% | 3356 | 73 | 5531 | 17686 |
| just | nll | 1417 | 1526 | 1011 | 467 | 0 | 46.2% | 3356 | 60 | 5245 | 16749 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 233 | 13 | 2 | 17 | 111 | 7 | 0 | 62 | 19 | 1 | 34 |
| nll | 214 | 12 | 2 | 16 | 105 | 7 | 0 | 58 | 1 | 1 | 32 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 17686 | 29280 | just | `just::error::<Error<'_>>::fmt` | 12141 | 6749 | 445 | 6966 | 6966 | constraints 6499, liveness 4370, moves 2515 |
| 14214 | 22100 | just | `just::analyzer::<Analyzer<'run, 'src>>::justfile` | 5092 | 3166 | 163 | 4679 | 4665 | constraints 5547, liveness 4390, moves 1469 |
| 12243 | 17700 | just | `just::subcommand::<Subcommand>::list_module` | 3831 | 2261 | 119 | 3208 | 3205 | constraints 6841, liveness 2382, moves 1003 |
| 11893 | 17698 | just | `just::error::<Error<'src>>::fmt` | 7953 | 7722 | 759 | 11785 | 11785 | constraints 3419, liveness 3139, moves 1548 |
| 10015 | 20749 | just | `just::arguments::<Arguments>::augment_args_for_update` | 5868 | 2927 | 329 | 4284 | 4284 | constraints 4433, liveness 2318, moves 1345 |
| 9974 | 20059 | just | `just::arguments::<Arguments>::augment_args` | 5668 | 2927 | 329 | 4284 | 4284 | constraints 4490, liveness 2255, moves 1294 |
| 9273 | 30661 | just | `just::arguments::<Arguments>::from_arg_matches_mut` | 4791 | 641 | 52 | 392 | 392 | liveness 3709, constraints 3008, moves 1064 |
| 8769 | 13941 | just | `just::compile_error_kind::<CompileErrorKind<'src>>::fmt` | 6167 | 5933 | 586 | 9039 | 9039 | constraints 2747, liveness 2019, moves 1201 |
| 8656 | 19537 | just | `just::compile_error::<CompileError<'_>>::fmt` | 5464 | 4895 | 327 | 5112 | 5112 | constraints 3449, liveness 1747, moves 1029 |
| 8098 | 12999 | just | `just::unresolved_recipe::<Recipe<'src, UnresolvedDependency<'src>>>::resolve` | 3693 | 1770 | 157 | 2452 | 2452 | constraints 3209, liveness 2191, moves 960 |
| 8028 | 18387 | just | `just::recipe::<Recipe<'src, Dependency<'src>>>::run_script` | 3304 | 1576 | 71 | 1815 | 1813 | constraints 3769, liveness 1956, moves 836 |
| 7895 | 32359 | just | `just::arguments::<Arguments>::update_from_arg_matches_mut` | 4417 | 1198 | 154 | 847 | 847 | constraints 3979, liveness 1556, moves 866 |
| 7329 | 9935 | just | `just::justfile::<Justfile<'src>>::run` | 2480 | 1332 | 70 | 1959 | 1959 | constraints 3112, liveness 1475, scopes 1127 |
| 5710 | 8585 | just | `just::parser::<Parser<'run, 'src>>::parse_recipe` | 2280 | 1090 | 73 | 1427 | 1427 | constraints 2999, liveness 1224, moves 529 |
| 5531 | 15170 | just | `just::invocation_parser::<InvocationParser<'src, 'run>>::parse_invocation` | 2109 | 1155 | 39 | 1610 | 1610 | constraints 2984, liveness 1135, moves 509 |
| 5478 | 12282 | just | `just::attribute::<Attribute<'src>>::new_arg` | 2571 | 1475 | 55 | 1754 | 1754 | constraints 2789, liveness 1074, moves 624 |
| 5009 | 9792 | just | `just::expression::<Expression<'_>>::serialize` | 3297 | 513 | 47 | 541 | 541 | constraints 1842, liveness 1522, moves 714 |
| 4841 | 7121 | just | `just::evaluator::<Evaluator<'src, 'run>>::evaluate_builtin_function` | 2757 | 1123 | 37 | 1426 | 1426 | constraints 1957, liveness 1365, moves 599 |
| 4719 | 5377 | just | `just::parser::<Parser<'run, 'src>>::parse_set` | 2657 | 989 | 54 | 1334 | 1334 | constraints 2084, liveness 1172, moves 601 |
| 4579 | 5027 | just | `just::parser::<Parser<'run, 'src>>::parse_item` | 2388 | 1173 | 64 | 1652 | 1652 | constraints 1897, liveness 1127, moves 535 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 16749 | 29280 | just | `just::error::<Error<'_>>::fmt` | 12141 | 6749 | 445 | 6966 | 6966 | constraints 6375, liveness 4325, moves 2460 |
| 13365 | 22100 | just | `just::analyzer::<Analyzer<'run, 'src>>::justfile` | 5092 | 3166 | 163 | 4679 | 4665 | constraints 5351, liveness 4423, moves 1472 |
| 11768 | 17700 | just | `just::subcommand::<Subcommand>::list_module` | 3831 | 2261 | 119 | 3208 | 3205 | constraints 6701, liveness 2382, moves 992 |
| 10589 | 17698 | just | `just::error::<Error<'src>>::fmt` | 7953 | 7722 | 759 | 11785 | 11785 | constraints 3392, liveness 2874, moves 1486 |
| 9584 | 20749 | just | `just::arguments::<Arguments>::augment_args_for_update` | 5868 | 2927 | 329 | 4284 | 4284 | constraints 4447, liveness 2360, moves 1299 |
| 9519 | 20059 | just | `just::arguments::<Arguments>::augment_args` | 5668 | 2927 | 329 | 4284 | 4284 | constraints 4398, liveness 2254, moves 1243 |
| 9122 | 30661 | just | `just::arguments::<Arguments>::from_arg_matches_mut` | 4791 | 641 | 52 | 392 | 392 | liveness 3708, constraints 2976, moves 1010 |
| 8246 | 13941 | just | `just::compile_error_kind::<CompileErrorKind<'src>>::fmt` | 6167 | 5933 | 586 | 9039 | 9039 | constraints 2710, liveness 2142, moves 1171 |
| 7823 | 19537 | just | `just::compile_error::<CompileError<'_>>::fmt` | 5464 | 4895 | 327 | 5112 | 5112 | constraints 3272, liveness 1677, moves 979 |
| 7768 | 32359 | just | `just::arguments::<Arguments>::update_from_arg_matches_mut` | 4417 | 1198 | 154 | 847 | 847 | constraints 3961, liveness 1574, moves 880 |
| 7677 | 12999 | just | `just::unresolved_recipe::<Recipe<'src, UnresolvedDependency<'src>>>::resolve` | 3693 | 1770 | 157 | 2452 | 2452 | constraints 3149, liveness 2178, moves 952 |
| 7648 | 18387 | just | `just::recipe::<Recipe<'src, Dependency<'src>>>::run_script` | 3304 | 1576 | 71 | 1815 | 1813 | constraints 3704, liveness 1877, moves 813 |
| 6147 | 9935 | just | `just::justfile::<Justfile<'src>>::run` | 2480 | 1332 | 70 | 1959 | 1959 | constraints 3058, liveness 1490, moves 685 |
| 5438 | 8585 | just | `just::parser::<Parser<'run, 'src>>::parse_recipe` | 2280 | 1090 | 73 | 1427 | 1427 | constraints 2957, liveness 1216, moves 501 |
| 5245 | 15170 | just | `just::invocation_parser::<InvocationParser<'src, 'run>>::parse_invocation` | 2109 | 1155 | 39 | 1610 | 1610 | constraints 2940, liveness 1062, moves 486 |
| 5190 | 12282 | just | `just::attribute::<Attribute<'src>>::new_arg` | 2571 | 1475 | 55 | 1754 | 1754 | constraints 2724, liveness 1011, moves 614 |
| 4855 | 9792 | just | `just::expression::<Expression<'_>>::serialize` | 3297 | 513 | 47 | 541 | 541 | constraints 1827, liveness 1446, moves 699 |
| 4677 | 7121 | just | `just::evaluator::<Evaluator<'src, 'run>>::evaluate_builtin_function` | 2757 | 1123 | 37 | 1426 | 1426 | constraints 1929, liveness 1366, moves 576 |
| 4641 | 5377 | just | `just::parser::<Parser<'run, 'src>>::parse_set` | 2657 | 989 | 54 | 1334 | 1334 | constraints 2043, liveness 1175, moves 582 |
| 4242 | 5027 | just | `just::parser::<Parser<'run, 'src>>::parse_item` | 2388 | 1173 | 64 | 1652 | 1652 | constraints 1871, liveness 1064, moves 518 |
