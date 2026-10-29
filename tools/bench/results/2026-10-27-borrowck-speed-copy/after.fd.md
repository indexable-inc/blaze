| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| fd | polonius | 267 | 330 | 190 | 89 | 0 | 46.9% | 1262 | 78 | 8280 | 15563 |
| fd | nll | 267 | 330 | 190 | 81 | 0 | 42.6% | 1262 | 65 | 8098 | 14969 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 36 | 2 | 0 | 2 | 18 | 1 | 0 | 11 | 2 | 0 | 12 |
| nll | 33 | 2 | 0 | 2 | 17 | 1 | 0 | 10 | 0 | 0 | 12 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 15563 | 21721 | fd | `fd::cli::<Opts>::augment_args_for_update` | 6699 | 3298 | 317 | 4612 | 4606 | constraints 6088, liveness 3217, walk 3173 |
| 14739 | 20779 | fd | `fd::cli::<Opts>::augment_args` | 6471 | 3298 | 317 | 4612 | 4606 | constraints 5847, walk 2978, liveness 2785 |
| 8280 | 28630 | fd | `fd::cli::<Opts>::update_from_arg_matches_mut` | 4424 | 1288 | 172 | 933 | 933 | constraints 3845, liveness 1642, walk 982 |
| 7107 | 26872 | fd | `fd::cli::<Opts>::from_arg_matches_mut` | 3585 | 654 | 56 | 415 | 415 | constraints 2871, liveness 1916, walk 862 |
| 2325 | 6212 | fd | `fd::construct_config` | 1318 | 293 | 35 | 267 | 267 | constraints 948, liveness 564, moves 314 |
| 1651 | 4587 | fd | `fd::walk::<WorkerState>::spawn_senders` | 1127 | 384 | 24 | 369 | 369 | constraints 600, liveness 343, walk 229 |
| 1151 | 2294 | fd | `fd::run` | 639 | 144 | 14 | 140 | 140 | constraints 499, liveness 250, moves 144 |
| 1003 | 2041 | fd | `fd::output::print_entry_colorized` | 724 | 270 | 20 | 260 | 260 | constraints 345, liveness 245, moves 129 |
| 956 | 3343 | fd | `fd::filter::owner::<OwnerFilter>::from_string` | 537 | 195 | 10 | 176 | 176 | constraints 421, liveness 189, moves 120 |
| 950 | 1324 | fd | `fd::walk::<WorkerState>::build_walker` | 670 | 205 | 29 | 184 | 184 | liveness 285, constraints 281, moves 139 |
| 903 | 1017 | fd | `tests::testenv::create_working_directory` | 721 | 146 | 9 | 175 | 175 | constraints 294, liveness 252, moves 142 |
| 811 | 2793 | fd | `fd::walk::<WorkerState>::receive` | 319 | 157 | 6 | 180 | 180 | constraints 380, liveness 156, walk 95 |
| 766 | 1651 | fd | `fd::exec::<CommandSet>::execute_batch` | 455 | 129 | 15 | 128 | 128 | constraints 269, liveness 213, moves 106 |
| 700 | 993 | fd | `fd::determine_ls_command` | 353 | 201 | 15 | 180 | 178 | constraints 292, liveness 119, moves 77 |
| 674 | 977 | fd | `fd::walk::<ReceiverBuffer<'a, W>>::poll` | 513 | 91 | 16 | 99 | 99 | constraints 216, liveness 191, moves 96 |
| 669 | 1453 | fd | `fd::filter::size::<SizeFilter>::parse_opt` | 447 | 192 | 15 | 168 | 168 | constraints 277, liveness 127, moves 89 |
| 648 | 2243 | fd | `fd::walk::<WorkerState>::scan` | 309 | 104 | 2 | 114 | 114 | constraints 253, liveness 190, moves 91 |
| 640 | 1194 | fd | `fd::fmt::<FormatTemplate>::parse` | 574 | 187 | 23 | 157 | 157 | constraints 199, liveness 163, moves 107 |
| 542 | 828 | fd | `fd::output::print_entry` | 465 | 102 | 9 | 88 | 88 | constraints 194, liveness 136, moves 87 |
| 503 | 798 | fd | `fd::filter::time::<TimeFilter>::from_str` | 321 | 32 | 2 | 26 | 26 | constraints 171, liveness 168, moves 69 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 14969 | 21721 | fd | `fd::cli::<Opts>::augment_args_for_update` | 6699 | 3298 | 317 | 4612 | 4606 | constraints 6003, walk 3163, liveness 3031 |
| 14136 | 20779 | fd | `fd::cli::<Opts>::augment_args` | 6471 | 3298 | 317 | 4612 | 4606 | constraints 5730, walk 2989, liveness 2709 |
| 8098 | 28630 | fd | `fd::cli::<Opts>::update_from_arg_matches_mut` | 4424 | 1288 | 172 | 933 | 933 | constraints 3883, liveness 1628, walk 968 |
| 7008 | 26872 | fd | `fd::cli::<Opts>::from_arg_matches_mut` | 3585 | 654 | 56 | 415 | 415 | constraints 2831, liveness 1938, walk 866 |
| 2133 | 6212 | fd | `fd::construct_config` | 1318 | 293 | 35 | 267 | 267 | constraints 874, liveness 532, moves 298 |
| 1550 | 4587 | fd | `fd::walk::<WorkerState>::spawn_senders` | 1127 | 384 | 24 | 369 | 369 | constraints 594, liveness 324, walk 231 |
| 997 | 2294 | fd | `fd::run` | 639 | 144 | 14 | 140 | 140 | constraints 429, liveness 227, moves 131 |
| 932 | 2041 | fd | `fd::output::print_entry_colorized` | 724 | 270 | 20 | 260 | 260 | constraints 355, liveness 222, moves 123 |
| 830 | 1324 | fd | `fd::walk::<WorkerState>::build_walker` | 670 | 205 | 29 | 184 | 184 | liveness 282, constraints 222, moves 133 |
| 810 | 1017 | fd | `tests::testenv::create_working_directory` | 721 | 146 | 9 | 175 | 175 | constraints 265, liveness 242, moves 132 |
| 807 | 3343 | fd | `fd::filter::owner::<OwnerFilter>::from_string` | 537 | 195 | 10 | 176 | 176 | constraints 355, liveness 148, moves 107 |
| 714 | 2793 | fd | `fd::walk::<WorkerState>::receive` | 319 | 157 | 6 | 180 | 180 | constraints 337, liveness 142, walk 86 |
| 658 | 1651 | fd | `fd::exec::<CommandSet>::execute_batch` | 455 | 129 | 15 | 128 | 128 | constraints 236, liveness 187, moves 94 |
| 588 | 977 | fd | `fd::walk::<ReceiverBuffer<'a, W>>::poll` | 513 | 91 | 16 | 99 | 99 | constraints 189, liveness 160, moves 90 |
| 569 | 1194 | fd | `fd::fmt::<FormatTemplate>::parse` | 574 | 187 | 23 | 157 | 157 | constraints 183, liveness 148, moves 100 |
| 556 | 993 | fd | `fd::determine_ls_command` | 353 | 201 | 15 | 180 | 178 | constraints 246, liveness 107, moves 69 |
| 554 | 1453 | fd | `fd::filter::size::<SizeFilter>::parse_opt` | 447 | 192 | 15 | 168 | 168 | constraints 215, liveness 118, moves 83 |
| 521 | 2243 | fd | `fd::walk::<WorkerState>::scan` | 309 | 104 | 2 | 114 | 114 | constraints 206, liveness 163, moves 67 |
| 464 | 828 | fd | `fd::output::print_entry` | 465 | 102 | 9 | 88 | 88 | constraints 156, liveness 124, moves 78 |
| 421 | 798 | fd | `fd::filter::time::<TimeFilter>::from_str` | 321 | 32 | 2 | 26 | 26 | liveness 150, constraints 130, moves 60 |
