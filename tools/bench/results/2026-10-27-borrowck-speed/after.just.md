| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| just | polonius | 1417 | 1660 | 1069 | 715 | 0 | 66.9% | 3594 | 94 | 8984 | 20569 |
| just | nll | 1417 | 1660 | 1069 | 659 | 0 | 61.7% | 3594 | 79 | 8444 | 19366 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 306 | 12 | 3 | 17 | 149 | 8 | 0 | 104 | 20 | 1 | 73 |
| nll | 287 | 13 | 3 | 17 | 142 | 8 | 0 | 98 | 1 | 1 | 71 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 20569 | 23205 | just | `just::analyzer::<Analyzer<'run, 'src>>::justfile` | 5092 | 3166 | 163 | 4679 | 4665 | constraints 8472, liveness 6009, moves 2451 |
| 20344 | 31743 | just | `just::error::<Error<'_>>::fmt` | 12141 | 6749 | 445 | 6966 | 6966 | constraints 7686, liveness 4742, moves 3308 |
| 14920 | 18942 | just | `just::subcommand::<Subcommand>::list_module` | 3831 | 2261 | 119 | 3208 | 3205 | constraints 7406, liveness 3002, moves 1587 |
| 13266 | 22479 | just | `just::arguments::<Arguments>::augment_args_for_update` | 5868 | 2927 | 329 | 4284 | 4284 | constraints 4669, walk 3164, liveness 2581 |
| 13109 | 19824 | just | `just::recipe::<Recipe<'src, Dependency<'src>>>::run_script` | 3304 | 1576 | 71 | 1815 | 1813 | constraints 5742, liveness 2799, walk 1916 |
| 12904 | 21709 | just | `just::arguments::<Arguments>::augment_args` | 5668 | 2927 | 329 | 4284 | 4284 | constraints 4686, walk 2950, liveness 2387 |
| 12060 | 18823 | just | `just::error::<Error<'src>>::fmt` | 7953 | 7722 | 759 | 11785 | 11785 | constraints 3648, liveness 3046, moves 1555 |
| 11544 | 13857 | just | `just::unresolved_recipe::<Recipe<'src, UnresolvedDependency<'src>>>::resolve` | 3693 | 1770 | 157 | 2452 | 2452 | constraints 4851, liveness 3123, moves 1489 |
| 10577 | 31932 | just | `just::arguments::<Arguments>::from_arg_matches_mut` | 4791 | 641 | 52 | 392 | 392 | liveness 3804, constraints 3569, moves 1352 |
| 10411 | 10671 | just | `just::justfile::<Justfile<'src>>::run` | 2480 | 1332 | 70 | 1959 | 1959 | constraints 3901, liveness 2237, walk 1359 |
| 10161 | 20801 | just | `just::compile_error::<CompileError<'_>>::fmt` | 5464 | 4895 | 327 | 5112 | 5112 | constraints 4172, liveness 1896, moves 1505 |
| 9192 | 33909 | just | `just::arguments::<Arguments>::update_from_arg_matches_mut` | 4417 | 1198 | 154 | 847 | 847 | constraints 4587, liveness 1653, moves 1198 |
| 9161 | 12832 | just | `just::attribute::<Attribute<'src>>::new_arg` | 2571 | 1475 | 55 | 1754 | 1754 | constraints 4038, liveness 2035, moves 1287 |
| 9120 | 14746 | just | `just::compile_error_kind::<CompileErrorKind<'src>>::fmt` | 6167 | 5933 | 586 | 9039 | 9039 | constraints 2711, liveness 2270, moves 1209 |
| 8984 | 7510 | just | `just::evaluator::<Evaluator<'src, 'run>>::evaluate_builtin_function` | 2757 | 1123 | 37 | 1426 | 1426 | constraints 2713, liveness 2428, walk 2404 |
| 8067 | 16101 | just | `just::invocation_parser::<InvocationParser<'src, 'run>>::parse_invocation` | 2109 | 1155 | 39 | 1610 | 1610 | constraints 4115, liveness 1553, moves 965 |
| 7629 | 9397 | just | `just::parser::<Parser<'run, 'src>>::parse_recipe` | 2280 | 1090 | 73 | 1427 | 1427 | constraints 3416, liveness 1958, moves 885 |
| 7483 | 5914 | just | `just::parser::<Parser<'run, 'src>>::parse_set` | 2657 | 989 | 54 | 1334 | 1334 | constraints 2877, liveness 2298, walk 986 |
| 7212 | 5426 | just | `just::parser::<Parser<'run, 'src>>::parse_item` | 2388 | 1173 | 64 | 1652 | 1652 | constraints 2859, liveness 2192, moves 810 |
| 6971 | 8640 | just | `just::justfile::<Justfile<'src>>::run_dependencies` | 1319 | 871 | 34 | 1167 | 1165 | constraints 3462, liveness 1425, moves 915 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 19366 | 23205 | just | `just::analyzer::<Analyzer<'run, 'src>>::justfile` | 5092 | 3166 | 163 | 4679 | 4665 | constraints 8192, liveness 5806, moves 2445 |
| 18977 | 31743 | just | `just::error::<Error<'_>>::fmt` | 12141 | 6749 | 445 | 6966 | 6966 | constraints 7536, liveness 4308, moves 3074 |
| 14478 | 18942 | just | `just::subcommand::<Subcommand>::list_module` | 3831 | 2261 | 119 | 3208 | 3205 | constraints 7291, liveness 2996, moves 1549 |
| 12846 | 19824 | just | `just::recipe::<Recipe<'src, Dependency<'src>>>::run_script` | 3304 | 1576 | 71 | 1815 | 1813 | constraints 5637, liveness 2826, walk 1915 |
| 12744 | 22479 | just | `just::arguments::<Arguments>::augment_args_for_update` | 5868 | 2927 | 329 | 4284 | 4284 | constraints 4640, walk 3093, liveness 2441 |
| 12551 | 21709 | just | `just::arguments::<Arguments>::augment_args` | 5668 | 2927 | 329 | 4284 | 4284 | constraints 4708, walk 2929, liveness 2400 |
| 11261 | 18823 | just | `just::error::<Error<'src>>::fmt` | 7953 | 7722 | 759 | 11785 | 11785 | constraints 3536, liveness 3073, moves 1548 |
| 10935 | 13857 | just | `just::unresolved_recipe::<Recipe<'src, UnresolvedDependency<'src>>>::resolve` | 3693 | 1770 | 157 | 2452 | 2452 | constraints 4751, liveness 2989, moves 1450 |
| 10588 | 31932 | just | `just::arguments::<Arguments>::from_arg_matches_mut` | 4791 | 641 | 52 | 392 | 392 | liveness 3821, constraints 3589, moves 1291 |
| 9231 | 20801 | just | `just::compile_error::<CompileError<'_>>::fmt` | 5464 | 4895 | 327 | 5112 | 5112 | constraints 3987, liveness 1726, moves 1454 |
| 9127 | 10671 | just | `just::justfile::<Justfile<'src>>::run` | 2480 | 1332 | 70 | 1959 | 1959 | constraints 3835, liveness 2181, walk 1348 |
| 8956 | 33909 | just | `just::arguments::<Arguments>::update_from_arg_matches_mut` | 4417 | 1198 | 154 | 847 | 847 | constraints 4577, liveness 1606, moves 1187 |
| 8849 | 12832 | just | `just::attribute::<Attribute<'src>>::new_arg` | 2571 | 1475 | 55 | 1754 | 1754 | constraints 4004, liveness 1974, moves 1251 |
| 8610 | 7510 | just | `just::evaluator::<Evaluator<'src, 'run>>::evaluate_builtin_function` | 2757 | 1123 | 37 | 1426 | 1426 | constraints 2673, liveness 2417, walk 2177 |
| 8444 | 14746 | just | `just::compile_error_kind::<CompileErrorKind<'src>>::fmt` | 6167 | 5933 | 586 | 9039 | 9039 | constraints 2691, liveness 2219, moves 1186 |
| 7755 | 16101 | just | `just::invocation_parser::<InvocationParser<'src, 'run>>::parse_invocation` | 2109 | 1155 | 39 | 1610 | 1610 | constraints 4031, liveness 1485, moves 958 |
| 7279 | 9397 | just | `just::parser::<Parser<'run, 'src>>::parse_recipe` | 2280 | 1090 | 73 | 1427 | 1427 | constraints 3340, liveness 1946, moves 871 |
| 7214 | 5914 | just | `just::parser::<Parser<'run, 'src>>::parse_set` | 2657 | 989 | 54 | 1334 | 1334 | constraints 2725, liveness 2282, walk 948 |
| 6797 | 5426 | just | `just::parser::<Parser<'run, 'src>>::parse_item` | 2388 | 1173 | 64 | 1652 | 1652 | constraints 2794, liveness 2119, moves 729 |
| 6475 | 10271 | just | `just::expression::<Expression<'_>>::serialize` | 3297 | 513 | 47 | 541 | 541 | constraints 2596, liveness 1481, walk 1019 |
