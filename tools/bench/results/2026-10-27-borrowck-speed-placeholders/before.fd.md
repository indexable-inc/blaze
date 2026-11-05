| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| fd | polonius | 267 | 309 | 177 | 85 | 0 | 48.2% | 1222 | 75 | 8002 | 14491 |
| fd | nll | 267 | 309 | 177 | 79 | 0 | 44.4% | 1222 | 63 | 8299 | 14016 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 33 | 2 | 0 | 2 | 18 | 1 | 0 | 11 | 2 | 0 | 12 |
| nll | 31 | 2 | 0 | 2 | 17 | 1 | 0 | 10 | 0 | 0 | 12 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 14491 | 19448 | fd | `fd::cli::<Opts>::augment_args_for_update` | 6699 | 3298 | 317 | 4612 | 4606 | constraints 5194, walk 3194, liveness 2895 |
| 13951 | 18745 | fd | `fd::cli::<Opts>::augment_args` | 6471 | 3298 | 317 | 4612 | 4606 | constraints 5206, walk 2999, liveness 2674 |
| 8002 | 26018 | fd | `fd::cli::<Opts>::update_from_arg_matches_mut` | 4424 | 1288 | 172 | 933 | 933 | constraints 3690, liveness 1629, walk 953 |
| 6959 | 24613 | fd | `fd::cli::<Opts>::from_arg_matches_mut` | 3585 | 654 | 56 | 415 | 415 | constraints 2693, liveness 1983, walk 860 |
| 2224 | 5837 | fd | `fd::construct_config` | 1318 | 293 | 35 | 267 | 267 | constraints 881, liveness 536, moves 321 |
| 1608 | 4455 | fd | `fd::walk::<WorkerState>::spawn_senders` | 1127 | 384 | 24 | 369 | 369 | constraints 596, liveness 315, moves 233 |
| 1136 | 2151 | fd | `fd::run` | 639 | 144 | 14 | 140 | 140 | constraints 482, liveness 253, moves 142 |
| 1012 | 1840 | fd | `fd::output::print_entry_colorized` | 724 | 270 | 20 | 260 | 260 | constraints 347, liveness 229, moves 152 |
| 933 | 1254 | fd | `fd::walk::<WorkerState>::build_walker` | 670 | 205 | 29 | 184 | 184 | liveness 283, constraints 268, moves 129 |
| 923 | 3273 | fd | `fd::filter::owner::<OwnerFilter>::from_string` | 537 | 195 | 10 | 176 | 176 | constraints 404, liveness 166, moves 130 |
| 922 | 980 | fd | `tests::testenv::create_working_directory` | 721 | 146 | 9 | 175 | 175 | constraints 295, liveness 276, moves 144 |
| 795 | 2810 | fd | `fd::walk::<WorkerState>::receive` | 319 | 157 | 6 | 180 | 180 | constraints 350, liveness 154, walk 110 |
| 755 | 1607 | fd | `fd::exec::<CommandSet>::execute_batch` | 455 | 129 | 15 | 128 | 128 | constraints 273, liveness 197, moves 106 |
| 649 | 949 | fd | `fd::walk::<ReceiverBuffer<'a, W>>::poll` | 513 | 91 | 16 | 99 | 99 | constraints 204, liveness 180, moves 96 |
| 636 | 1388 | fd | `fd::filter::size::<SizeFilter>::parse_opt` | 455 | 192 | 15 | 168 | 168 | constraints 233, liveness 126, moves 93 |
| 624 | 2221 | fd | `fd::walk::<WorkerState>::scan` | 309 | 104 | 2 | 114 | 114 | constraints 253, liveness 182, moves 78 |
| 622 | 939 | fd | `fd::determine_ls_command` | 353 | 201 | 15 | 180 | 178 | constraints 266, liveness 113, moves 76 |
| 618 | 1121 | fd | `fd::fmt::<FormatTemplate>::parse` | 575 | 187 | 23 | 157 | 157 | constraints 189, liveness 162, moves 109 |
| 523 | 795 | fd | `fd::output::print_entry` | 465 | 102 | 9 | 88 | 88 | constraints 183, liveness 131, moves 87 |
| 505 | 765 | fd | `fd::filter::time::<TimeFilter>::from_str` | 321 | 32 | 2 | 26 | 26 | liveness 177, constraints 163, moves 69 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 14016 | 19448 | fd | `fd::cli::<Opts>::augment_args_for_update` | 6699 | 3298 | 317 | 4612 | 4606 | constraints 5176, walk 3207, liveness 2957 |
| 13600 | 18745 | fd | `fd::cli::<Opts>::augment_args` | 6471 | 3298 | 317 | 4612 | 4606 | constraints 5164, walk 3026, liveness 2702 |
| 8299 | 26018 | fd | `fd::cli::<Opts>::update_from_arg_matches_mut` | 4424 | 1288 | 172 | 933 | 933 | constraints 4068, liveness 1671, walk 940 |
| 6942 | 24613 | fd | `fd::cli::<Opts>::from_arg_matches_mut` | 3585 | 654 | 56 | 415 | 415 | constraints 2691, liveness 1987, walk 858 |
| 2082 | 5837 | fd | `fd::construct_config` | 1318 | 293 | 35 | 267 | 267 | constraints 823, liveness 538, moves 297 |
| 1470 | 4455 | fd | `fd::walk::<WorkerState>::spawn_senders` | 1127 | 384 | 24 | 369 | 369 | constraints 529, liveness 304, walk 224 |
| 981 | 2151 | fd | `fd::run` | 639 | 144 | 14 | 140 | 140 | constraints 409, liveness 228, moves 132 |
| 876 | 1840 | fd | `fd::output::print_entry_colorized` | 724 | 270 | 20 | 260 | 260 | constraints 316, liveness 207, moves 125 |
| 807 | 980 | fd | `tests::testenv::create_working_directory` | 721 | 146 | 9 | 175 | 175 | constraints 260, liveness 244, moves 132 |
| 796 | 3273 | fd | `fd::filter::owner::<OwnerFilter>::from_string` | 537 | 195 | 10 | 176 | 176 | constraints 343, liveness 147, moves 112 |
| 790 | 1254 | fd | `fd::walk::<WorkerState>::build_walker` | 670 | 205 | 29 | 184 | 184 | liveness 264, constraints 215, moves 120 |
| 706 | 2810 | fd | `fd::walk::<WorkerState>::receive` | 319 | 157 | 6 | 180 | 180 | constraints 315, liveness 141, walk 88 |
| 646 | 1607 | fd | `fd::exec::<CommandSet>::execute_batch` | 455 | 129 | 15 | 128 | 128 | constraints 220, liveness 182, moves 96 |
| 569 | 949 | fd | `fd::walk::<ReceiverBuffer<'a, W>>::poll` | 513 | 91 | 16 | 99 | 99 | constraints 179, liveness 158, moves 88 |
| 540 | 1121 | fd | `fd::fmt::<FormatTemplate>::parse` | 575 | 187 | 23 | 157 | 157 | constraints 165, liveness 141, moves 100 |
| 535 | 939 | fd | `fd::determine_ls_command` | 353 | 201 | 15 | 180 | 178 | constraints 219, liveness 101, moves 82 |
| 532 | 2221 | fd | `fd::walk::<WorkerState>::scan` | 309 | 104 | 2 | 114 | 114 | constraints 196, liveness 156, moves 68 |
| 531 | 1388 | fd | `fd::filter::size::<SizeFilter>::parse_opt` | 455 | 192 | 15 | 168 | 168 | constraints 189, liveness 115, moves 81 |
| 458 | 795 | fd | `fd::output::print_entry` | 465 | 102 | 9 | 88 | 88 | constraints 151, liveness 124, moves 80 |
| 416 | 765 | fd | `fd::filter::time::<TimeFilter>::from_str` | 321 | 32 | 2 | 26 | 26 | liveness 149, constraints 126, moves 63 |
