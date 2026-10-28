| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| fd | polonius | 267 | 321 | 187 | 106 | 0 | 56.5% | 1237 | 101 | 8745 | 15313 |
| fd | nll | 267 | 321 | 187 | 97 | 0 | 51.8% | 1237 | 77 | 8575 | 14909 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 45 | 2 | 0 | 2 | 19 | 1 | 0 | 18 | 2 | 0 | 12 |
| nll | 42 | 2 | 0 | 2 | 18 | 1 | 0 | 17 | 0 | 0 | 12 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 15313 | 20785 | fd | `fd::cli::<Opts>::augment_args_for_update` | 6699 | 3298 | 317 | 4612 | 4606 | constraints 5720, walk 3308, liveness 2877 |
| 15028 | 20049 | fd | `fd::cli::<Opts>::augment_args` | 6471 | 3298 | 317 | 4612 | 4606 | constraints 5716, walk 3108, liveness 2839 |
| 8745 | 27345 | fd | `fd::cli::<Opts>::update_from_arg_matches_mut` | 4424 | 1288 | 172 | 933 | 933 | constraints 4294, liveness 1644, moves 1164 |
| 7692 | 25806 | fd | `fd::cli::<Opts>::from_arg_matches_mut` | 3585 | 654 | 56 | 415 | 415 | constraints 3248, liveness 2049, moves 1009 |
| 3363 | 6028 | fd | `fd::construct_config` | 1318 | 293 | 35 | 267 | 267 | constraints 1640, liveness 604, moves 601 |
| 2242 | 4693 | fd | `fd::walk::<WorkerState>::spawn_senders` | 1127 | 384 | 24 | 369 | 369 | constraints 920, moves 494, liveness 394 |
| 1784 | 2360 | fd | `fd::run` | 639 | 144 | 14 | 140 | 140 | constraints 921, liveness 302, moves 296 |
| 1310 | 1889 | fd | `fd::output::print_entry_colorized` | 724 | 270 | 20 | 260 | 260 | constraints 581, moves 227, liveness 224 |
| 1270 | 1329 | fd | `fd::walk::<WorkerState>::build_walker` | 670 | 205 | 29 | 184 | 184 | constraints 466, liveness 334, moves 211 |
| 1247 | 3437 | fd | `fd::filter::owner::<OwnerFilter>::from_string` | 537 | 195 | 10 | 176 | 176 | constraints 580, moves 277, liveness 180 |
| 1242 | 1744 | fd | `fd::exec::<CommandSet>::execute_batch` | 455 | 129 | 15 | 128 | 128 | constraints 620, liveness 223, moves 195 |
| 1240 | 2876 | fd | `fd::walk::<WorkerState>::receive` | 319 | 157 | 6 | 180 | 180 | constraints 601, liveness 276, moves 211 |
| 1165 | 1077 | fd | `tests::testenv::create_working_directory` | 721 | 146 | 9 | 175 | 175 | constraints 455, liveness 283, moves 196 |
| 975 | 2313 | fd | `fd::walk::<WorkerState>::scan` | 309 | 104 | 2 | 114 | 114 | constraints 372, liveness 293, moves 181 |
| 880 | 1421 | fd | `fd::filter::size::<SizeFilter>::parse_opt` | 447 | 192 | 15 | 168 | 168 | constraints 369, moves 181, liveness 147 |
| 850 | 1012 | fd | `fd::walk::<ReceiverBuffer<'a, W>>::poll` | 513 | 91 | 16 | 99 | 99 | constraints 297, liveness 198, moves 162 |
| 825 | 1203 | fd | `fd::fmt::<FormatTemplate>::parse` | 574 | 187 | 23 | 157 | 157 | constraints 290, liveness 190, moves 168 |
| 785 | 1004 | fd | `fd::determine_ls_command` | 353 | 201 | 15 | 180 | 178 | constraints 330, moves 163, liveness 130 |
| 719 | 957 | fd | `fd::cli::<Exec>::from_arg_matches` | 135 | 55 | 0 | 74 | 74 | constraints 462, moves 104, liveness 73 |
| 704 | 1322 | fd | `fd::cli::<Opts>::search_paths` | 271 | 127 | 3 | 121 | 121 | constraints 373, moves 148, liveness 88 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 14909 | 20785 | fd | `fd::cli::<Opts>::augment_args_for_update` | 6699 | 3298 | 317 | 4612 | 4606 | constraints 5687, walk 3288, liveness 2914 |
| 14451 | 20049 | fd | `fd::cli::<Opts>::augment_args` | 6471 | 3298 | 317 | 4612 | 4606 | constraints 5645, walk 3108, liveness 2739 |
| 8575 | 27345 | fd | `fd::cli::<Opts>::update_from_arg_matches_mut` | 4424 | 1288 | 172 | 933 | 933 | constraints 4199, liveness 1660, moves 1201 |
| 7584 | 25806 | fd | `fd::cli::<Opts>::from_arg_matches_mut` | 3585 | 654 | 56 | 415 | 415 | constraints 3230, liveness 2039, moves 975 |
| 3163 | 6028 | fd | `fd::construct_config` | 1318 | 293 | 35 | 267 | 267 | constraints 1543, liveness 597, moves 582 |
| 2070 | 4693 | fd | `fd::walk::<WorkerState>::spawn_senders` | 1127 | 384 | 24 | 369 | 369 | constraints 892, moves 460, liveness 357 |
| 1569 | 2360 | fd | `fd::run` | 639 | 144 | 14 | 140 | 140 | constraints 853, liveness 258, moves 246 |
| 1202 | 1889 | fd | `fd::output::print_entry_colorized` | 724 | 270 | 20 | 260 | 260 | constraints 534, liveness 229, moves 219 |
| 1165 | 2876 | fd | `fd::walk::<WorkerState>::receive` | 319 | 157 | 6 | 180 | 180 | constraints 616, liveness 241, moves 181 |
| 1137 | 1744 | fd | `fd::exec::<CommandSet>::execute_batch` | 455 | 129 | 15 | 128 | 128 | constraints 573, liveness 209, moves 204 |
| 1131 | 1329 | fd | `fd::walk::<WorkerState>::build_walker` | 670 | 205 | 29 | 184 | 184 | constraints 439, liveness 293, moves 192 |
| 1120 | 3437 | fd | `fd::filter::owner::<OwnerFilter>::from_string` | 537 | 195 | 10 | 176 | 176 | constraints 510, moves 265, liveness 169 |
| 1063 | 1077 | fd | `tests::testenv::create_working_directory` | 721 | 146 | 9 | 175 | 175 | constraints 432, liveness 260, moves 187 |
| 808 | 2313 | fd | `fd::walk::<WorkerState>::scan` | 309 | 104 | 2 | 114 | 114 | constraints 298, liveness 237, moves 174 |
| 806 | 1012 | fd | `fd::walk::<ReceiverBuffer<'a, W>>::poll` | 513 | 91 | 16 | 99 | 99 | constraints 275, liveness 218, moves 154 |
| 774 | 1421 | fd | `fd::filter::size::<SizeFilter>::parse_opt` | 447 | 192 | 15 | 168 | 168 | constraints 335, moves 152, liveness 139 |
| 756 | 1203 | fd | `fd::fmt::<FormatTemplate>::parse` | 574 | 187 | 23 | 157 | 157 | constraints 283, moves 172, liveness 153 |
| 643 | 1004 | fd | `fd::determine_ls_command` | 353 | 201 | 15 | 180 | 178 | constraints 281, moves 122, liveness 116 |
| 626 | 696 | fd | `fd::ensure_search_pattern_is_not_a_path` | 171 | 43 | 2 | 65 | 65 | constraints 436, moves 63, liveness 62 |
| 625 | 1322 | fd | `fd::cli::<Opts>::search_paths` | 271 | 127 | 3 | 121 | 121 | constraints 335, moves 124, liveness 79 |
