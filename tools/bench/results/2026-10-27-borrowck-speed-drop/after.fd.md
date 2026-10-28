| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| fd | polonius | 267 | 300 | 178 | 79 | 0 | 44.5% | 1179 | 74 | 7745 | 12299 |
| fd | nll | 267 | 300 | 178 | 71 | 0 | 40.0% | 1179 | 59 | 7515 | 12038 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 34 | 2 | 0 | 2 | 18 | 1 | 0 | 11 | 2 | 0 | 5 |
| nll | 31 | 2 | 0 | 2 | 17 | 1 | 0 | 10 | 0 | 0 | 5 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 12299 | 19438 | fd | `fd::cli::<Opts>::augment_args_for_update` | 6699 | 3298 | 317 | 4612 | 4606 | constraints 5289, liveness 2986, moves 1742 |
| 11813 | 18815 | fd | `fd::cli::<Opts>::augment_args` | 6471 | 3298 | 317 | 4612 | 4606 | constraints 5329, liveness 2739, moves 1603 |
| 7745 | 26254 | fd | `fd::cli::<Opts>::update_from_arg_matches_mut` | 4424 | 1288 | 172 | 933 | 933 | constraints 3716, liveness 1668, moves 944 |
| 6602 | 24738 | fd | `fd::cli::<Opts>::from_arg_matches_mut` | 3585 | 654 | 56 | 415 | 415 | constraints 2720, liveness 2068, moves 750 |
| 2149 | 5738 | fd | `fd::construct_config` | 1318 | 293 | 35 | 267 | 267 | constraints 884, liveness 510, moves 314 |
| 1472 | 4475 | fd | `fd::walk::<WorkerState>::spawn_senders` | 1127 | 384 | 24 | 369 | 369 | constraints 587, liveness 295, moves 230 |
| 1051 | 2176 | fd | `fd::run` | 639 | 144 | 14 | 140 | 140 | constraints 474, liveness 243, moves 142 |
| 953 | 1858 | fd | `fd::output::print_entry_colorized` | 724 | 270 | 20 | 260 | 260 | constraints 356, liveness 216, moves 129 |
| 923 | 1256 | fd | `fd::walk::<WorkerState>::build_walker` | 670 | 205 | 29 | 184 | 184 | constraints 298, liveness 276, moves 128 |
| 905 | 993 | fd | `tests::testenv::create_working_directory` | 721 | 146 | 9 | 175 | 175 | constraints 294, liveness 274, moves 145 |
| 852 | 3225 | fd | `fd::filter::owner::<OwnerFilter>::from_string` | 537 | 195 | 10 | 176 | 176 | constraints 399, liveness 140, moves 136 |
| 737 | 1599 | fd | `fd::exec::<CommandSet>::execute_batch` | 455 | 129 | 15 | 128 | 128 | constraints 278, liveness 190, moves 104 |
| 714 | 2782 | fd | `fd::walk::<WorkerState>::receive` | 319 | 157 | 6 | 180 | 180 | constraints 364, liveness 117, moves 99 |
| 656 | 943 | fd | `fd::walk::<ReceiverBuffer<'a, W>>::poll` | 513 | 91 | 16 | 99 | 99 | constraints 227, liveness 187, moves 95 |
| 621 | 1524 | fd | `fd::filter::size::<SizeFilter>::parse_opt` | 455 | 192 | 15 | 168 | 168 | constraints 245, liveness 115, moves 91 |
| 616 | 1134 | fd | `fd::fmt::<FormatTemplate>::parse` | 575 | 187 | 23 | 157 | 157 | constraints 200, liveness 151, moves 109 |
| 613 | 2242 | fd | `fd::walk::<WorkerState>::scan` | 309 | 104 | 2 | 114 | 114 | constraints 247, liveness 183, moves 79 |
| 611 | 945 | fd | `fd::determine_ls_command` | 353 | 201 | 15 | 180 | 178 | constraints 290, liveness 103, moves 76 |
| 513 | 815 | fd | `fd::output::print_entry` | 465 | 102 | 9 | 88 | 88 | constraints 172, liveness 130, moves 88 |
| 438 | 1062 | fd | `fd::walk::<WorkerState>::build_overrides` | 250 | 81 | 6 | 68 | 68 | constraints 186, liveness 98, moves 64 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 12038 | 19438 | fd | `fd::cli::<Opts>::augment_args_for_update` | 6699 | 3298 | 317 | 4612 | 4606 | constraints 5461, liveness 3002, moves 1686 |
| 11309 | 18815 | fd | `fd::cli::<Opts>::augment_args` | 6471 | 3298 | 317 | 4612 | 4606 | constraints 5223, liveness 2708, moves 1662 |
| 7515 | 26254 | fd | `fd::cli::<Opts>::update_from_arg_matches_mut` | 4424 | 1288 | 172 | 933 | 933 | constraints 3666, liveness 1650, moves 909 |
| 6528 | 24738 | fd | `fd::cli::<Opts>::from_arg_matches_mut` | 3585 | 654 | 56 | 415 | 415 | constraints 2703, liveness 2092, moves 733 |
| 1982 | 5738 | fd | `fd::construct_config` | 1318 | 293 | 35 | 267 | 267 | constraints 821, liveness 494, moves 298 |
| 1343 | 4475 | fd | `fd::walk::<WorkerState>::spawn_senders` | 1127 | 384 | 24 | 369 | 369 | constraints 533, liveness 281, moves 217 |
| 878 | 2176 | fd | `fd::run` | 639 | 144 | 14 | 140 | 140 | constraints 397, liveness 208, moves 125 |
| 811 | 1858 | fd | `fd::output::print_entry_colorized` | 724 | 270 | 20 | 260 | 260 | constraints 294, liveness 206, moves 122 |
| 802 | 993 | fd | `tests::testenv::create_working_directory` | 721 | 146 | 9 | 175 | 175 | constraints 262, liveness 243, moves 141 |
| 779 | 1256 | fd | `fd::walk::<WorkerState>::build_walker` | 670 | 205 | 29 | 184 | 184 | liveness 261, constraints 215, moves 119 |
| 685 | 3225 | fd | `fd::filter::owner::<OwnerFilter>::from_string` | 537 | 195 | 10 | 176 | 176 | constraints 318, liveness 125, moves 105 |
| 628 | 1599 | fd | `fd::exec::<CommandSet>::execute_batch` | 455 | 129 | 15 | 128 | 128 | constraints 215, liveness 192, moves 94 |
| 617 | 2782 | fd | `fd::walk::<WorkerState>::receive` | 319 | 157 | 6 | 180 | 180 | constraints 317, liveness 107, moves 77 |
| 593 | 943 | fd | `fd::walk::<ReceiverBuffer<'a, W>>::poll` | 513 | 91 | 16 | 99 | 99 | constraints 213, liveness 168, moves 92 |
| 557 | 1134 | fd | `fd::fmt::<FormatTemplate>::parse` | 575 | 187 | 23 | 157 | 157 | constraints 175, liveness 139, moves 99 |
| 517 | 1524 | fd | `fd::filter::size::<SizeFilter>::parse_opt` | 455 | 192 | 15 | 168 | 168 | constraints 196, liveness 106, moves 83 |
| 481 | 2242 | fd | `fd::walk::<WorkerState>::scan` | 309 | 104 | 2 | 114 | 114 | constraints 205, liveness 131, moves 68 |
| 473 | 945 | fd | `fd::determine_ls_command` | 353 | 201 | 15 | 180 | 178 | constraints 210, liveness 90, moves 69 |
| 446 | 815 | fd | `fd::output::print_entry` | 465 | 102 | 9 | 88 | 88 | constraints 149, liveness 120, moves 81 |
| 363 | 1282 | fd | `fd::cli::<Opts>::search_paths` | 271 | 127 | 3 | 121 | 121 | constraints 187, liveness 61, moves 52 |
