| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| just | polonius | 1417 | 1563 | 1058 | 704 | 0 | 66.5% | 3470 | 93 | 8595 | 19471 |
| just | nll | 1417 | 1563 | 1058 | 652 | 0 | 61.6% | 3470 | 80 | 8044 | 18647 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 301 | 13 | 3 | 17 | 147 | 8 | 0 | 102 | 20 | 1 | 70 |
| nll | 285 | 12 | 2 | 17 | 141 | 7 | 0 | 97 | 2 | 1 | 68 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 19471 | 30396 | just | `just::error::<Error<'_>>::fmt` | 12141 | 6749 | 445 | 6966 | 6966 | constraints 7487, liveness 4491, moves 3025 |
| 19467 | 22953 | just | `just::analyzer::<Analyzer<'run, 'src>>::justfile` | 5092 | 3166 | 163 | 4679 | 4665 | constraints 7786, liveness 5811, moves 2373 |
| 14513 | 18543 | just | `just::subcommand::<Subcommand>::list_module` | 3831 | 2261 | 119 | 3208 | 3205 | constraints 7186, liveness 2925, moves 1529 |
| 13080 | 21975 | just | `just::arguments::<Arguments>::augment_args_for_update` | 5868 | 2927 | 329 | 4284 | 4284 | constraints 4604, walk 3026, liveness 2544 |
| 12943 | 19530 | just | `just::recipe::<Recipe<'src, Dependency<'src>>>::run_script` | 3304 | 1576 | 71 | 1815 | 1813 | constraints 5600, liveness 2818, walk 1855 |
| 12611 | 21238 | just | `just::arguments::<Arguments>::augment_args` | 5668 | 2927 | 329 | 4284 | 4284 | constraints 4617, walk 2862, liveness 2323 |
| 11960 | 18602 | just | `just::error::<Error<'src>>::fmt` | 7953 | 7722 | 759 | 11785 | 11785 | constraints 3416, liveness 3144, moves 1536 |
| 11089 | 13525 | just | `just::unresolved_recipe::<Recipe<'src, UnresolvedDependency<'src>>>::resolve` | 3693 | 1770 | 157 | 2452 | 2452 | constraints 4673, liveness 2995, moves 1418 |
| 10658 | 31928 | just | `just::arguments::<Arguments>::from_arg_matches_mut` | 4791 | 641 | 52 | 392 | 392 | liveness 3885, constraints 3510, moves 1336 |
| 10166 | 10277 | just | `just::justfile::<Justfile<'src>>::run` | 2480 | 1332 | 70 | 1959 | 1959 | constraints 3814, liveness 2216, walk 1282 |
| 9808 | 20480 | just | `just::compile_error::<CompileError<'_>>::fmt` | 5464 | 4895 | 327 | 5112 | 5112 | constraints 4030, liveness 1713, moves 1500 |
| 9115 | 33830 | just | `just::arguments::<Arguments>::update_from_arg_matches_mut` | 4417 | 1198 | 154 | 847 | 847 | constraints 4542, liveness 1642, moves 1167 |
| 8918 | 12722 | just | `just::attribute::<Attribute<'src>>::new_arg` | 2571 | 1475 | 55 | 1754 | 1754 | constraints 3975, liveness 1961, moves 1269 |
| 8728 | 14708 | just | `just::compile_error_kind::<CompileErrorKind<'src>>::fmt` | 6167 | 5933 | 586 | 9039 | 9039 | constraints 2645, liveness 2148, moves 1140 |
| 8595 | 7422 | just | `just::evaluator::<Evaluator<'src, 'run>>::evaluate_builtin_function` | 2757 | 1123 | 37 | 1426 | 1426 | constraints 2669, liveness 2407, walk 2024 |
| 7890 | 15801 | just | `just::invocation_parser::<InvocationParser<'src, 'run>>::parse_invocation` | 2109 | 1155 | 39 | 1610 | 1610 | constraints 4043, liveness 1474, moves 937 |
| 7424 | 8946 | just | `just::parser::<Parser<'run, 'src>>::parse_recipe` | 2280 | 1090 | 73 | 1427 | 1427 | constraints 3261, liveness 1986, moves 875 |
| 7245 | 5654 | just | `just::parser::<Parser<'run, 'src>>::parse_set` | 2657 | 989 | 54 | 1334 | 1334 | constraints 2739, liveness 2226, walk 947 |
| 6900 | 5330 | just | `just::parser::<Parser<'run, 'src>>::parse_item` | 2388 | 1173 | 64 | 1652 | 1652 | constraints 2655, liveness 2182, moves 757 |
| 6552 | 10114 | just | `just::expression::<Expression<'_>>::serialize` | 3297 | 513 | 47 | 541 | 541 | constraints 2603, liveness 1528, walk 975 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 18647 | 22953 | just | `just::analyzer::<Analyzer<'run, 'src>>::justfile` | 5092 | 3166 | 163 | 4679 | 4665 | constraints 7845, liveness 5647, moves 2340 |
| 18570 | 30396 | just | `just::error::<Error<'_>>::fmt` | 12141 | 6749 | 445 | 6966 | 6966 | constraints 7391, liveness 4293, moves 3059 |
| 14255 | 18543 | just | `just::subcommand::<Subcommand>::list_module` | 3831 | 2261 | 119 | 3208 | 3205 | constraints 7109, liveness 2982, moves 1577 |
| 12632 | 19530 | just | `just::recipe::<Recipe<'src, Dependency<'src>>>::run_script` | 3304 | 1576 | 71 | 1815 | 1813 | constraints 5596, liveness 2759, walk 1804 |
| 12470 | 21975 | just | `just::arguments::<Arguments>::augment_args_for_update` | 5868 | 2927 | 329 | 4284 | 4284 | constraints 4602, walk 3009, liveness 2456 |
| 12166 | 21238 | just | `just::arguments::<Arguments>::augment_args` | 5668 | 2927 | 329 | 4284 | 4284 | constraints 4577, walk 2822, liveness 2331 |
| 11384 | 18602 | just | `just::error::<Error<'src>>::fmt` | 7953 | 7722 | 759 | 11785 | 11785 | constraints 3422, liveness 3275, moves 1540 |
| 10715 | 13525 | just | `just::unresolved_recipe::<Recipe<'src, UnresolvedDependency<'src>>>::resolve` | 3693 | 1770 | 157 | 2452 | 2452 | constraints 4660, liveness 2963, moves 1425 |
| 10460 | 31928 | just | `just::arguments::<Arguments>::from_arg_matches_mut` | 4791 | 641 | 52 | 392 | 392 | liveness 3771, constraints 3538, moves 1287 |
| 9320 | 20480 | just | `just::compile_error::<CompileError<'_>>::fmt` | 5464 | 4895 | 327 | 5112 | 5112 | constraints 3905, liveness 1810, moves 1497 |
| 8875 | 10277 | just | `just::justfile::<Justfile<'src>>::run` | 2480 | 1332 | 70 | 1959 | 1959 | constraints 3768, liveness 2137, walk 1255 |
| 8823 | 33830 | just | `just::arguments::<Arguments>::update_from_arg_matches_mut` | 4417 | 1198 | 154 | 847 | 847 | constraints 4501, liveness 1586, moves 1178 |
| 8709 | 12722 | just | `just::attribute::<Attribute<'src>>::new_arg` | 2571 | 1475 | 55 | 1754 | 1754 | constraints 3927, liveness 1934, moves 1294 |
| 8468 | 7422 | just | `just::evaluator::<Evaluator<'src, 'run>>::evaluate_builtin_function` | 2757 | 1123 | 37 | 1426 | 1426 | constraints 2662, liveness 2400, walk 2062 |
| 8044 | 14708 | just | `just::compile_error_kind::<CompileErrorKind<'src>>::fmt` | 6167 | 5933 | 586 | 9039 | 9039 | constraints 2620, liveness 2118, moves 1144 |
| 7628 | 15801 | just | `just::invocation_parser::<InvocationParser<'src, 'run>>::parse_invocation` | 2109 | 1155 | 39 | 1610 | 1610 | constraints 3996, liveness 1435, moves 925 |
| 7326 | 8946 | just | `just::parser::<Parser<'run, 'src>>::parse_recipe` | 2280 | 1090 | 73 | 1427 | 1427 | constraints 3272, liveness 1957, moves 996 |
| 7108 | 5654 | just | `just::parser::<Parser<'run, 'src>>::parse_set` | 2657 | 989 | 54 | 1334 | 1334 | constraints 2739, liveness 2233, walk 913 |
| 6583 | 5330 | just | `just::parser::<Parser<'run, 'src>>::parse_item` | 2388 | 1173 | 64 | 1652 | 1652 | constraints 2661, liveness 2055, moves 757 |
| 6483 | 10114 | just | `just::expression::<Expression<'_>>::serialize` | 3297 | 513 | 47 | 541 | 541 | constraints 2618, liveness 1581, walk 979 |
