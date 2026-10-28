| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| just | polonius | 1417 | 1892 | 1163 | 846 | 0 | 72.7% | 3833 | 108 | 10454 | 27804 |
| just | nll | 1417 | 1892 | 1163 | 782 | 0 | 67.2% | 3833 | 93 | 10018 | 26239 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 418 | 13 | 3 | 18 | 157 | 8 | 0 | 110 | 21 | 2 | 73 |
| nll | 395 | 13 | 3 | 17 | 149 | 8 | 0 | 103 | 1 | 1 | 71 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 27804 | 39403 | just | `just::error::<Error<'_>>::fmt` | 12141 | 6749 | 445 | 6966 | 6966 | constraints 13214, liveness 5375, moves 3898 |
| 23298 | 25287 | just | `just::analyzer::<Analyzer<'run, 'src>>::justfile` | 5092 | 3166 | 163 | 4679 | 4665 | constraints 10218, liveness 6497, moves 2646 |
| 15914 | 20579 | just | `just::subcommand::<Subcommand>::list_module` | 3831 | 2261 | 119 | 3208 | 3205 | constraints 8106, liveness 3188, moves 1688 |
| 15668 | 22324 | just | `just::recipe::<Recipe<'src, Dependency<'src>>>::run_script` | 3304 | 1575 | 71 | 1815 | 1813 | constraints 7560, liveness 3268, walk 1912 |
| 15630 | 26042 | just | `just::arguments::<Arguments>::augment_args_for_update` | 5868 | 2927 | 329 | 4284 | 4284 | constraints 6637, walk 3111, liveness 2624 |
| 15445 | 21964 | just | `just::error::<Error<'src>>::fmt` | 7953 | 7722 | 759 | 11785 | 11785 | constraints 6243, liveness 3361, moves 1789 |
| 15227 | 23859 | just | `just::arguments::<Arguments>::augment_args` | 5668 | 2927 | 329 | 4284 | 4284 | constraints 6612, walk 2924, liveness 2460 |
| 12580 | 15458 | just | `just::unresolved_recipe::<Recipe<'src, UnresolvedDependency<'src>>>::resolve` | 3693 | 1770 | 157 | 2452 | 2452 | constraints 5802, liveness 3131, moves 1555 |
| 12027 | 22333 | just | `just::compile_error::<CompileError<'_>>::fmt` | 5464 | 4895 | 327 | 5112 | 5112 | constraints 5790, liveness 1968, moves 1639 |
| 11958 | 34758 | just | `just::arguments::<Arguments>::from_arg_matches_mut` | 4791 | 641 | 52 | 392 | 392 | constraints 4646, liveness 4032, moves 1387 |
| 11760 | 12466 | just | `just::justfile::<Justfile<'src>>::run` | 2480 | 1332 | 70 | 1959 | 1959 | constraints 4903, liveness 2466, walk 1354 |
| 11414 | 15516 | just | `just::attribute::<Attribute<'src>>::new_arg` | 2571 | 1475 | 55 | 1754 | 1754 | constraints 5842, liveness 2218, moves 1505 |
| 10586 | 15440 | just | `just::compile_error_kind::<CompileErrorKind<'src>>::fmt` | 6167 | 5933 | 586 | 9039 | 9039 | constraints 3968, liveness 2405, moves 1282 |
| 10522 | 20244 | just | `just::invocation_parser::<InvocationParser<'src, 'run>>::parse_invocation` | 2109 | 1155 | 39 | 1610 | 1610 | constraints 5921, liveness 1750, moves 1187 |
| 10454 | 36313 | just | `just::arguments::<Arguments>::update_from_arg_matches_mut` | 4417 | 1198 | 154 | 847 | 847 | constraints 5672, liveness 1711, moves 1233 |
| 10090 | 8491 | just | `just::evaluator::<Evaluator<'src, 'run>>::evaluate_builtin_function` | 2757 | 1123 | 37 | 1426 | 1426 | constraints 3667, liveness 2735, walk 2122 |
| 9159 | 11827 | just | `just::expression::<Expression<'_>>::serialize` | 3297 | 513 | 47 | 541 | 541 | constraints 4761, liveness 1637, walk 1126 |
| 8742 | 9193 | just | `just::justfile::<Justfile<'src>>::run_dependencies` | 1319 | 869 | 34 | 1167 | 1165 | constraints 5346, liveness 1429, moves 850 |
| 8216 | 6107 | just | `just::parser::<Parser<'run, 'src>>::parse_set` | 2657 | 989 | 54 | 1334 | 1334 | constraints 3448, liveness 2400, walk 940 |
| 8070 | 9853 | just | `just::parser::<Parser<'run, 'src>>::parse_recipe` | 2280 | 1090 | 73 | 1427 | 1427 | constraints 3691, liveness 2014, moves 989 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 26239 | 39403 | just | `just::error::<Error<'_>>::fmt` | 12141 | 6749 | 445 | 6966 | 6966 | constraints 13082, liveness 4983, moves 3749 |
| 22622 | 25287 | just | `just::analyzer::<Analyzer<'run, 'src>>::justfile` | 5092 | 3166 | 163 | 4679 | 4665 | constraints 10845, liveness 6160, moves 2632 |
| 15585 | 22324 | just | `just::recipe::<Recipe<'src, Dependency<'src>>>::run_script` | 3304 | 1575 | 71 | 1815 | 1813 | constraints 7820, liveness 3130, moves 1925 |
| 15116 | 20579 | just | `just::subcommand::<Subcommand>::list_module` | 3831 | 2261 | 119 | 3208 | 3205 | constraints 7901, liveness 3004, moves 1604 |
| 14757 | 23859 | just | `just::arguments::<Arguments>::augment_args` | 5668 | 2927 | 329 | 4284 | 4284 | constraints 6460, walk 2935, liveness 2634 |
| 14326 | 21964 | just | `just::error::<Error<'src>>::fmt` | 7953 | 7722 | 759 | 11785 | 11785 | constraints 6086, liveness 3305, moves 1723 |
| 14195 | 26042 | just | `just::arguments::<Arguments>::augment_args_for_update` | 5868 | 2927 | 329 | 4284 | 4284 | constraints 6015, walk 3089, liveness 2425 |
| 11925 | 15458 | just | `just::unresolved_recipe::<Recipe<'src, UnresolvedDependency<'src>>>::resolve` | 3693 | 1770 | 157 | 2452 | 2452 | constraints 5658, liveness 3058, moves 1490 |
| 11608 | 34758 | just | `just::arguments::<Arguments>::from_arg_matches_mut` | 4791 | 641 | 52 | 392 | 392 | constraints 4568, liveness 3893, moves 1342 |
| 11584 | 12466 | just | `just::justfile::<Justfile<'src>>::run` | 2480 | 1332 | 70 | 1959 | 1959 | constraints 5571, liveness 2664, walk 1385 |
| 11078 | 15516 | just | `just::attribute::<Attribute<'src>>::new_arg` | 2571 | 1475 | 55 | 1754 | 1754 | constraints 5734, liveness 2170, moves 1489 |
| 10947 | 22333 | just | `just::compile_error::<CompileError<'_>>::fmt` | 5464 | 4895 | 327 | 5112 | 5112 | constraints 5560, liveness 1704, moves 1602 |
| 10364 | 20244 | just | `just::invocation_parser::<InvocationParser<'src, 'run>>::parse_invocation` | 2109 | 1155 | 39 | 1610 | 1610 | constraints 6058, liveness 1746, moves 1144 |
| 10346 | 36313 | just | `just::arguments::<Arguments>::update_from_arg_matches_mut` | 4417 | 1198 | 154 | 847 | 847 | constraints 5730, liveness 1754, moves 1228 |
| 10018 | 8491 | just | `just::evaluator::<Evaluator<'src, 'run>>::evaluate_builtin_function` | 2757 | 1123 | 37 | 1426 | 1426 | constraints 3625, liveness 2864, walk 2133 |
| 9217 | 15440 | just | `just::compile_error_kind::<CompileErrorKind<'src>>::fmt` | 6167 | 5933 | 586 | 9039 | 9039 | constraints 3430, liveness 2233, moves 1221 |
| 9135 | 11827 | just | `just::expression::<Expression<'_>>::serialize` | 3297 | 513 | 47 | 541 | 541 | constraints 4777, liveness 1689, walk 1089 |
| 8336 | 9193 | just | `just::justfile::<Justfile<'src>>::run_dependencies` | 1319 | 869 | 34 | 1167 | 1165 | constraints 5189, liveness 1337, moves 787 |
| 8053 | 6107 | just | `just::parser::<Parser<'run, 'src>>::parse_set` | 2657 | 989 | 54 | 1334 | 1334 | constraints 3333, liveness 2430, walk 930 |
| 7738 | 9853 | just | `just::parser::<Parser<'run, 'src>>::parse_recipe` | 2280 | 1090 | 73 | 1427 | 1427 | constraints 3605, liveness 2055, moves 931 |
