| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| fd | polonius | 267 | 300 | 177 | 78 | 0 | 44.0% | 1188 | 73 | 7744 | 11825 |
| fd | nll | 267 | 300 | 177 | 70 | 0 | 39.5% | 1188 | 60 | 7573 | 11292 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 33 | 2 | 0 | 2 | 17 | 1 | 0 | 11 | 2 | 0 | 5 |
| nll | 30 | 2 | 0 | 2 | 16 | 1 | 0 | 10 | 0 | 0 | 5 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 11825 | 19465 | fd | `fd::cli::<Opts>::augment_args_for_update` | 6699 | 3298 | 317 | 4612 | 4606 | constraints 4896, liveness 2950, moves 1714 |
| 11404 | 18889 | fd | `fd::cli::<Opts>::augment_args` | 6471 | 3298 | 317 | 4612 | 4606 | constraints 4912, liveness 2741, moves 1633 |
| 7744 | 25989 | fd | `fd::cli::<Opts>::update_from_arg_matches_mut` | 4424 | 1288 | 172 | 933 | 933 | constraints 3713, liveness 1712, moves 918 |
| 6427 | 24811 | fd | `fd::cli::<Opts>::from_arg_matches_mut` | 3585 | 654 | 56 | 415 | 415 | constraints 2730, liveness 1901, moves 735 |
| 2213 | 5620 | fd | `fd::construct_config` | 1318 | 293 | 35 | 267 | 267 | constraints 894, liveness 585, moves 303 |
| 1502 | 4365 | fd | `fd::walk::<WorkerState>::spawn_senders` | 1127 | 384 | 24 | 369 | 369 | constraints 614, liveness 292, moves 232 |
| 1035 | 2145 | fd | `fd::run` | 639 | 144 | 14 | 140 | 140 | constraints 462, liveness 240, moves 141 |
| 935 | 1812 | fd | `fd::output::print_entry_colorized` | 724 | 270 | 20 | 260 | 260 | constraints 348, liveness 214, moves 126 |
| 904 | 1011 | fd | `tests::testenv::create_working_directory` | 721 | 146 | 9 | 175 | 175 | constraints 317, liveness 250, moves 145 |
| 901 | 1238 | fd | `fd::walk::<WorkerState>::build_walker` | 670 | 205 | 29 | 184 | 184 | constraints 275, liveness 263, moves 141 |
| 851 | 3238 | fd | `fd::filter::owner::<OwnerFilter>::from_string` | 537 | 195 | 10 | 176 | 176 | constraints 396, liveness 139, moves 136 |
| 723 | 1597 | fd | `fd::exec::<CommandSet>::execute_batch` | 455 | 129 | 15 | 128 | 128 | constraints 265, liveness 190, moves 105 |
| 702 | 2693 | fd | `fd::walk::<WorkerState>::receive` | 319 | 157 | 6 | 180 | 180 | constraints 368, liveness 118, moves 87 |
| 663 | 1160 | fd | `fd::fmt::<FormatTemplate>::parse` | 575 | 187 | 23 | 157 | 157 | constraints 231, liveness 157, moves 112 |
| 616 | 967 | fd | `fd::walk::<ReceiverBuffer<'a, W>>::poll` | 513 | 91 | 16 | 99 | 99 | constraints 211, liveness 169, moves 97 |
| 614 | 956 | fd | `fd::determine_ls_command` | 353 | 201 | 15 | 180 | 178 | constraints 290, liveness 108, moves 76 |
| 612 | 1354 | fd | `fd::filter::size::<SizeFilter>::parse_opt` | 455 | 192 | 15 | 168 | 168 | constraints 244, liveness 113, moves 90 |
| 562 | 2173 | fd | `fd::walk::<WorkerState>::scan` | 309 | 104 | 2 | 114 | 114 | constraints 240, liveness 143, moves 77 |
| 506 | 828 | fd | `fd::output::print_entry` | 465 | 102 | 9 | 88 | 88 | constraints 175, liveness 127, moves 89 |
| 426 | 1047 | fd | `fd::walk::<WorkerState>::build_overrides` | 250 | 81 | 6 | 68 | 68 | constraints 186, liveness 86, moves 64 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 11292 | 19465 | fd | `fd::cli::<Opts>::augment_args_for_update` | 6699 | 3298 | 317 | 4612 | 4606 | constraints 4872, liveness 2901, moves 1664 |
| 10941 | 18889 | fd | `fd::cli::<Opts>::augment_args` | 6471 | 3298 | 317 | 4612 | 4606 | constraints 4858, liveness 2607, moves 1663 |
| 7573 | 25989 | fd | `fd::cli::<Opts>::update_from_arg_matches_mut` | 4424 | 1288 | 172 | 933 | 933 | constraints 3713, liveness 1697, moves 906 |
| 6410 | 24811 | fd | `fd::cli::<Opts>::from_arg_matches_mut` | 3585 | 654 | 56 | 415 | 415 | constraints 2719, liveness 1986, moves 719 |
| 2008 | 5620 | fd | `fd::construct_config` | 1318 | 293 | 35 | 267 | 267 | constraints 835, liveness 485, moves 313 |
| 1306 | 4365 | fd | `fd::walk::<WorkerState>::spawn_senders` | 1127 | 384 | 24 | 369 | 369 | constraints 515, liveness 292, moves 213 |
| 905 | 2145 | fd | `fd::run` | 639 | 144 | 14 | 140 | 140 | constraints 422, liveness 209, moves 126 |
| 847 | 1812 | fd | `fd::output::print_entry_colorized` | 724 | 270 | 20 | 260 | 260 | constraints 329, liveness 204, moves 123 |
| 812 | 1011 | fd | `tests::testenv::create_working_directory` | 721 | 146 | 9 | 175 | 175 | constraints 262, liveness 257, moves 137 |
| 753 | 1238 | fd | `fd::walk::<WorkerState>::build_walker` | 670 | 205 | 29 | 184 | 184 | liveness 241, constraints 223, moves 119 |
| 723 | 3238 | fd | `fd::filter::owner::<OwnerFilter>::from_string` | 537 | 195 | 10 | 176 | 176 | constraints 341, liveness 134, moves 107 |
| 634 | 1597 | fd | `fd::exec::<CommandSet>::execute_batch` | 455 | 129 | 15 | 128 | 128 | constraints 217, liveness 179, moves 108 |
| 610 | 2693 | fd | `fd::walk::<WorkerState>::receive` | 319 | 157 | 6 | 180 | 180 | constraints 306, liveness 110, moves 78 |
| 563 | 1160 | fd | `fd::fmt::<FormatTemplate>::parse` | 575 | 187 | 23 | 157 | 157 | constraints 178, liveness 147, moves 102 |
| 553 | 967 | fd | `fd::walk::<ReceiverBuffer<'a, W>>::poll` | 513 | 91 | 16 | 99 | 99 | constraints 178, liveness 170, moves 88 |
| 510 | 1354 | fd | `fd::filter::size::<SizeFilter>::parse_opt` | 455 | 192 | 15 | 168 | 168 | constraints 192, liveness 106, moves 81 |
| 486 | 2173 | fd | `fd::walk::<WorkerState>::scan` | 309 | 104 | 2 | 114 | 114 | constraints 215, liveness 126, moves 68 |
| 474 | 956 | fd | `fd::determine_ls_command` | 353 | 201 | 15 | 180 | 178 | constraints 215, liveness 88, moves 66 |
| 458 | 828 | fd | `fd::output::print_entry` | 465 | 102 | 9 | 88 | 88 | constraints 162, liveness 119, moves 81 |
| 360 | 1260 | fd | `fd::cli::<Opts>::search_paths` | 271 | 127 | 3 | 121 | 121 | constraints 177, moves 62, liveness 58 |
