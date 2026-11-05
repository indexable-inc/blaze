| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| just | polonius | 1417 | 1520 | 998 | 519 | 0 | 52.0% | 3366 | 72 | 5449 | 17445 |
| just | nll | 1417 | 1520 | 998 | 467 | 0 | 46.8% | 3366 | 60 | 5230 | 16616 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 231 | 12 | 2 | 17 | 112 | 7 | 0 | 62 | 19 | 1 | 34 |
| nll | 213 | 12 | 2 | 16 | 107 | 7 | 0 | 58 | 1 | 1 | 32 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 17445 | 29108 | just | `just::error::<Error<'_>>::fmt` | 12141 | 6749 | 445 | 6966 | 6966 | constraints 6282, liveness 4386, moves 2515 |
| 14348 | 21842 | just | `just::analyzer::<Analyzer<'run, 'src>>::justfile` | 5092 | 3166 | 163 | 4679 | 4665 | constraints 5508, liveness 4537, moves 1516 |
| 12409 | 17644 | just | `just::subcommand::<Subcommand>::list_module` | 3831 | 2261 | 119 | 3208 | 3205 | constraints 6784, liveness 2434, moves 1008 |
| 11683 | 17457 | just | `just::error::<Error<'src>>::fmt` | 7953 | 7722 | 759 | 11785 | 11785 | constraints 3331, liveness 3108, moves 1519 |
| 10141 | 20575 | just | `just::arguments::<Arguments>::augment_args_for_update` | 5868 | 2927 | 329 | 4284 | 4284 | constraints 4444, liveness 2369, moves 1342 |
| 9888 | 19957 | just | `just::arguments::<Arguments>::augment_args` | 5668 | 2927 | 329 | 4284 | 4284 | constraints 4372, liveness 2373, moves 1258 |
| 9672 | 30398 | just | `just::arguments::<Arguments>::from_arg_matches_mut` | 4791 | 641 | 52 | 392 | 392 | liveness 4109, constraints 2999, moves 1048 |
| 8921 | 13793 | just | `just::compile_error_kind::<CompileErrorKind<'src>>::fmt` | 6167 | 5933 | 586 | 9039 | 9039 | constraints 2743, liveness 2179, moves 1169 |
| 8581 | 19141 | just | `just::compile_error::<CompileError<'_>>::fmt` | 5464 | 4895 | 327 | 5112 | 5112 | constraints 3314, liveness 1751, moves 1003 |
| 8167 | 12731 | just | `just::unresolved_recipe::<Recipe<'src, UnresolvedDependency<'src>>>::resolve` | 3693 | 1770 | 157 | 2452 | 2452 | constraints 3192, liveness 2248, moves 981 |
| 7980 | 32042 | just | `just::arguments::<Arguments>::update_from_arg_matches_mut` | 4417 | 1198 | 154 | 847 | 847 | constraints 3985, liveness 1622, moves 903 |
| 7957 | 18525 | just | `just::recipe::<Recipe<'src, Dependency<'src>>>::run_script` | 3304 | 1576 | 71 | 1815 | 1813 | constraints 3728, liveness 1926, moves 861 |
| 7341 | 9745 | just | `just::justfile::<Justfile<'src>>::run` | 2480 | 1332 | 70 | 1959 | 1959 | constraints 3068, liveness 1535, scopes 1123 |
| 5713 | 8410 | just | `just::parser::<Parser<'run, 'src>>::parse_recipe` | 2280 | 1090 | 73 | 1427 | 1427 | constraints 2997, liveness 1260, moves 517 |
| 5449 | 12026 | just | `just::attribute::<Attribute<'src>>::new_arg` | 2571 | 1475 | 55 | 1754 | 1754 | constraints 2796, liveness 1035, moves 633 |
| 5444 | 14938 | just | `just::invocation_parser::<InvocationParser<'src, 'run>>::parse_invocation` | 2109 | 1155 | 39 | 1610 | 1610 | constraints 2961, liveness 1076, moves 498 |
| 4924 | 9639 | just | `just::expression::<Expression<'_>>::serialize` | 3297 | 513 | 47 | 541 | 541 | constraints 1793, liveness 1541, moves 724 |
| 4833 | 7010 | just | `just::evaluator::<Evaluator<'src, 'run>>::evaluate_builtin_function` | 2757 | 1123 | 37 | 1426 | 1426 | constraints 1920, liveness 1388, moves 594 |
| 4780 | 5380 | just | `just::parser::<Parser<'run, 'src>>::parse_set` | 2657 | 989 | 54 | 1334 | 1334 | constraints 2088, liveness 1242, moves 592 |
| 4564 | 5004 | just | `just::parser::<Parser<'run, 'src>>::parse_item` | 2388 | 1173 | 64 | 1652 | 1652 | constraints 1907, liveness 1125, moves 545 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 16616 | 29108 | just | `just::error::<Error<'_>>::fmt` | 12141 | 6749 | 445 | 6966 | 6966 | constraints 6163, liveness 4418, moves 2448 |
| 13583 | 21842 | just | `just::analyzer::<Analyzer<'run, 'src>>::justfile` | 5092 | 3166 | 163 | 4679 | 4665 | constraints 5386, liveness 4594, moves 1484 |
| 11832 | 17644 | just | `just::subcommand::<Subcommand>::list_module` | 3831 | 2261 | 119 | 3208 | 3205 | constraints 6729, liveness 2401, moves 993 |
| 11052 | 17457 | just | `just::error::<Error<'src>>::fmt` | 7953 | 7722 | 759 | 11785 | 11785 | constraints 3316, liveness 3111, moves 1532 |
| 9762 | 20575 | just | `just::arguments::<Arguments>::augment_args_for_update` | 5868 | 2927 | 329 | 4284 | 4284 | constraints 4400, liveness 2347, moves 1372 |
| 9523 | 30398 | just | `just::arguments::<Arguments>::from_arg_matches_mut` | 4791 | 641 | 52 | 392 | 392 | liveness 4058, constraints 2987, moves 1055 |
| 9461 | 19957 | just | `just::arguments::<Arguments>::augment_args` | 5668 | 2927 | 329 | 4284 | 4284 | constraints 4404, liveness 2233, moves 1311 |
| 8210 | 13793 | just | `just::compile_error_kind::<CompileErrorKind<'src>>::fmt` | 6167 | 5933 | 586 | 9039 | 9039 | constraints 2714, liveness 2170, moves 1198 |
| 7806 | 19141 | just | `just::compile_error::<CompileError<'_>>::fmt` | 5464 | 4895 | 327 | 5112 | 5112 | constraints 3189, liveness 1656, moves 966 |
| 7761 | 32042 | just | `just::arguments::<Arguments>::update_from_arg_matches_mut` | 4417 | 1198 | 154 | 847 | 847 | constraints 3951, liveness 1590, moves 876 |
| 7714 | 18525 | just | `just::recipe::<Recipe<'src, Dependency<'src>>>::run_script` | 3304 | 1576 | 71 | 1815 | 1813 | constraints 3675, liveness 1941, moves 838 |
| 7682 | 12731 | just | `just::unresolved_recipe::<Recipe<'src, UnresolvedDependency<'src>>>::resolve` | 3693 | 1770 | 157 | 2452 | 2452 | constraints 3090, liveness 2247, moves 950 |
| 6096 | 9745 | just | `just::justfile::<Justfile<'src>>::run` | 2480 | 1332 | 70 | 1959 | 1959 | constraints 3026, liveness 1494, moves 691 |
| 5453 | 8410 | just | `just::parser::<Parser<'run, 'src>>::parse_recipe` | 2280 | 1090 | 73 | 1427 | 1427 | constraints 2986, liveness 1220, moves 499 |
| 5230 | 14938 | just | `just::invocation_parser::<InvocationParser<'src, 'run>>::parse_invocation` | 2109 | 1155 | 39 | 1610 | 1610 | constraints 2904, liveness 1067, moves 507 |
| 5174 | 12026 | just | `just::attribute::<Attribute<'src>>::new_arg` | 2571 | 1475 | 55 | 1754 | 1754 | constraints 2692, liveness 1039, moves 606 |
| 4800 | 9639 | just | `just::expression::<Expression<'_>>::serialize` | 3297 | 513 | 47 | 541 | 541 | constraints 1797, liveness 1453, moves 675 |
| 4654 | 7010 | just | `just::evaluator::<Evaluator<'src, 'run>>::evaluate_builtin_function` | 2757 | 1123 | 37 | 1426 | 1426 | constraints 1900, liveness 1362, moves 583 |
| 4602 | 5380 | just | `just::parser::<Parser<'run, 'src>>::parse_set` | 2657 | 989 | 54 | 1334 | 1334 | constraints 2055, liveness 1230, moves 578 |
| 4330 | 5004 | just | `just::parser::<Parser<'run, 'src>>::parse_item` | 2388 | 1173 | 64 | 1652 | 1652 | constraints 1870, liveness 1070, moves 572 |
