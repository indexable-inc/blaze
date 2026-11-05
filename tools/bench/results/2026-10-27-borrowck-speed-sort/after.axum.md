| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| axum | polonius | 1616 | 7283 | 3771 | 1018 | 0 | 27.0% | 8160 | 83 | 9654 | 14742 |
| axum | nll | 1616 | 7283 | 3771 | 947 | 0 | 25.1% | 8160 | 67 | 9232 | 14186 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 484 | 20 | 3 | 10 | 285 | 9 | 0 | 107 | 29 | 1 | 46 |
| nll | 463 | 20 | 3 | 9 | 274 | 8 | 0 | 100 | 2 | 1 | 44 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 14742 | 34636 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3757 | 614 | 89 | 887 | 887 | liveness 6995, constraints 5400, moves 1131 |
| 14296 | 17789 | axum | `axum_macros::from_request::extract_fields` | 11070 | 4074 | 1063 | 3174 | 3174 | liveness 4552, constraints 3501, moves 2293 |
| 14096 | 32180 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3695 | 627 | 91 | 908 | 908 | liveness 6764, constraints 5029, moves 1062 |
| 13512 | 30939 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3549 | 578 | 84 | 833 | 833 | liveness 6293, constraints 4986, moves 1077 |
| 12981 | 28749 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3490 | 591 | 86 | 854 | 854 | liveness 6174, constraints 4580, moves 1053 |
| 12827 | 28554 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3341 | 542 | 79 | 779 | 779 | liveness 5795, constraints 4921, moves 954 |
| 12499 | 97123 | axum | `axum::serve::handle_connection` | 3440 | 1155 | 57 | 1231 | 1212 | constraints 8042, liveness 1668, moves 914 |
| 11867 | 25836 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3285 | 555 | 81 | 800 | 800 | liveness 5622, constraints 4252, moves 908 |
| 11604 | 25596 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3133 | 506 | 74 | 725 | 725 | liveness 5293, constraints 4412, moves 863 |
| 11180 | 23613 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3080 | 519 | 76 | 746 | 746 | liveness 5279, constraints 3993, moves 857 |
| 10708 | 23073 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12)>>::call` | 2925 | 470 | 69 | 671 | 671 | liveness 4715, constraints 4081, moves 873 |
| 10703 | 33867 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3991 | 618 | 72 | 901 | 901 | constraints 5526, liveness 2753, moves 1218 |
| 10528 | 12050 | axum | `axum_macros::from_request::impl_struct_by_extracting_all_at_once` | 8125 | 3371 | 866 | 2681 | 2681 | liveness 3630, constraints 2625, moves 1533 |
| 10274 | 29229 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3561 | 601 | 86 | 873 | 873 | liveness 4452, constraints 3579, moves 1068 |
| 10216 | 20812 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12)>>::call` | 2875 | 483 | 71 | 692 | 692 | liveness 4762, constraints 3683, moves 813 |
| 9877 | 30537 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3768 | 582 | 68 | 847 | 847 | constraints 5128, liveness 2529, moves 1102 |
| 9654 | 26563 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3359 | 565 | 81 | 819 | 819 | liveness 4113, constraints 3376, moves 967 |
| 9432 | 20166 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11)>>::call` | 2717 | 434 | 64 | 617 | 617 | liveness 4102, constraints 3710, moves 739 |
| 9215 | 27433 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3545 | 546 | 64 | 793 | 793 | constraints 4784, liveness 2295, moves 1047 |
| 9174 | 18883 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11)>>::call` | 2670 | 447 | 66 | 638 | 638 | liveness 4135, constraints 3440, moves 719 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 14186 | 34636 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3757 | 614 | 89 | 887 | 887 | liveness 6834, constraints 5318, moves 1116 |
| 13691 | 32180 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3695 | 627 | 91 | 908 | 908 | liveness 6836, constraints 4907, moves 1091 |
| 13634 | 17789 | axum | `axum_macros::from_request::extract_fields` | 11070 | 4074 | 1063 | 3174 | 3174 | liveness 4684, constraints 3495, moves 2247 |
| 13091 | 30939 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3549 | 578 | 84 | 833 | 833 | liveness 6327, constraints 4863, moves 1058 |
| 12386 | 28749 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3490 | 591 | 86 | 854 | 854 | liveness 6082, constraints 4509, moves 982 |
| 12059 | 28554 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3341 | 542 | 79 | 779 | 779 | liveness 5649, constraints 4628, moves 904 |
| 11490 | 97123 | axum | `axum::serve::handle_connection` | 3440 | 1155 | 57 | 1231 | 1212 | constraints 7820, liveness 1621, moves 904 |
| 11406 | 25836 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3285 | 555 | 81 | 800 | 800 | liveness 5539, constraints 4203, moves 888 |
| 11152 | 25596 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3133 | 506 | 74 | 725 | 725 | liveness 5192, constraints 4294, moves 855 |
| 10657 | 23613 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3080 | 519 | 76 | 746 | 746 | liveness 5160, constraints 3905, moves 815 |
| 10278 | 33867 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3991 | 618 | 72 | 901 | 901 | constraints 5463, liveness 2706, moves 1202 |
| 10273 | 23073 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12)>>::call` | 2925 | 470 | 69 | 671 | 671 | liveness 4643, constraints 4077, moves 839 |
| 10000 | 12050 | axum | `axum_macros::from_request::impl_struct_by_extracting_all_at_once` | 8125 | 3371 | 866 | 2681 | 2681 | liveness 3584, constraints 2556, moves 1560 |
| 9645 | 29229 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3561 | 601 | 86 | 873 | 873 | liveness 4319, constraints 3495, moves 999 |
| 9635 | 20812 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12)>>::call` | 2875 | 483 | 71 | 692 | 692 | liveness 4612, constraints 3571, moves 777 |
| 9628 | 30537 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3768 | 582 | 68 | 847 | 847 | constraints 5111, liveness 2449, moves 1063 |
| 9232 | 20166 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11)>>::call` | 2717 | 434 | 64 | 617 | 617 | liveness 4093, constraints 3651, moves 748 |
| 9049 | 26563 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3359 | 565 | 81 | 819 | 819 | liveness 3994, constraints 3309, moves 917 |
| 8881 | 27433 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3545 | 546 | 64 | 793 | 793 | constraints 4691, liveness 2295, moves 1027 |
| 8875 | 18883 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11)>>::call` | 2670 | 447 | 66 | 638 | 638 | liveness 4107, constraints 3345, moves 717 |
