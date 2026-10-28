| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| axum | polonius | 1616 | 7411 | 3885 | 2575 | 0 | 66.3% | 6713 | 110 | 32816 | 50744 |
| axum | nll | 1616 | 7411 | 3885 | 2511 | 0 | 64.6% | 6713 | 91 | 32349 | 50430 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 1138 | 21 | 3 | 10 | 321 | 9 | 0 | 791 | 30 | 2 | 224 |
| nll | 1120 | 21 | 3 | 10 | 316 | 9 | 0 | 784 | 3 | 1 | 221 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 50744 | 34959 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3757 | 614 | 89 | 887 | 887 | constraints 20760, moves 18315, liveness 7166 |
| 49268 | 32100 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3695 | 627 | 91 | 908 | 908 | constraints 20472, moves 17503, liveness 6990 |
| 46171 | 31536 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3549 | 578 | 84 | 833 | 833 | constraints 18882, moves 16504, liveness 6643 |
| 44532 | 29227 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3490 | 591 | 86 | 854 | 854 | constraints 18418, moves 15734, liveness 6390 |
| 43867 | 34752 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3991 | 618 | 72 | 901 | 901 | constraints 20267, moves 16017, walk 3534 |
| 43189 | 30099 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3561 | 601 | 86 | 873 | 873 | constraints 18440, moves 15918, liveness 4769 |
| 41634 | 29010 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3341 | 542 | 79 | 779 | 779 | constraints 17034, moves 14785, liveness 5973 |
| 40289 | 26600 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3285 | 555 | 81 | 800 | 800 | constraints 16637, moves 14110, liveness 5855 |
| 38974 | 31466 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3768 | 582 | 68 | 847 | 847 | constraints 17867, moves 14211, walk 3215 |
| 37764 | 27002 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3359 | 565 | 81 | 819 | 819 | constraints 15821, moves 14005, liveness 4286 |
| 36948 | 26080 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3133 | 506 | 74 | 725 | 725 | constraints 14996, moves 13031, liveness 5417 |
| 36085 | 23890 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3080 | 519 | 76 | 746 | 746 | constraints 14849, moves 12474, liveness 5353 |
| 34941 | 28019 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3545 | 546 | 64 | 793 | 793 | constraints 16088, moves 12532, walk 2880 |
| 34805 | 24194 | axum | `axum::handler::<F>::call` | 3441 | 583 | 80 | 858 | 858 | constraints 14619, moves 13927, walk 2851 |
| 34333 | 24664 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3157 | 529 | 76 | 765 | 765 | constraints 14388, moves 12651, liveness 3963 |
| 33123 | 23506 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12)>>::call` | 2925 | 470 | 69 | 671 | 671 | constraints 13379, moves 11570, liveness 4984 |
| 32816 | 169050 | axum | `axum_core::extract::tuple::<(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>::from_request` | 3732 | 576 | 62 | 832 | 831 | constraints 14203, moves 11234, walk 3435 |
| 32035 | 21414 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12)>>::call` | 2875 | 483 | 71 | 692 | 692 | constraints 13112, moves 11027, liveness 4843 |
| 31743 | 22012 | axum | `axum::handler::<F>::call` | 3239 | 547 | 75 | 804 | 804 | constraints 13315, moves 12569, walk 2612 |
| 31487 | 146138 | axum | `axum_core::extract::tuple::<(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>::from_request_parts` | 3293 | 565 | 48 | 844 | 842 | constraints 13955, moves 10969, walk 3265 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 50430 | 34959 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3757 | 614 | 89 | 887 | 887 | constraints 20723, moves 18311, liveness 7213 |
| 48874 | 32100 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3695 | 627 | 91 | 908 | 908 | constraints 20430, moves 17533, liveness 6966 |
| 45301 | 31536 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3549 | 578 | 84 | 833 | 833 | constraints 18581, moves 16350, liveness 6575 |
| 44028 | 29227 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3490 | 591 | 86 | 854 | 854 | constraints 18332, moves 15670, liveness 6382 |
| 43766 | 30099 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3561 | 601 | 86 | 873 | 873 | constraints 18678, moves 16471, liveness 4892 |
| 43331 | 34752 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3991 | 618 | 72 | 901 | 901 | constraints 20083, moves 16038, walk 3480 |
| 41010 | 29010 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3341 | 542 | 79 | 779 | 779 | constraints 16808, moves 14712, liveness 5922 |
| 40065 | 26600 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3285 | 555 | 81 | 800 | 800 | constraints 16542, moves 14116, liveness 5821 |
| 38585 | 31466 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3768 | 582 | 68 | 847 | 847 | constraints 17905, moves 14126, walk 3164 |
| 37403 | 27002 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3359 | 565 | 81 | 819 | 819 | constraints 15738, moves 14016, liveness 4285 |
| 36664 | 26080 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3133 | 506 | 74 | 725 | 725 | constraints 14945, moves 13049, liveness 5459 |
| 35716 | 23890 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3080 | 519 | 76 | 746 | 746 | constraints 14834, moves 12475, liveness 5283 |
| 34462 | 28019 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3545 | 546 | 64 | 793 | 793 | constraints 15855, moves 12544, walk 2916 |
| 34436 | 24194 | axum | `axum::handler::<F>::call` | 3441 | 583 | 80 | 858 | 858 | constraints 14497, moves 13966, walk 2850 |
| 33771 | 24664 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3157 | 529 | 76 | 765 | 765 | constraints 14266, moves 12481, liveness 3986 |
| 32862 | 23506 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12)>>::call` | 2925 | 470 | 69 | 671 | 671 | constraints 13312, moves 11505, liveness 5134 |
| 32349 | 169050 | axum | `axum_core::extract::tuple::<(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>::from_request` | 3732 | 576 | 62 | 832 | 831 | constraints 14158, moves 11272, walk 3374 |
| 31780 | 21414 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12)>>::call` | 2875 | 483 | 71 | 692 | 692 | constraints 13034, moves 10974, liveness 4917 |
| 31103 | 22012 | axum | `axum::handler::<F>::call` | 3239 | 547 | 75 | 804 | 804 | constraints 13063, moves 12578, walk 2610 |
| 31053 | 146138 | axum | `axum_core::extract::tuple::<(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>::from_request_parts` | 3293 | 565 | 48 | 844 | 842 | constraints 13835, moves 10960, walk 3237 |
