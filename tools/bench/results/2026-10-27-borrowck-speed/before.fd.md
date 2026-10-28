| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| fd | polonius | 267 | 336 | 193 | 119 | 0 | 61.4% | 1331 | 107 | 10374 | 16470 |
| fd | nll | 267 | 336 | 193 | 110 | 0 | 56.8% | 1331 | 91 | 10108 | 15748 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 57 | 2 | 0 | 2 | 20 | 1 | 0 | 18 | 2 | 0 | 12 |
| nll | 53 | 2 | 0 | 2 | 19 | 1 | 0 | 17 | 0 | 0 | 12 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 16470 | 21723 | fd | `fd::cli::<Opts>::augment_args_for_update` | 6699 | 3298 | 317 | 4612 | 4606 | constraints 6527, walk 3366, liveness 3063 |
| 16251 | 20461 | fd | `fd::cli::<Opts>::augment_args` | 6471 | 3298 | 317 | 4612 | 4606 | constraints 6670, walk 3209, liveness 2821 |
| 10374 | 29876 | fd | `fd::cli::<Opts>::update_from_arg_matches_mut` | 4424 | 1288 | 172 | 933 | 933 | constraints 5600, liveness 1758, moves 1341 |
| 8795 | 26154 | fd | `fd::cli::<Opts>::from_arg_matches_mut` | 3585 | 654 | 56 | 415 | 415 | constraints 4234, liveness 2093, moves 1035 |
| 3690 | 6095 | fd | `fd::construct_config` | 1318 | 293 | 35 | 267 | 267 | constraints 1964, moves 618, liveness 604 |
| 2297 | 4693 | fd | `fd::walk::<WorkerState>::spawn_senders` | 1127 | 384 | 24 | 369 | 369 | constraints 989, moves 501, liveness 378 |
| 1973 | 2309 | fd | `fd::run` | 639 | 144 | 14 | 140 | 140 | constraints 1145, moves 288, liveness 284 |
| 1619 | 3573 | fd | `fd::filter::owner::<OwnerFilter>::from_string` | 537 | 195 | 10 | 176 | 176 | constraints 909, moves 304, liveness 195 |
| 1547 | 1989 | fd | `fd::output::print_entry_colorized` | 724 | 270 | 20 | 260 | 260 | constraints 788, liveness 249, moves 236 |
| 1437 | 2994 | fd | `fd::walk::<WorkerState>::receive` | 319 | 157 | 6 | 180 | 180 | constraints 839, liveness 241, moves 200 |
| 1395 | 1389 | fd | `fd::walk::<WorkerState>::build_walker` | 670 | 205 | 29 | 184 | 184 | constraints 589, liveness 311, moves 218 |
| 1365 | 1115 | fd | `tests::testenv::create_working_directory` | 721 | 146 | 9 | 175 | 175 | constraints 610, liveness 332, moves 197 |
| 1302 | 1676 | fd | `fd::exec::<CommandSet>::execute_batch` | 455 | 129 | 15 | 128 | 128 | constraints 697, liveness 221, moves 200 |
| 1039 | 2325 | fd | `fd::walk::<WorkerState>::scan` | 309 | 104 | 2 | 114 | 114 | constraints 457, liveness 274, moves 185 |
| 1030 | 1502 | fd | `fd::filter::size::<SizeFilter>::parse_opt` | 447 | 192 | 15 | 168 | 168 | constraints 491, moves 170, liveness 170 |
| 999 | 1016 | fd | `fd::walk::<ReceiverBuffer<'a, W>>::poll` | 513 | 91 | 16 | 99 | 99 | constraints 422, liveness 216, moves 166 |
| 903 | 1272 | fd | `fd::fmt::<FormatTemplate>::parse` | 574 | 187 | 23 | 157 | 157 | constraints 367, liveness 192, moves 169 |
| 865 | 840 | fd | `fd::filter::time::<TimeFilter>::from_str` | 321 | 32 | 2 | 26 | 26 | constraints 395, liveness 194, moves 172 |
| 835 | 958 | fd | `fd::cli::<Exec>::from_arg_matches` | 135 | 55 | 0 | 74 | 74 | constraints 600, moves 105, liveness 66 |
| 817 | 1025 | fd | `fd::determine_ls_command` | 353 | 201 | 15 | 180 | 178 | constraints 367, moves 161, liveness 128 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 15748 | 21723 | fd | `fd::cli::<Opts>::augment_args_for_update` | 6699 | 3298 | 317 | 4612 | 4606 | constraints 6471, walk 3369, liveness 2842 |
| 15704 | 20461 | fd | `fd::cli::<Opts>::augment_args` | 6471 | 3298 | 317 | 4612 | 4606 | constraints 6471, walk 3196, liveness 3017 |
| 10108 | 29876 | fd | `fd::cli::<Opts>::update_from_arg_matches_mut` | 4424 | 1288 | 172 | 933 | 933 | constraints 5583, liveness 1737, moves 1262 |
| 8692 | 26154 | fd | `fd::cli::<Opts>::from_arg_matches_mut` | 3585 | 654 | 56 | 415 | 415 | constraints 4181, liveness 2131, moves 1013 |
| 3541 | 6095 | fd | `fd::construct_config` | 1318 | 293 | 35 | 267 | 267 | constraints 1923, liveness 586, moves 583 |
| 2201 | 4693 | fd | `fd::walk::<WorkerState>::spawn_senders` | 1127 | 384 | 24 | 369 | 369 | constraints 993, moves 474, liveness 367 |
| 1884 | 2309 | fd | `fd::run` | 639 | 144 | 14 | 140 | 140 | constraints 1087, moves 293, liveness 287 |
| 1503 | 3573 | fd | `fd::filter::owner::<OwnerFilter>::from_string` | 537 | 195 | 10 | 176 | 176 | constraints 861, moves 281, liveness 178 |
| 1429 | 1989 | fd | `fd::output::print_entry_colorized` | 724 | 270 | 20 | 260 | 260 | constraints 743, liveness 234, moves 233 |
| 1301 | 2994 | fd | `fd::walk::<WorkerState>::receive` | 319 | 157 | 6 | 180 | 180 | constraints 764, liveness 213, moves 198 |
| 1281 | 1115 | fd | `tests::testenv::create_working_directory` | 721 | 146 | 9 | 175 | 175 | constraints 618, liveness 271, moves 193 |
| 1255 | 1389 | fd | `fd::walk::<WorkerState>::build_walker` | 670 | 205 | 29 | 184 | 184 | constraints 543, liveness 311, moves 194 |
| 1208 | 1676 | fd | `fd::exec::<CommandSet>::execute_batch` | 455 | 129 | 15 | 128 | 128 | constraints 664, liveness 210, moves 186 |
| 913 | 2325 | fd | `fd::walk::<WorkerState>::scan` | 309 | 104 | 2 | 114 | 114 | constraints 412, liveness 242, moves 161 |
| 909 | 1016 | fd | `fd::walk::<ReceiverBuffer<'a, W>>::poll` | 513 | 91 | 16 | 99 | 99 | constraints 396, liveness 194, moves 159 |
| 894 | 1502 | fd | `fd::filter::size::<SizeFilter>::parse_opt` | 447 | 192 | 15 | 168 | 168 | constraints 429, moves 154, liveness 153 |
| 810 | 1272 | fd | `fd::fmt::<FormatTemplate>::parse` | 574 | 187 | 23 | 157 | 157 | constraints 345, moves 157, liveness 148 |
| 725 | 840 | fd | `fd::filter::time::<TimeFilter>::from_str` | 321 | 32 | 2 | 26 | 26 | constraints 368, liveness 157, moves 108 |
| 723 | 657 | fd | `fd::ensure_search_pattern_is_not_a_path` | 171 | 43 | 2 | 65 | 65 | constraints 534, moves 62, liveness 62 |
| 721 | 958 | fd | `fd::cli::<Exec>::from_arg_matches` | 135 | 55 | 0 | 74 | 74 | constraints 523, moves 90, liveness 58 |
