| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| axum | polonius | 1616 | 7659 | 3920 | 2595 | 0 | 66.2% | 6740 | 109 | 33239 | 51802 |
| axum | nll | 1616 | 7659 | 3920 | 2519 | 0 | 64.3% | 6740 | 90 | 33110 | 51391 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 1147 | 21 | 4 | 11 | 330 | 10 | 0 | 788 | 30 | 2 | 226 |
| nll | 1124 | 21 | 3 | 10 | 322 | 9 | 0 | 779 | 3 | 1 | 223 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 51802 | 35296 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3757 | 614 | 89 | 887 | 887 | constraints 21364, moves 18571, liveness 7312 |
| 49809 | 31653 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3695 | 627 | 91 | 908 | 908 | constraints 20885, moves 17482, liveness 7066 |
| 47211 | 31941 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3549 | 578 | 84 | 833 | 833 | constraints 19263, moves 16849, liveness 6818 |
| 44762 | 29864 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3490 | 591 | 86 | 854 | 854 | constraints 18639, moves 15773, liveness 6403 |
| 44506 | 34889 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3991 | 618 | 72 | 901 | 901 | constraints 20442, moves 16084, walk 3611 |
| 43325 | 30416 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3561 | 601 | 86 | 873 | 873 | constraints 18338, moves 15763, liveness 5042 |
| 42975 | 29381 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3341 | 542 | 79 | 779 | 779 | constraints 17556, moves 15100, liveness 6256 |
| 41528 | 26648 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3285 | 555 | 81 | 800 | 800 | constraints 17188, moves 14246, liveness 6193 |
| 40143 | 31684 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3768 | 582 | 68 | 847 | 847 | constraints 18552, moves 14321, walk 3270 |
| 38599 | 27298 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3359 | 565 | 81 | 819 | 819 | constraints 16296, moves 13878, liveness 4595 |
| 38379 | 26562 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3133 | 506 | 74 | 725 | 725 | constraints 15650, moves 13168, liveness 5756 |
| 35948 | 24159 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3080 | 519 | 76 | 746 | 746 | constraints 14800, moves 12417, liveness 5335 |
| 35365 | 28624 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3545 | 546 | 64 | 793 | 793 | constraints 16213, moves 12521, walk 2997 |
| 35144 | 24846 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3157 | 529 | 76 | 765 | 765 | constraints 14842, moves 12536, liveness 4209 |
| 34497 | 168940 | axum | `axum_core::extract::tuple::<(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>::from_request` | 3732 | 576 | 62 | 832 | 831 | constraints 14871, moves 11457, walk 3634 |
| 34454 | 24031 | axum | `axum::handler::<F>::call` | 3441 | 583 | 80 | 858 | 858 | constraints 14484, moves 13741, walk 2750 |
| 33239 | 23573 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12)>>::call` | 2925 | 470 | 69 | 671 | 671 | constraints 13477, moves 11461, liveness 5066 |
| 32663 | 21510 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12)>>::call` | 2875 | 483 | 71 | 692 | 692 | constraints 13465, moves 11064, liveness 4965 |
| 32306 | 22101 | axum | `axum::handler::<F>::call` | 3239 | 547 | 75 | 804 | 804 | constraints 13718, moves 12545, walk 2659 |
| 31936 | 25696 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3322 | 510 | 60 | 739 | 739 | constraints 14678, moves 11246, walk 2725 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 51391 | 35296 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3757 | 614 | 89 | 887 | 887 | constraints 21254, moves 18584, liveness 7332 |
| 49529 | 31653 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3695 | 627 | 91 | 908 | 908 | constraints 20800, moves 17563, liveness 7114 |
| 45522 | 31941 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3549 | 578 | 84 | 833 | 833 | constraints 18770, moves 16266, liveness 6662 |
| 44225 | 29864 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3490 | 591 | 86 | 854 | 854 | constraints 18367, moves 15868, liveness 6346 |
| 44135 | 34889 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3991 | 618 | 72 | 901 | 901 | constraints 20434, moves 15982, walk 3642 |
| 42790 | 30416 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3561 | 601 | 86 | 873 | 873 | constraints 18268, moves 15737, liveness 4999 |
| 41637 | 29381 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3341 | 542 | 79 | 779 | 779 | constraints 17164, moves 14735, liveness 6072 |
| 41006 | 26648 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3285 | 555 | 81 | 800 | 800 | constraints 17114, moves 14271, liveness 6126 |
| 39012 | 31684 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3768 | 582 | 68 | 847 | 847 | constraints 18076, moves 14098, walk 3229 |
| 38781 | 27298 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3359 | 565 | 81 | 819 | 819 | constraints 16485, moves 14248, liveness 4552 |
| 37643 | 26562 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3133 | 506 | 74 | 725 | 725 | constraints 15486, moves 13253, liveness 5580 |
| 35304 | 24159 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3080 | 519 | 76 | 746 | 746 | constraints 14621, moves 12325, liveness 5307 |
| 35266 | 24846 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3157 | 529 | 76 | 765 | 765 | constraints 14907, moves 12795, liveness 4307 |
| 35121 | 28624 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3545 | 546 | 64 | 793 | 793 | constraints 16257, moves 12523, walk 2978 |
| 34225 | 168940 | axum | `axum_core::extract::tuple::<(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>::from_request` | 3732 | 576 | 62 | 832 | 831 | constraints 14925, moves 11470, walk 3639 |
| 33807 | 24031 | axum | `axum::handler::<F>::call` | 3441 | 583 | 80 | 858 | 858 | constraints 14289, moves 13678, walk 2774 |
| 33110 | 23573 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12)>>::call` | 2925 | 470 | 69 | 671 | 671 | constraints 13633, moves 11402, liveness 5064 |
| 32630 | 21510 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12)>>::call` | 2875 | 483 | 71 | 692 | 692 | constraints 13547, moves 11037, liveness 5045 |
| 31649 | 25696 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3322 | 510 | 60 | 739 | 739 | constraints 14614, moves 11245, walk 2699 |
| 31317 | 22101 | axum | `axum::handler::<F>::call` | 3239 | 547 | 75 | 804 | 804 | constraints 13279, moves 12475, walk 2615 |
