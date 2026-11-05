| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| fd | polonius | 267 | 295 | 176 | 78 | 0 | 44.0% | 1186 | 73 | 7564 | 11623 |
| fd | nll | 267 | 295 | 176 | 70 | 0 | 39.5% | 1186 | 59 | 7400 | 11170 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 33 | 2 | 0 | 2 | 17 | 1 | 0 | 11 | 2 | 0 | 5 |
| nll | 30 | 2 | 0 | 2 | 16 | 1 | 0 | 10 | 0 | 0 | 5 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 11623 | 19394 | fd | `fd::cli::<Opts>::augment_args_for_update` | 6699 | 3298 | 317 | 4612 | 4606 | constraints 4941, liveness 2832, moves 1695 |
| 11477 | 18808 | fd | `fd::cli::<Opts>::augment_args` | 6471 | 3298 | 317 | 4612 | 4606 | constraints 4905, liveness 2700, moves 1669 |
| 7564 | 25675 | fd | `fd::cli::<Opts>::update_from_arg_matches_mut` | 4424 | 1288 | 172 | 933 | 933 | constraints 3700, liveness 1594, moves 866 |
| 6446 | 24050 | fd | `fd::cli::<Opts>::from_arg_matches_mut` | 3585 | 654 | 56 | 415 | 415 | constraints 2705, liveness 1951, moves 717 |
| 2125 | 5667 | fd | `fd::construct_config` | 1318 | 293 | 35 | 267 | 267 | constraints 889, liveness 510, moves 308 |
| 1427 | 4371 | fd | `fd::walk::<WorkerState>::spawn_senders` | 1127 | 384 | 24 | 369 | 369 | constraints 566, liveness 288, moves 225 |
| 1035 | 2143 | fd | `fd::run` | 639 | 144 | 14 | 140 | 140 | constraints 481, liveness 221, moves 141 |
| 959 | 1816 | fd | `fd::output::print_entry_colorized` | 724 | 270 | 20 | 260 | 260 | constraints 342, liveness 238, moves 135 |
| 906 | 1275 | fd | `fd::walk::<WorkerState>::build_walker` | 670 | 205 | 29 | 184 | 184 | constraints 272, liveness 261, moves 153 |
| 885 | 968 | fd | `tests::testenv::create_working_directory` | 721 | 146 | 9 | 175 | 175 | constraints 290, liveness 266, moves 144 |
| 846 | 3158 | fd | `fd::filter::owner::<OwnerFilter>::from_string` | 537 | 195 | 10 | 176 | 176 | constraints 393, moves 140, liveness 139 |
| 733 | 2698 | fd | `fd::walk::<WorkerState>::receive` | 319 | 157 | 6 | 180 | 180 | constraints 396, liveness 118, moves 88 |
| 729 | 1592 | fd | `fd::exec::<CommandSet>::execute_batch` | 455 | 129 | 15 | 128 | 128 | constraints 264, liveness 190, moves 106 |
| 635 | 1135 | fd | `fd::fmt::<FormatTemplate>::parse` | 575 | 187 | 23 | 157 | 157 | constraints 200, liveness 168, moves 109 |
| 634 | 931 | fd | `fd::walk::<ReceiverBuffer<'a, W>>::poll` | 513 | 91 | 16 | 99 | 99 | constraints 211, liveness 175, moves 108 |
| 611 | 1340 | fd | `fd::filter::size::<SizeFilter>::parse_opt` | 455 | 192 | 15 | 168 | 168 | constraints 227, liveness 114, moves 90 |
| 601 | 2166 | fd | `fd::walk::<WorkerState>::scan` | 309 | 104 | 2 | 114 | 114 | constraints 279, liveness 142, moves 78 |
| 592 | 986 | fd | `fd::determine_ls_command` | 353 | 201 | 15 | 180 | 178 | constraints 263, liveness 102, moves 77 |
| 551 | 798 | fd | `fd::output::print_entry` | 465 | 102 | 9 | 88 | 88 | constraints 207, liveness 141, moves 88 |
| 441 | 1037 | fd | `fd::walk::<WorkerState>::build_overrides` | 250 | 81 | 6 | 68 | 68 | constraints 187, liveness 85, moves 65 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 11170 | 19394 | fd | `fd::cli::<Opts>::augment_args_for_update` | 6699 | 3298 | 317 | 4612 | 4606 | constraints 4832, liveness 2941, moves 1689 |
| 10749 | 18808 | fd | `fd::cli::<Opts>::augment_args` | 6471 | 3298 | 317 | 4612 | 4606 | constraints 4840, liveness 2623, moves 1594 |
| 7400 | 25675 | fd | `fd::cli::<Opts>::update_from_arg_matches_mut` | 4424 | 1288 | 172 | 933 | 933 | constraints 3663, liveness 1614, moves 876 |
| 6363 | 24050 | fd | `fd::cli::<Opts>::from_arg_matches_mut` | 3585 | 654 | 56 | 415 | 415 | constraints 2688, liveness 1918, moves 708 |
| 1989 | 5667 | fd | `fd::construct_config` | 1318 | 293 | 35 | 267 | 267 | constraints 851, liveness 489, moves 289 |
| 1329 | 4371 | fd | `fd::walk::<WorkerState>::spawn_senders` | 1127 | 384 | 24 | 369 | 369 | constraints 521, liveness 308, moves 216 |
| 902 | 2143 | fd | `fd::run` | 639 | 144 | 14 | 140 | 140 | constraints 416, liveness 205, moves 130 |
| 820 | 1816 | fd | `fd::output::print_entry_colorized` | 724 | 270 | 20 | 260 | 260 | constraints 297, liveness 209, moves 125 |
| 788 | 968 | fd | `tests::testenv::create_working_directory` | 721 | 146 | 9 | 175 | 175 | constraints 256, liveness 247, moves 132 |
| 765 | 1275 | fd | `fd::walk::<WorkerState>::build_walker` | 670 | 205 | 29 | 184 | 184 | liveness 245, constraints 228, moves 122 |
| 713 | 3158 | fd | `fd::filter::owner::<OwnerFilter>::from_string` | 537 | 195 | 10 | 176 | 176 | constraints 338, liveness 125, moves 109 |
| 625 | 1592 | fd | `fd::exec::<CommandSet>::execute_batch` | 455 | 129 | 15 | 128 | 128 | constraints 226, liveness 179, moves 95 |
| 621 | 2698 | fd | `fd::walk::<WorkerState>::receive` | 319 | 157 | 6 | 180 | 180 | constraints 317, liveness 109, moves 88 |
| 554 | 931 | fd | `fd::walk::<ReceiverBuffer<'a, W>>::poll` | 513 | 91 | 16 | 99 | 99 | constraints 181, liveness 160, moves 88 |
| 549 | 1135 | fd | `fd::fmt::<FormatTemplate>::parse` | 575 | 187 | 23 | 157 | 157 | constraints 172, liveness 144, moves 101 |
| 512 | 1340 | fd | `fd::filter::size::<SizeFilter>::parse_opt` | 455 | 192 | 15 | 168 | 168 | constraints 195, liveness 106, moves 81 |
| 474 | 2166 | fd | `fd::walk::<WorkerState>::scan` | 309 | 104 | 2 | 114 | 114 | constraints 200, liveness 133, moves 66 |
| 467 | 986 | fd | `fd::determine_ls_command` | 353 | 201 | 15 | 180 | 178 | constraints 210, liveness 89, moves 68 |
| 455 | 798 | fd | `fd::output::print_entry` | 465 | 102 | 9 | 88 | 88 | constraints 156, liveness 120, moves 81 |
| 354 | 1344 | fd | `tests::testenv::format_output_error` | 268 | 192 | 10 | 171 | 171 | constraints 184, liveness 52, moves 44 |
