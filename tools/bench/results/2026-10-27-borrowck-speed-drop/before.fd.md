| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| fd | polonius | 267 | 297 | 176 | 86 | 0 | 48.6% | 1193 | 77 | 8086 | 14264 |
| fd | nll | 267 | 297 | 176 | 78 | 0 | 44.3% | 1193 | 62 | 7861 | 13862 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 33 | 2 | 0 | 2 | 18 | 1 | 0 | 11 | 2 | 0 | 12 |
| nll | 30 | 2 | 0 | 2 | 17 | 1 | 0 | 10 | 0 | 0 | 12 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 14264 | 19215 | fd | `fd::cli::<Opts>::augment_args_for_update` | 6699 | 3298 | 317 | 4612 | 4606 | constraints 5176, walk 3165, liveness 2855 |
| 14022 | 19068 | fd | `fd::cli::<Opts>::augment_args` | 6471 | 3298 | 317 | 4612 | 4606 | constraints 5170, walk 3014, liveness 2704 |
| 8086 | 26075 | fd | `fd::cli::<Opts>::update_from_arg_matches_mut` | 4424 | 1288 | 172 | 933 | 933 | constraints 3661, liveness 1684, walk 944 |
| 6912 | 24349 | fd | `fd::cli::<Opts>::from_arg_matches_mut` | 3585 | 654 | 56 | 415 | 415 | constraints 2714, liveness 1942, walk 852 |
| 2242 | 5757 | fd | `fd::construct_config` | 1318 | 293 | 35 | 267 | 267 | constraints 895, liveness 555, moves 304 |
| 1615 | 4324 | fd | `fd::walk::<WorkerState>::spawn_senders` | 1127 | 384 | 24 | 369 | 369 | constraints 566, liveness 328, walk 254 |
| 1108 | 2125 | fd | `fd::run` | 639 | 144 | 14 | 140 | 140 | constraints 455, liveness 245, moves 152 |
| 965 | 1767 | fd | `fd::output::print_entry_colorized` | 724 | 270 | 20 | 260 | 260 | constraints 335, liveness 217, moves 129 |
| 926 | 1231 | fd | `fd::walk::<WorkerState>::build_walker` | 670 | 205 | 29 | 184 | 184 | liveness 291, constraints 263, moves 128 |
| 908 | 3167 | fd | `fd::filter::owner::<OwnerFilter>::from_string` | 537 | 195 | 10 | 176 | 176 | constraints 390, liveness 171, moves 124 |
| 905 | 948 | fd | `tests::testenv::create_working_directory` | 721 | 146 | 9 | 175 | 175 | constraints 280, liveness 272, moves 144 |
| 791 | 2711 | fd | `fd::walk::<WorkerState>::receive` | 319 | 157 | 6 | 180 | 180 | constraints 351, liveness 152, walk 105 |
| 745 | 1594 | fd | `fd::exec::<CommandSet>::execute_batch` | 455 | 129 | 15 | 128 | 128 | constraints 262, liveness 205, moves 102 |
| 650 | 941 | fd | `fd::determine_ls_command` | 353 | 201 | 15 | 180 | 178 | constraints 286, liveness 114, moves 75 |
| 647 | 920 | fd | `fd::walk::<ReceiverBuffer<'a, W>>::poll` | 513 | 91 | 16 | 99 | 99 | constraints 208, liveness 169, moves 95 |
| 630 | 1331 | fd | `fd::filter::size::<SizeFilter>::parse_opt` | 455 | 192 | 15 | 168 | 168 | constraints 227, liveness 126, moves 103 |
| 624 | 1128 | fd | `fd::fmt::<FormatTemplate>::parse` | 575 | 187 | 23 | 157 | 157 | constraints 194, liveness 149, moves 121 |
| 608 | 2157 | fd | `fd::walk::<WorkerState>::scan` | 309 | 104 | 2 | 114 | 114 | constraints 238, liveness 180, moves 79 |
| 522 | 778 | fd | `fd::output::print_entry` | 465 | 102 | 9 | 88 | 88 | constraints 169, liveness 142, moves 90 |
| 495 | 749 | fd | `fd::filter::time::<TimeFilter>::from_str` | 321 | 32 | 2 | 26 | 26 | liveness 169, constraints 163, moves 69 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 13862 | 19215 | fd | `fd::cli::<Opts>::augment_args_for_update` | 6699 | 3298 | 317 | 4612 | 4606 | constraints 5135, walk 3160, liveness 2841 |
| 13512 | 19068 | fd | `fd::cli::<Opts>::augment_args` | 6471 | 3298 | 317 | 4612 | 4606 | constraints 5105, walk 3010, liveness 2695 |
| 7861 | 26075 | fd | `fd::cli::<Opts>::update_from_arg_matches_mut` | 4424 | 1288 | 172 | 933 | 933 | constraints 3632, liveness 1643, walk 930 |
| 6927 | 24349 | fd | `fd::cli::<Opts>::from_arg_matches_mut` | 3585 | 654 | 56 | 415 | 415 | constraints 2678, liveness 1962, walk 856 |
| 2065 | 5757 | fd | `fd::construct_config` | 1318 | 293 | 35 | 267 | 267 | constraints 822, liveness 516, moves 291 |
| 1467 | 4324 | fd | `fd::walk::<WorkerState>::spawn_senders` | 1127 | 384 | 24 | 369 | 369 | constraints 525, liveness 301, walk 234 |
| 993 | 2125 | fd | `fd::run` | 639 | 144 | 14 | 140 | 140 | constraints 413, liveness 229, walk 133 |
| 848 | 1767 | fd | `fd::output::print_entry_colorized` | 724 | 270 | 20 | 260 | 260 | constraints 295, liveness 207, moves 120 |
| 814 | 948 | fd | `tests::testenv::create_working_directory` | 721 | 146 | 9 | 175 | 175 | liveness 255, constraints 254, moves 135 |
| 796 | 3167 | fd | `fd::filter::owner::<OwnerFilter>::from_string` | 537 | 195 | 10 | 176 | 176 | constraints 358, liveness 143, moves 110 |
| 795 | 1231 | fd | `fd::walk::<WorkerState>::build_walker` | 670 | 205 | 29 | 184 | 184 | liveness 268, constraints 213, moves 120 |
| 690 | 2711 | fd | `fd::walk::<WorkerState>::receive` | 319 | 157 | 6 | 180 | 180 | constraints 304, liveness 137, walk 97 |
| 645 | 1594 | fd | `fd::exec::<CommandSet>::execute_batch` | 455 | 129 | 15 | 128 | 128 | constraints 224, liveness 188, moves 94 |
| 572 | 920 | fd | `fd::walk::<ReceiverBuffer<'a, W>>::poll` | 513 | 91 | 16 | 99 | 99 | constraints 181, liveness 160, moves 89 |
| 537 | 941 | fd | `fd::determine_ls_command` | 353 | 201 | 15 | 180 | 178 | constraints 233, liveness 101, moves 71 |
| 537 | 1128 | fd | `fd::fmt::<FormatTemplate>::parse` | 575 | 187 | 23 | 157 | 157 | constraints 164, liveness 139, moves 99 |
| 523 | 1331 | fd | `fd::filter::size::<SizeFilter>::parse_opt` | 455 | 192 | 15 | 168 | 168 | constraints 190, liveness 115, moves 82 |
| 511 | 2157 | fd | `fd::walk::<WorkerState>::scan` | 309 | 104 | 2 | 114 | 114 | constraints 195, liveness 152, moves 78 |
| 457 | 778 | fd | `fd::output::print_entry` | 465 | 102 | 9 | 88 | 88 | constraints 150, liveness 123, moves 81 |
| 419 | 749 | fd | `fd::filter::time::<TimeFilter>::from_str` | 321 | 32 | 2 | 26 | 26 | liveness 152, constraints 127, moves 62 |
