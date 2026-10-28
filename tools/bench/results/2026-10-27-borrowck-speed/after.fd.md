| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| fd | polonius | 267 | 360 | 200 | 111 | 0 | 55.7% | 1303 | 97 | 9658 | 17642 |
| fd | nll | 267 | 360 | 200 | 101 | 0 | 50.8% | 1303 | 81 | 9067 | 16622 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 48 | 2 | 0 | 2 | 20 | 1 | 0 | 18 | 2 | 0 | 12 |
| nll | 45 | 2 | 0 | 2 | 19 | 1 | 0 | 17 | 0 | 0 | 12 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 17642 | 22945 | fd | `fd::cli::<Opts>::augment_args_for_update` | 6699 | 3298 | 317 | 4612 | 4606 | constraints 7308, walk 3470, liveness 3111 |
| 16654 | 22384 | fd | `fd::cli::<Opts>::augment_args` | 6471 | 3298 | 317 | 4612 | 4606 | constraints 6915, walk 3240, liveness 2912 |
| 9658 | 29990 | fd | `fd::cli::<Opts>::update_from_arg_matches_mut` | 4424 | 1288 | 172 | 933 | 933 | constraints 4758, liveness 1838, moves 1307 |
| 8062 | 27564 | fd | `fd::cli::<Opts>::from_arg_matches_mut` | 3585 | 654 | 56 | 415 | 415 | constraints 3472, liveness 2125, moves 1047 |
| 3402 | 6309 | fd | `fd::construct_config` | 1318 | 293 | 35 | 267 | 267 | constraints 1666, moves 615, liveness 614 |
| 2318 | 5267 | fd | `fd::walk::<WorkerState>::spawn_senders` | 1127 | 384 | 24 | 369 | 369 | constraints 998, moves 492, liveness 385 |
| 1785 | 2474 | fd | `fd::run` | 639 | 144 | 14 | 140 | 140 | constraints 938, liveness 307, moves 278 |
| 1364 | 1972 | fd | `fd::output::print_entry_colorized` | 724 | 270 | 20 | 260 | 260 | constraints 596, moves 243, liveness 243 |
| 1316 | 1349 | fd | `fd::walk::<WorkerState>::build_walker` | 670 | 205 | 29 | 184 | 184 | constraints 517, liveness 326, moves 208 |
| 1282 | 1772 | fd | `fd::exec::<CommandSet>::execute_batch` | 455 | 129 | 15 | 128 | 128 | constraints 646, liveness 230, moves 214 |
| 1258 | 3234 | fd | `fd::walk::<WorkerState>::receive` | 319 | 157 | 6 | 180 | 180 | constraints 633, liveness 261, moves 196 |
| 1245 | 3538 | fd | `fd::filter::owner::<OwnerFilter>::from_string` | 537 | 195 | 10 | 176 | 176 | constraints 563, moves 290, liveness 178 |
| 1229 | 1102 | fd | `tests::testenv::create_working_directory` | 721 | 146 | 9 | 175 | 175 | constraints 491, liveness 294, moves 209 |
| 945 | 2484 | fd | `fd::walk::<WorkerState>::scan` | 309 | 104 | 2 | 114 | 114 | constraints 360, liveness 278, moves 182 |
| 924 | 1527 | fd | `fd::filter::size::<SizeFilter>::parse_opt` | 447 | 192 | 15 | 168 | 168 | constraints 403, moves 175, liveness 151 |
| 846 | 1010 | fd | `fd::walk::<ReceiverBuffer<'a, W>>::poll` | 513 | 91 | 16 | 99 | 99 | constraints 290, liveness 212, moves 161 |
| 821 | 1242 | fd | `fd::fmt::<FormatTemplate>::parse` | 574 | 187 | 23 | 157 | 157 | constraints 306, moves 169, liveness 156 |
| 768 | 1081 | fd | `fd::determine_ls_command` | 353 | 201 | 15 | 180 | 178 | constraints 326, moves 149, liveness 132 |
| 758 | 825 | fd | `fd::filter::time::<TimeFilter>::from_str` | 321 | 32 | 2 | 26 | 26 | constraints 314, liveness 192, moves 147 |
| 686 | 808 | fd | `tests::testenv::normalize_output` | 201 | 131 | 10 | 141 | 139 | constraints 381, walk 90, moves 87 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 16622 | 22945 | fd | `fd::cli::<Opts>::augment_args_for_update` | 6699 | 3298 | 317 | 4612 | 4606 | constraints 7059, walk 3384, liveness 3027 |
| 15948 | 22384 | fd | `fd::cli::<Opts>::augment_args` | 6471 | 3298 | 317 | 4612 | 4606 | constraints 6827, walk 3174, liveness 2837 |
| 9067 | 29990 | fd | `fd::cli::<Opts>::update_from_arg_matches_mut` | 4424 | 1288 | 172 | 933 | 933 | constraints 4552, liveness 1688, moves 1262 |
| 8005 | 27564 | fd | `fd::cli::<Opts>::from_arg_matches_mut` | 3585 | 654 | 56 | 415 | 415 | constraints 3423, liveness 2118, moves 1043 |
| 3220 | 6309 | fd | `fd::construct_config` | 1318 | 293 | 35 | 267 | 267 | constraints 1545, liveness 631, moves 573 |
| 2190 | 5267 | fd | `fd::walk::<WorkerState>::spawn_senders` | 1127 | 384 | 24 | 369 | 369 | constraints 944, moves 470, liveness 401 |
| 1655 | 2474 | fd | `fd::run` | 639 | 144 | 14 | 140 | 140 | constraints 906, liveness 269, moves 249 |
| 1231 | 1972 | fd | `fd::output::print_entry_colorized` | 724 | 270 | 20 | 260 | 260 | constraints 565, liveness 221, moves 216 |
| 1160 | 3538 | fd | `fd::filter::owner::<OwnerFilter>::from_string` | 537 | 195 | 10 | 176 | 176 | constraints 544, moves 255, liveness 174 |
| 1151 | 1349 | fd | `fd::walk::<WorkerState>::build_walker` | 670 | 205 | 29 | 184 | 184 | constraints 426, liveness 300, moves 205 |
| 1149 | 1772 | fd | `fd::exec::<CommandSet>::execute_batch` | 455 | 129 | 15 | 128 | 128 | constraints 594, liveness 214, moves 187 |
| 1109 | 3234 | fd | `fd::walk::<WorkerState>::receive` | 319 | 157 | 6 | 180 | 180 | constraints 569, liveness 237, moves 178 |
| 1078 | 1102 | fd | `tests::testenv::create_working_directory` | 721 | 146 | 9 | 175 | 175 | constraints 438, liveness 261, moves 188 |
| 819 | 2484 | fd | `fd::walk::<WorkerState>::scan` | 309 | 104 | 2 | 114 | 114 | constraints 319, liveness 247, moves 156 |
| 808 | 1527 | fd | `fd::filter::size::<SizeFilter>::parse_opt` | 447 | 192 | 15 | 168 | 168 | constraints 337, moves 165, liveness 140 |
| 789 | 1010 | fd | `fd::walk::<ReceiverBuffer<'a, W>>::poll` | 513 | 91 | 16 | 99 | 99 | constraints 283, liveness 191, moves 151 |
| 737 | 1242 | fd | `fd::fmt::<FormatTemplate>::parse` | 574 | 187 | 23 | 157 | 157 | constraints 275, moves 159, liveness 155 |
| 657 | 1081 | fd | `fd::determine_ls_command` | 353 | 201 | 15 | 180 | 178 | constraints 266, moves 122, liveness 117 |
| 615 | 1390 | fd | `fd::cli::<Opts>::search_paths` | 271 | 127 | 3 | 121 | 121 | constraints 335, moves 122, liveness 77 |
| 614 | 663 | fd | `fd::ensure_search_pattern_is_not_a_path` | 171 | 43 | 2 | 65 | 65 | constraints 428, liveness 63, moves 61 |
