| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| just | polonius | 1417 | 1460 | 984 | 573 | 0 | 58.2% | 3370 | 72 | 6827 | 17675 |
| just | nll | 1417 | 1460 | 984 | 524 | 0 | 53.3% | 3370 | 62 | 6560 | 17039 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 226 | 13 | 2 | 17 | 139 | 7 | 0 | 62 | 19 | 1 | 68 |
| nll | 210 | 12 | 2 | 16 | 134 | 7 | 0 | 58 | 1 | 1 | 66 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 17675 | 28715 | just | `just::error::<Error<'_>>::fmt` | 12141 | 6749 | 445 | 6966 | 6966 | constraints 6230, liveness 4461, moves 2520 |
| 15803 | 21436 | just | `just::analyzer::<Analyzer<'run, 'src>>::justfile` | 5092 | 3166 | 163 | 4679 | 4665 | liveness 5595, constraints 5383, moves 1444 |
| 13377 | 17238 | just | `just::subcommand::<Subcommand>::list_module` | 3831 | 2261 | 119 | 3208 | 3205 | constraints 6662, liveness 2909, walk 1418 |
| 12634 | 20301 | just | `just::arguments::<Arguments>::augment_args_for_update` | 5868 | 2927 | 329 | 4284 | 4284 | constraints 4453, walk 2990, liveness 2486 |
| 11955 | 19611 | just | `just::arguments::<Arguments>::augment_args` | 5668 | 2927 | 329 | 4284 | 4284 | constraints 4293, walk 2770, liveness 2336 |
| 11630 | 17165 | just | `just::error::<Error<'src>>::fmt` | 7953 | 7722 | 759 | 11785 | 11785 | constraints 3293, liveness 3024, moves 1532 |
| 9735 | 17867 | just | `just::recipe::<Recipe<'src, Dependency<'src>>>::run_script` | 3304 | 1576 | 71 | 1815 | 1813 | constraints 3637, liveness 2600, walk 1761 |
| 9707 | 30212 | just | `just::arguments::<Arguments>::from_arg_matches_mut` | 4791 | 641 | 52 | 392 | 392 | liveness 3699, constraints 2986, walk 1047 |
| 8913 | 13578 | just | `just::compile_error_kind::<CompileErrorKind<'src>>::fmt` | 6167 | 5933 | 586 | 9039 | 9039 | constraints 2609, liveness 2140, moves 1182 |
| 8910 | 12556 | just | `just::unresolved_recipe::<Recipe<'src, UnresolvedDependency<'src>>>::resolve` | 3693 | 1770 | 157 | 2452 | 2452 | constraints 3154, liveness 2884, moves 943 |
| 8832 | 9691 | just | `just::justfile::<Justfile<'src>>::run` | 2480 | 1332 | 70 | 1959 | 1959 | constraints 3066, liveness 2149, walk 1259 |
| 8445 | 18711 | just | `just::compile_error::<CompileError<'_>>::fmt` | 5464 | 4895 | 327 | 5112 | 5112 | constraints 3317, liveness 1752, moves 965 |
| 8158 | 32073 | just | `just::arguments::<Arguments>::update_from_arg_matches_mut` | 4417 | 1198 | 154 | 847 | 847 | constraints 3941, liveness 1487, walk 989 |
| 7321 | 6886 | just | `just::evaluator::<Evaluator<'src, 'run>>::evaluate_builtin_function` | 2757 | 1123 | 37 | 1426 | 1426 | liveness 2321, constraints 1953, walk 1889 |
| 6827 | 11898 | just | `just::attribute::<Attribute<'src>>::new_arg` | 2571 | 1475 | 55 | 1754 | 1754 | constraints 2765, liveness 1855, walk 948 |
| 6644 | 8337 | just | `just::parser::<Parser<'run, 'src>>::parse_recipe` | 2280 | 1090 | 73 | 1427 | 1427 | constraints 2943, liveness 1910, walk 597 |
| 6179 | 5274 | just | `just::parser::<Parser<'run, 'src>>::parse_set` | 2657 | 989 | 54 | 1334 | 1334 | liveness 2189, constraints 2054, walk 834 |
| 6114 | 14643 | just | `just::invocation_parser::<InvocationParser<'src, 'run>>::parse_invocation` | 2109 | 1155 | 39 | 1610 | 1610 | constraints 2899, liveness 1396, walk 712 |
| 5735 | 4939 | just | `just::parser::<Parser<'run, 'src>>::parse_item` | 2388 | 1173 | 64 | 1652 | 1652 | liveness 2056, constraints 1886, walk 601 |
| 5552 | 9508 | just | `just::expression::<Expression<'_>>::serialize` | 3297 | 513 | 47 | 541 | 541 | constraints 1788, liveness 1474, walk 975 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 17039 | 28715 | just | `just::error::<Error<'_>>::fmt` | 12141 | 6749 | 445 | 6966 | 6966 | constraints 6261, liveness 4450, moves 2560 |
| 14917 | 21436 | just | `just::analyzer::<Analyzer<'run, 'src>>::justfile` | 5092 | 3166 | 163 | 4679 | 4665 | liveness 5564, constraints 5192, moves 1510 |
| 12869 | 17238 | just | `just::subcommand::<Subcommand>::list_module` | 3831 | 2261 | 119 | 3208 | 3205 | constraints 6596, liveness 2814, walk 1384 |
| 12087 | 20301 | just | `just::arguments::<Arguments>::augment_args_for_update` | 5868 | 2927 | 329 | 4284 | 4284 | constraints 4314, walk 2963, liveness 2521 |
| 11592 | 19611 | just | `just::arguments::<Arguments>::augment_args` | 5668 | 2927 | 329 | 4284 | 4284 | constraints 4290, walk 2771, liveness 2292 |
| 10707 | 17165 | just | `just::error::<Error<'src>>::fmt` | 7953 | 7722 | 759 | 11785 | 11785 | constraints 3260, liveness 3051, moves 1474 |
| 9603 | 30212 | just | `just::arguments::<Arguments>::from_arg_matches_mut` | 4791 | 641 | 52 | 392 | 392 | liveness 3788, constraints 2944, moves 1087 |
| 9467 | 17867 | just | `just::recipe::<Recipe<'src, Dependency<'src>>>::run_script` | 3304 | 1576 | 71 | 1815 | 1813 | constraints 3599, liveness 2586, walk 1712 |
| 8491 | 12556 | just | `just::unresolved_recipe::<Recipe<'src, UnresolvedDependency<'src>>>::resolve` | 3693 | 1770 | 157 | 2452 | 2452 | constraints 3112, liveness 2854, moves 935 |
| 8120 | 32073 | just | `just::arguments::<Arguments>::update_from_arg_matches_mut` | 4417 | 1198 | 154 | 847 | 847 | constraints 3928, liveness 1575, walk 965 |
| 8098 | 13578 | just | `just::compile_error_kind::<CompileErrorKind<'src>>::fmt` | 6167 | 5933 | 586 | 9039 | 9039 | constraints 2523, liveness 2178, moves 1161 |
| 7842 | 18711 | just | `just::compile_error::<CompileError<'_>>::fmt` | 5464 | 4895 | 327 | 5112 | 5112 | constraints 3197, liveness 1683, moves 984 |
| 7581 | 9691 | just | `just::justfile::<Justfile<'src>>::run` | 2480 | 1332 | 70 | 1959 | 1959 | constraints 3025, liveness 2054, walk 1248 |
| 7167 | 6886 | just | `just::evaluator::<Evaluator<'src, 'run>>::evaluate_builtin_function` | 2757 | 1123 | 37 | 1426 | 1426 | liveness 2284, constraints 1955, walk 1882 |
| 6560 | 11898 | just | `just::attribute::<Attribute<'src>>::new_arg` | 2571 | 1475 | 55 | 1754 | 1754 | constraints 2659, liveness 1838, walk 920 |
| 6316 | 8337 | just | `just::parser::<Parser<'run, 'src>>::parse_recipe` | 2280 | 1090 | 73 | 1427 | 1427 | constraints 2894, liveness 1868, walk 588 |
| 6071 | 5274 | just | `just::parser::<Parser<'run, 'src>>::parse_set` | 2657 | 989 | 54 | 1334 | 1334 | liveness 2181, constraints 2035, walk 832 |
| 5900 | 14643 | just | `just::invocation_parser::<InvocationParser<'src, 'run>>::parse_invocation` | 2109 | 1155 | 39 | 1610 | 1610 | constraints 2840, liveness 1394, walk 708 |
| 5426 | 4939 | just | `just::parser::<Parser<'run, 'src>>::parse_item` | 2388 | 1173 | 64 | 1652 | 1652 | liveness 1965, constraints 1884, walk 596 |
| 5425 | 9508 | just | `just::expression::<Expression<'_>>::serialize` | 3297 | 513 | 47 | 541 | 541 | constraints 1756, liveness 1481, walk 970 |
