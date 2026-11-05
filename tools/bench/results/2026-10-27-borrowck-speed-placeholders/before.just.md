| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| just | polonius | 1417 | 1487 | 1000 | 578 | 0 | 57.8% | 3385 | 73 | 6840 | 17720 |
| just | nll | 1417 | 1487 | 1000 | 528 | 0 | 52.8% | 3385 | 62 | 6571 | 17059 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 228 | 12 | 2 | 16 | 140 | 7 | 0 | 62 | 19 | 1 | 68 |
| nll | 211 | 12 | 2 | 16 | 134 | 7 | 0 | 58 | 1 | 1 | 67 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 17720 | 28721 | just | `just::error::<Error<'_>>::fmt` | 12141 | 6749 | 445 | 6966 | 6966 | constraints 6219, liveness 4566, moves 2499 |
| 15842 | 21844 | just | `just::analyzer::<Analyzer<'run, 'src>>::justfile` | 5092 | 3166 | 163 | 4679 | 4665 | liveness 5571, constraints 5374, moves 1507 |
| 13599 | 17511 | just | `just::subcommand::<Subcommand>::list_module` | 3831 | 2261 | 119 | 3208 | 3205 | constraints 6808, liveness 2917, walk 1436 |
| 12737 | 20516 | just | `just::arguments::<Arguments>::augment_args_for_update` | 5868 | 2927 | 329 | 4284 | 4284 | constraints 4454, walk 2931, liveness 2606 |
| 12237 | 19733 | just | `just::arguments::<Arguments>::augment_args` | 5668 | 2927 | 329 | 4284 | 4284 | constraints 4436, walk 2763, liveness 2384 |
| 11705 | 17406 | just | `just::error::<Error<'src>>::fmt` | 7953 | 7722 | 759 | 11785 | 11785 | constraints 3341, liveness 3052, moves 1534 |
| 9916 | 18501 | just | `just::recipe::<Recipe<'src, Dependency<'src>>>::run_script` | 3304 | 1576 | 71 | 1815 | 1813 | constraints 3676, liveness 2680, walk 1789 |
| 9640 | 30142 | just | `just::arguments::<Arguments>::from_arg_matches_mut` | 4791 | 641 | 52 | 392 | 392 | liveness 3745, constraints 2955, moves 1075 |
| 9084 | 12942 | just | `just::unresolved_recipe::<Recipe<'src, UnresolvedDependency<'src>>>::resolve` | 3693 | 1770 | 157 | 2452 | 2452 | constraints 3172, liveness 3009, moves 963 |
| 8985 | 9910 | just | `just::justfile::<Justfile<'src>>::run` | 2480 | 1332 | 70 | 1959 | 1959 | constraints 3095, liveness 2222, walk 1285 |
| 8819 | 13841 | just | `just::compile_error_kind::<CompileErrorKind<'src>>::fmt` | 6167 | 5933 | 586 | 9039 | 9039 | constraints 2585, liveness 2246, moves 1135 |
| 8348 | 32040 | just | `just::arguments::<Arguments>::update_from_arg_matches_mut` | 4417 | 1198 | 154 | 847 | 847 | constraints 3921, liveness 1613, walk 1026 |
| 8312 | 19217 | just | `just::compile_error::<CompileError<'_>>::fmt` | 5464 | 4895 | 327 | 5112 | 5112 | constraints 3245, liveness 1673, moves 980 |
| 7409 | 7016 | just | `just::evaluator::<Evaluator<'src, 'run>>::evaluate_builtin_function` | 2757 | 1123 | 37 | 1426 | 1426 | liveness 2370, constraints 1951, walk 1949 |
| 6840 | 12082 | just | `just::attribute::<Attribute<'src>>::new_arg` | 2571 | 1475 | 55 | 1754 | 1754 | constraints 2762, liveness 1857, walk 957 |
| 6751 | 8537 | just | `just::parser::<Parser<'run, 'src>>::parse_recipe` | 2280 | 1090 | 73 | 1427 | 1427 | constraints 3023, liveness 1933, walk 597 |
| 6318 | 14766 | just | `just::invocation_parser::<InvocationParser<'src, 'run>>::parse_invocation` | 2109 | 1155 | 39 | 1610 | 1610 | constraints 2952, liveness 1506, walk 740 |
| 6298 | 5407 | just | `just::parser::<Parser<'run, 'src>>::parse_set` | 2657 | 989 | 54 | 1334 | 1334 | liveness 2241, constraints 2090, walk 863 |
| 5728 | 5029 | just | `just::parser::<Parser<'run, 'src>>::parse_item` | 2388 | 1173 | 64 | 1652 | 1652 | liveness 2012, constraints 1910, walk 591 |
| 5555 | 9670 | just | `just::expression::<Expression<'_>>::serialize` | 3297 | 513 | 47 | 541 | 541 | constraints 1782, liveness 1463, walk 986 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 17059 | 28721 | just | `just::error::<Error<'_>>::fmt` | 12141 | 6749 | 445 | 6966 | 6966 | constraints 6165, liveness 4679, moves 2417 |
| 14966 | 21844 | just | `just::analyzer::<Analyzer<'run, 'src>>::justfile` | 5092 | 3166 | 163 | 4679 | 4665 | liveness 5598, constraints 5234, moves 1487 |
| 13102 | 17511 | just | `just::subcommand::<Subcommand>::list_module` | 3831 | 2261 | 119 | 3208 | 3205 | constraints 6713, liveness 2851, walk 1448 |
| 12182 | 20516 | just | `just::arguments::<Arguments>::augment_args_for_update` | 5868 | 2927 | 329 | 4284 | 4284 | constraints 4421, walk 2918, liveness 2507 |
| 11695 | 19733 | just | `just::arguments::<Arguments>::augment_args` | 5668 | 2927 | 329 | 4284 | 4284 | constraints 4379, walk 2771, liveness 2261 |
| 10885 | 17406 | just | `just::error::<Error<'src>>::fmt` | 7953 | 7722 | 759 | 11785 | 11785 | constraints 3285, liveness 3033, moves 1531 |
| 9665 | 18501 | just | `just::recipe::<Recipe<'src, Dependency<'src>>>::run_script` | 3304 | 1576 | 71 | 1815 | 1813 | constraints 3700, liveness 2611, walk 1774 |
| 9584 | 30142 | just | `just::arguments::<Arguments>::from_arg_matches_mut` | 4791 | 641 | 52 | 392 | 392 | liveness 3688, constraints 2912, moves 1067 |
| 8625 | 12942 | just | `just::unresolved_recipe::<Recipe<'src, UnresolvedDependency<'src>>>::resolve` | 3693 | 1770 | 157 | 2452 | 2452 | constraints 3175, liveness 2898, moves 962 |
| 8175 | 32040 | just | `just::arguments::<Arguments>::update_from_arg_matches_mut` | 4417 | 1198 | 154 | 847 | 847 | constraints 3925, liveness 1605, walk 997 |
| 8117 | 19217 | just | `just::compile_error::<CompileError<'_>>::fmt` | 5464 | 4895 | 327 | 5112 | 5112 | constraints 3202, liveness 1803, moves 999 |
| 7980 | 13841 | just | `just::compile_error_kind::<CompileErrorKind<'src>>::fmt` | 6167 | 5933 | 586 | 9039 | 9039 | constraints 2528, liveness 2129, moves 1171 |
| 7687 | 9910 | just | `just::justfile::<Justfile<'src>>::run` | 2480 | 1332 | 70 | 1959 | 1959 | constraints 3097, liveness 2076, walk 1263 |
| 7230 | 7016 | just | `just::evaluator::<Evaluator<'src, 'run>>::evaluate_builtin_function` | 2757 | 1123 | 37 | 1426 | 1426 | liveness 2363, walk 1922, constraints 1920 |
| 6571 | 12082 | just | `just::attribute::<Attribute<'src>>::new_arg` | 2571 | 1475 | 55 | 1754 | 1754 | constraints 2653, liveness 1840, walk 928 |
| 6408 | 8537 | just | `just::parser::<Parser<'run, 'src>>::parse_recipe` | 2280 | 1090 | 73 | 1427 | 1427 | constraints 2967, liveness 1898, walk 595 |
| 6096 | 5407 | just | `just::parser::<Parser<'run, 'src>>::parse_set` | 2657 | 989 | 54 | 1334 | 1334 | liveness 2216, constraints 2019, walk 846 |
| 6013 | 14766 | just | `just::invocation_parser::<InvocationParser<'src, 'run>>::parse_invocation` | 2109 | 1155 | 39 | 1610 | 1610 | constraints 2922, liveness 1422, walk 720 |
| 5498 | 9670 | just | `just::expression::<Expression<'_>>::serialize` | 3297 | 513 | 47 | 541 | 541 | constraints 1730, liveness 1559, walk 987 |
| 5462 | 5029 | just | `just::parser::<Parser<'run, 'src>>::parse_item` | 2388 | 1173 | 64 | 1652 | 1652 | liveness 1962, constraints 1881, walk 599 |
