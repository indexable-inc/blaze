| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| axum | polonius | 1616 | 7330 | 3801 | 1685 | 0 | 44.3% | 8246 | 87 | 19639 | 32328 |
| axum | nll | 1616 | 7330 | 3801 | 1622 | 0 | 42.7% | 8246 | 72 | 19102 | 31742 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 485 | 22 | 3 | 10 | 290 | 9 | 0 | 108 | 29 | 1 | 702 |
| nll | 468 | 23 | 3 | 9 | 283 | 8 | 0 | 104 | 2 | 1 | 697 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 32328 | 35920 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3757 | 614 | 89 | 887 | 887 | walk 17617, liveness 7038, constraints 5710 |
| 30055 | 32141 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3695 | 627 | 91 | 908 | 908 | walk 16548, liveness 6709, constraints 4912 |
| 28543 | 32287 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3549 | 578 | 84 | 833 | 833 | walk 15339, liveness 6236, constraints 5109 |
| 27313 | 29193 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3490 | 591 | 86 | 854 | 854 | walk 14749, liveness 6170, constraints 4633 |
| 25587 | 28764 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3341 | 542 | 79 | 779 | 779 | walk 13462, liveness 5678, constraints 4700 |
| 25469 | 34420 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3991 | 618 | 72 | 901 | 901 | walk 14950, constraints 5730, liveness 2780 |
| 24825 | 26342 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3285 | 555 | 81 | 800 | 800 | walk 13089, liveness 5656, constraints 4361 |
| 24824 | 30569 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3561 | 601 | 86 | 873 | 873 | walk 14692, liveness 4458, constraints 3758 |
| 24161 | 26923 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3133 | 506 | 74 | 725 | 725 | walk 12258, liveness 5372, constraints 4839 |
| 23506 | 31325 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3768 | 582 | 68 | 847 | 847 | walk 13379, constraints 5453, liveness 2616 |
| 23019 | 23759 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3080 | 519 | 76 | 746 | 746 | walk 11683, liveness 5291, constraints 4360 |
| 22124 | 27236 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3359 | 565 | 81 | 819 | 819 | walk 12759, liveness 4083, constraints 3459 |
| 21793 | 23372 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12)>>::call` | 2925 | 470 | 69 | 671 | 671 | walk 10813, liveness 4757, constraints 4556 |
| 20752 | 24341 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3157 | 529 | 76 | 765 | 765 | walk 11596, liveness 3808, constraints 3618 |
| 20573 | 28233 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3545 | 546 | 64 | 793 | 793 | walk 11659, constraints 4886, liveness 2296 |
| 19953 | 21299 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12)>>::call` | 2875 | 483 | 71 | 692 | 692 | walk 10116, liveness 4644, constraints 3706 |
| 19639 | 24476 | axum | `axum::handler::<F>::call` | 3441 | 583 | 80 | 858 | 858 | walk 13209, constraints 2342, liveness 2293 |
| 19116 | 25788 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3322 | 510 | 60 | 739 | 739 | walk 10545, constraints 4715, liveness 2111 |
| 18636 | 170131 | axum | `axum_core::extract::tuple::<(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>::from_request` | 3732 | 576 | 62 | 832 | 831 | walk 11147, constraints 2911, liveness 2457 |
| 18572 | 20923 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11)>>::call` | 2717 | 434 | 64 | 617 | 617 | walk 9139, liveness 4123, constraints 3843 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 31742 | 35920 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3757 | 614 | 89 | 887 | 887 | walk 17477, liveness 7023, constraints 5529 |
| 29685 | 32141 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3695 | 627 | 91 | 908 | 908 | walk 16502, liveness 6701, constraints 4893 |
| 28080 | 32287 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3549 | 578 | 84 | 833 | 833 | walk 15303, liveness 6236, constraints 4999 |
| 26987 | 29193 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3490 | 591 | 86 | 854 | 854 | walk 14695, liveness 6171, constraints 4583 |
| 25256 | 28764 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3341 | 542 | 79 | 779 | 779 | walk 13479, liveness 5714, constraints 4634 |
| 25054 | 34420 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3991 | 618 | 72 | 901 | 901 | walk 14881, constraints 5547, liveness 2758 |
| 24249 | 26923 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3133 | 506 | 74 | 725 | 725 | walk 12415, liveness 5449, constraints 4900 |
| 24223 | 26342 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3285 | 555 | 81 | 800 | 800 | walk 12966, liveness 5631, constraints 4237 |
| 24194 | 30569 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3561 | 601 | 86 | 873 | 873 | walk 14648, liveness 4397, constraints 3607 |
| 22734 | 31325 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3768 | 582 | 68 | 847 | 847 | walk 13235, constraints 5150, liveness 2504 |
| 22459 | 23759 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3080 | 519 | 76 | 746 | 746 | walk 11592, liveness 5323, constraints 4161 |
| 21569 | 27236 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3359 | 565 | 81 | 819 | 819 | walk 12728, liveness 4034, constraints 3326 |
| 21489 | 23372 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12)>>::call` | 2925 | 470 | 69 | 671 | 671 | walk 10782, liveness 4769, constraints 4511 |
| 20241 | 28233 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3545 | 546 | 64 | 793 | 793 | walk 11662, constraints 4795, liveness 2247 |
| 20069 | 24341 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3157 | 529 | 76 | 765 | 765 | walk 11489, liveness 3773, constraints 3442 |
| 19572 | 21299 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12)>>::call` | 2875 | 483 | 71 | 692 | 692 | walk 10044, liveness 4659, constraints 3647 |
| 19102 | 24476 | axum | `axum::handler::<F>::call` | 3441 | 583 | 80 | 858 | 858 | walk 13134, liveness 2297, constraints 2212 |
| 18736 | 25788 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3322 | 510 | 60 | 739 | 739 | walk 10477, constraints 4607, liveness 2118 |
| 18205 | 20923 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11)>>::call` | 2717 | 434 | 64 | 617 | 617 | walk 9083, liveness 4120, constraints 3745 |
| 17926 | 170131 | axum | `axum_core::extract::tuple::<(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>::from_request` | 3732 | 576 | 62 | 832 | 831 | walk 10995, constraints 2668, liveness 2423 |
