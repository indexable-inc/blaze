| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| axum | polonius | 1616 | 8116 | 4080 | 3536 | 0 | 86.7% | 5176 | 130 | 47956 | 73929 |
| axum | nll | 1616 | 8116 | 4080 | 3464 | 0 | 84.9% | 5176 | 111 | 47657 | 74334 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 2035 | 23 | 4 | 11 | 336 | 10 | 0 | 820 | 32 | 2 | 235 |
| nll | 2013 | 22 | 4 | 10 | 330 | 9 | 0 | 814 | 3 | 2 | 233 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 73929 | 36895 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3757 | 614 | 89 | 887 | 887 | constraints 41836, moves 19564, liveness 7726 |
| 68463 | 33407 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3695 | 627 | 91 | 908 | 908 | constraints 38184, moves 18163, liveness 7457 |
| 66009 | 33130 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3549 | 578 | 84 | 833 | 833 | constraints 37071, moves 17263, liveness 7177 |
| 63145 | 41198 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3991 | 618 | 72 | 901 | 901 | constraints 37960, moves 17290, walk 3715 |
| 61540 | 31189 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3561 | 601 | 86 | 873 | 873 | constraints 35252, moves 16475, liveness 5399 |
| 60343 | 29895 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3490 | 591 | 86 | 854 | 854 | constraints 33616, moves 15987, liveness 6654 |
| 59995 | 33223 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3768 | 582 | 68 | 847 | 847 | constraints 36325, moves 15673, walk 3649 |
| 58891 | 30675 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3341 | 542 | 79 | 779 | 779 | constraints 33015, moves 15394, liveness 6364 |
| 54526 | 27498 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3285 | 555 | 81 | 800 | 800 | constraints 30328, moves 14370, liveness 6028 |
| 54062 | 182128 | axum | `axum_core::extract::tuple::<(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>::from_request` | 3732 | 576 | 62 | 831 | 831 | constraints 34142, moves 12077, walk 3685 |
| 53942 | 29440 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3359 | 565 | 81 | 819 | 819 | constraints 30873, moves 14556, liveness 4601 |
| 53492 | 28082 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3133 | 506 | 74 | 725 | 725 | constraints 29744, moves 13869, liveness 5989 |
| 50662 | 25462 | axum | `axum::handler::<F>::call` | 3441 | 583 | 80 | 858 | 858 | constraints 28649, moves 14768, walk 3158 |
| 50576 | 156300 | axum | `axum_core::extract::tuple::<(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>::from_request_parts` | 3293 | 565 | 48 | 842 | 842 | constraints 32774, moves 11275, walk 3405 |
| 50311 | 25483 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3080 | 519 | 76 | 746 | 746 | constraints 27155, moves 13002, liveness 6028 |
| 48601 | 29637 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3545 | 546 | 64 | 793 | 793 | constraints 28920, moves 13007, walk 3017 |
| 47956 | 26121 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3157 | 529 | 76 | 765 | 765 | constraints 27302, moves 12880, liveness 4216 |
| 47595 | 25026 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12)>>::call` | 2925 | 470 | 69 | 671 | 671 | constraints 26252, moves 12254, liveness 5522 |
| 45596 | 151911 | axum | `axum_core::extract::tuple::<(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>::from_request` | 3503 | 540 | 58 | 778 | 778 | constraints 28648, moves 10259, walk 3179 |
| 44737 | 22812 | axum | `axum::handler::<F>::call` | 3239 | 547 | 75 | 804 | 804 | constraints 25223, moves 13024, liveness 2833 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 74334 | 36895 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3757 | 614 | 89 | 887 | 887 | constraints 42131, moves 19563, liveness 8074 |
| 67231 | 33407 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3695 | 627 | 91 | 908 | 908 | constraints 37828, moves 18056, liveness 7252 |
| 65792 | 33130 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3549 | 578 | 84 | 833 | 833 | constraints 37179, moves 17351, liveness 7080 |
| 63449 | 41198 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3991 | 618 | 72 | 901 | 901 | constraints 38787, moves 17062, walk 3688 |
| 60591 | 29895 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3490 | 591 | 86 | 854 | 854 | constraints 34017, moves 16085, liveness 6681 |
| 60401 | 33223 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3768 | 582 | 68 | 847 | 847 | constraints 36335, moves 16507, walk 3620 |
| 59571 | 31189 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3561 | 601 | 86 | 873 | 873 | constraints 34143, moves 16347, liveness 5177 |
| 58431 | 30675 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3341 | 542 | 79 | 779 | 779 | constraints 32912, moves 15320, liveness 6416 |
| 53935 | 27498 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3285 | 555 | 81 | 800 | 800 | constraints 30233, moves 14286, liveness 5968 |
| 53364 | 182128 | axum | `axum_core::extract::tuple::<(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>::from_request` | 3732 | 576 | 62 | 831 | 831 | constraints 33760, moves 12014, walk 3706 |
| 53345 | 29440 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3359 | 565 | 81 | 819 | 819 | constraints 30629, moves 14539, liveness 4575 |
| 53176 | 28082 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3133 | 506 | 74 | 725 | 725 | constraints 29629, moves 13834, liveness 6112 |
| 50469 | 156300 | axum | `axum_core::extract::tuple::<(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>::from_request_parts` | 3293 | 565 | 48 | 842 | 842 | constraints 32829, moves 11303, walk 3405 |
| 49712 | 25462 | axum | `axum::handler::<F>::call` | 3441 | 583 | 80 | 858 | 858 | constraints 28379, moves 14745, walk 3111 |
| 49223 | 29637 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3545 | 546 | 64 | 793 | 793 | constraints 29260, moves 13342, walk 3088 |
| 48918 | 25483 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3080 | 519 | 76 | 746 | 746 | constraints 27033, moves 13094, liveness 5541 |
| 47657 | 25026 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12)>>::call` | 2925 | 470 | 69 | 671 | 671 | constraints 26367, moves 12249, liveness 5651 |
| 47631 | 26121 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3157 | 529 | 76 | 765 | 765 | constraints 27226, moves 12984, liveness 4180 |
| 44799 | 151911 | axum | `axum_core::extract::tuple::<(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>::from_request` | 3503 | 540 | 58 | 778 | 778 | constraints 28301, moves 10197, walk 3174 |
| 44660 | 130179 | axum | `axum_core::extract::tuple::<(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>::from_request_parts` | 3091 | 531 | 45 | 790 | 790 | constraints 28500, moves 10172, walk 3077 |
