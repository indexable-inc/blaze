| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| fd | polonius | 267 | 323 | 183 | 88 | 0 | 48.1% | 1256 | 79 | 8327 | 15587 |
| fd | nll | 267 | 323 | 183 | 79 | 0 | 43.1% | 1256 | 64 | 8130 | 13949 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 34 | 2 | 0 | 2 | 18 | 1 | 0 | 11 | 2 | 0 | 13 |
| nll | 31 | 2 | 0 | 2 | 17 | 1 | 0 | 10 | 0 | 0 | 12 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 15587 | 20557 | fd | `fd::cli::<Opts>::augment_args` | 6471 | 3298 | 317 | 4612 | 4606 | constraints 6255, walk 3151, liveness 2960 |
| 14363 | 20138 | fd | `fd::cli::<Opts>::augment_args_for_update` | 6699 | 3298 | 317 | 4612 | 4606 | constraints 5033, walk 3293, liveness 2828 |
| 8327 | 27102 | fd | `fd::cli::<Opts>::update_from_arg_matches_mut` | 4424 | 1288 | 172 | 933 | 933 | constraints 3828, liveness 1717, walk 978 |
| 7199 | 26095 | fd | `fd::cli::<Opts>::from_arg_matches_mut` | 3585 | 654 | 56 | 415 | 415 | constraints 2783, liveness 2041, walk 872 |
| 2285 | 5790 | fd | `fd::construct_config` | 1318 | 293 | 35 | 267 | 267 | constraints 896, liveness 547, moves 317 |
| 1604 | 4479 | fd | `fd::walk::<WorkerState>::spawn_senders` | 1127 | 384 | 24 | 369 | 369 | constraints 589, liveness 320, moves 236 |
| 1151 | 2209 | fd | `fd::run` | 639 | 144 | 14 | 140 | 140 | constraints 486, liveness 253, moves 146 |
| 982 | 1843 | fd | `fd::output::print_entry_colorized` | 724 | 270 | 20 | 260 | 260 | constraints 334, liveness 224, moves 142 |
| 950 | 1263 | fd | `fd::walk::<WorkerState>::build_walker` | 670 | 205 | 29 | 184 | 184 | liveness 304, constraints 274, moves 130 |
| 909 | 3290 | fd | `fd::filter::owner::<OwnerFilter>::from_string` | 537 | 195 | 10 | 176 | 176 | constraints 392, liveness 158, moves 124 |
| 899 | 1000 | fd | `tests::testenv::create_working_directory` | 721 | 146 | 9 | 175 | 175 | constraints 288, liveness 252, moves 158 |
| 794 | 2787 | fd | `fd::walk::<WorkerState>::receive` | 319 | 157 | 6 | 180 | 180 | constraints 367, liveness 153, walk 93 |
| 768 | 1635 | fd | `fd::exec::<CommandSet>::execute_batch` | 455 | 129 | 15 | 128 | 128 | constraints 273, liveness 200, moves 120 |
| 650 | 1372 | fd | `fd::filter::size::<SizeFilter>::parse_opt` | 455 | 192 | 15 | 168 | 168 | constraints 251, liveness 126, moves 91 |
| 647 | 957 | fd | `fd::determine_ls_command` | 353 | 201 | 15 | 180 | 178 | constraints 286, liveness 115, moves 77 |
| 640 | 943 | fd | `fd::walk::<ReceiverBuffer<'a, W>>::poll` | 513 | 91 | 16 | 99 | 99 | constraints 209, liveness 172, moves 95 |
| 639 | 2256 | fd | `fd::walk::<WorkerState>::scan` | 309 | 104 | 2 | 114 | 114 | constraints 251, liveness 195, moves 79 |
| 614 | 1159 | fd | `fd::fmt::<FormatTemplate>::parse` | 575 | 187 | 23 | 157 | 157 | constraints 195, liveness 151, moves 109 |
| 532 | 773 | fd | `fd::filter::time::<TimeFilter>::from_str` | 321 | 32 | 2 | 26 | 26 | liveness 191, constraints 169, moves 79 |
| 527 | 801 | fd | `fd::output::print_entry` | 465 | 102 | 9 | 88 | 88 | constraints 176, liveness 131, moves 87 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 13949 | 20557 | fd | `fd::cli::<Opts>::augment_args` | 6471 | 3298 | 317 | 4612 | 4606 | constraints 5392, walk 3085, liveness 2741 |
| 13773 | 20138 | fd | `fd::cli::<Opts>::augment_args_for_update` | 6699 | 3298 | 317 | 4612 | 4606 | constraints 4976, walk 3237, liveness 2858 |
| 8130 | 27102 | fd | `fd::cli::<Opts>::update_from_arg_matches_mut` | 4424 | 1288 | 172 | 933 | 933 | constraints 3824, liveness 1717, walk 942 |
| 7124 | 26095 | fd | `fd::cli::<Opts>::from_arg_matches_mut` | 3585 | 654 | 56 | 415 | 415 | constraints 2763, liveness 2020, walk 868 |
| 2067 | 5790 | fd | `fd::construct_config` | 1318 | 293 | 35 | 267 | 267 | constraints 816, liveness 535, moves 292 |
| 1490 | 4479 | fd | `fd::walk::<WorkerState>::spawn_senders` | 1127 | 384 | 24 | 369 | 369 | constraints 534, liveness 331, walk 233 |
| 1003 | 2209 | fd | `fd::run` | 639 | 144 | 14 | 140 | 140 | constraints 407, liveness 242, moves 132 |
| 893 | 1843 | fd | `fd::output::print_entry_colorized` | 724 | 270 | 20 | 260 | 260 | constraints 308, liveness 208, moves 136 |
| 806 | 3290 | fd | `fd::filter::owner::<OwnerFilter>::from_string` | 537 | 195 | 10 | 176 | 176 | constraints 347, liveness 144, moves 130 |
| 805 | 1000 | fd | `tests::testenv::create_working_directory` | 721 | 146 | 9 | 175 | 175 | constraints 262, liveness 241, moves 133 |
| 804 | 1263 | fd | `fd::walk::<WorkerState>::build_walker` | 670 | 205 | 29 | 184 | 184 | liveness 274, constraints 220, moves 119 |
| 692 | 2787 | fd | `fd::walk::<WorkerState>::receive` | 319 | 157 | 6 | 180 | 180 | constraints 322, liveness 139, walk 86 |
| 652 | 1635 | fd | `fd::exec::<CommandSet>::execute_batch` | 455 | 129 | 15 | 128 | 128 | constraints 222, liveness 196, moves 95 |
| 580 | 943 | fd | `fd::walk::<ReceiverBuffer<'a, W>>::poll` | 513 | 91 | 16 | 99 | 99 | constraints 179, liveness 161, moves 96 |
| 570 | 1159 | fd | `fd::fmt::<FormatTemplate>::parse` | 575 | 187 | 23 | 157 | 157 | constraints 186, liveness 147, moves 100 |
| 543 | 1372 | fd | `fd::filter::size::<SizeFilter>::parse_opt` | 455 | 192 | 15 | 168 | 168 | constraints 201, liveness 118, moves 85 |
| 519 | 957 | fd | `fd::determine_ls_command` | 353 | 201 | 15 | 180 | 178 | constraints 220, liveness 103, moves 69 |
| 518 | 2256 | fd | `fd::walk::<WorkerState>::scan` | 309 | 104 | 2 | 114 | 114 | constraints 202, liveness 159, moves 70 |
| 471 | 801 | fd | `fd::output::print_entry` | 465 | 102 | 9 | 88 | 88 | constraints 153, liveness 131, moves 82 |
| 409 | 773 | fd | `fd::filter::time::<TimeFilter>::from_str` | 321 | 32 | 2 | 26 | 26 | liveness 139, constraints 129, moves 63 |
