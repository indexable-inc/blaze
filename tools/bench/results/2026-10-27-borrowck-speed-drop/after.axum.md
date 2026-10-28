| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| axum | polonius | 1616 | 7165 | 3725 | 1013 | 0 | 27.2% | 8108 | 80 | 9732 | 14789 |
| axum | nll | 1616 | 7165 | 3725 | 945 | 0 | 25.4% | 8108 | 66 | 9321 | 14256 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 475 | 20 | 3 | 9 | 292 | 9 | 0 | 106 | 29 | 1 | 45 |
| nll | 454 | 21 | 3 | 9 | 283 | 8 | 0 | 100 | 2 | 1 | 43 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 14789 | 34201 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3757 | 614 | 89 | 887 | 887 | liveness 7167, constraints 5310, moves 1097 |
| 14609 | 17464 | axum | `axum_macros::from_request::extract_fields` | 11070 | 4074 | 1063 | 3174 | 3174 | liveness 5089, constraints 3437, moves 2230 |
| 13855 | 31314 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3695 | 627 | 91 | 908 | 908 | liveness 6793, constraints 4806, moves 1056 |
| 13506 | 31825 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3549 | 578 | 84 | 833 | 833 | liveness 6329, constraints 5008, moves 1037 |
| 12991 | 28492 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3490 | 591 | 86 | 854 | 854 | liveness 6364, constraints 4483, moves 1009 |
| 12712 | 28115 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3341 | 542 | 79 | 779 | 779 | liveness 5869, constraints 4737, moves 954 |
| 12318 | 96818 | axum | `axum::serve::handle_connection` | 3440 | 1155 | 57 | 1231 | 1212 | constraints 7870, liveness 1702, moves 905 |
| 12223 | 26545 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3285 | 555 | 81 | 800 | 800 | liveness 5772, constraints 4415, moves 944 |
| 11468 | 25504 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3133 | 506 | 74 | 725 | 725 | liveness 5216, constraints 4303, moves 891 |
| 11221 | 23410 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3080 | 519 | 76 | 746 | 746 | liveness 5284, constraints 3998, moves 893 |
| 10728 | 23220 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12)>>::call` | 2925 | 470 | 69 | 671 | 671 | liveness 4809, constraints 4092, moves 862 |
| 10704 | 33678 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3991 | 618 | 72 | 901 | 901 | constraints 5426, liveness 2942, moves 1186 |
| 10656 | 29651 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3561 | 601 | 86 | 873 | 873 | liveness 4580, constraints 3745, moves 1088 |
| 10636 | 11908 | axum | `axum_macros::from_request::impl_struct_by_extracting_all_at_once` | 8125 | 3371 | 866 | 2681 | 2681 | liveness 3792, constraints 2511, moves 1537 |
| 10238 | 20957 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12)>>::call` | 2875 | 483 | 71 | 692 | 692 | liveness 4762, constraints 3713, moves 808 |
| 9960 | 30371 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3768 | 582 | 68 | 847 | 847 | constraints 5089, liveness 2657, moves 1083 |
| 9732 | 26898 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3359 | 565 | 81 | 819 | 819 | liveness 4237, constraints 3397, moves 941 |
| 9541 | 20074 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11)>>::call` | 2717 | 434 | 64 | 617 | 617 | liveness 4185, constraints 3711, moves 770 |
| 9523 | 18597 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11)>>::call` | 2670 | 447 | 66 | 638 | 638 | liveness 4293, constraints 3498, moves 760 |
| 9386 | 27330 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3545 | 546 | 64 | 793 | 793 | constraints 4678, liveness 2543, moves 1006 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 14256 | 34201 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3757 | 614 | 89 | 887 | 887 | liveness 7072, constraints 5195, moves 1112 |
| 13917 | 17464 | axum | `axum_macros::from_request::extract_fields` | 11070 | 4074 | 1063 | 3174 | 3174 | liveness 5044, constraints 3380, moves 2186 |
| 13563 | 31314 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3695 | 627 | 91 | 908 | 908 | liveness 6861, constraints 4721, moves 1128 |
| 13053 | 31825 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3549 | 578 | 84 | 833 | 833 | liveness 6325, constraints 4905, moves 1014 |
| 12521 | 28492 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3490 | 591 | 86 | 854 | 854 | liveness 6208, constraints 4460, moves 1044 |
| 12334 | 28115 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3341 | 542 | 79 | 779 | 779 | liveness 5775, constraints 4685, moves 997 |
| 11523 | 26545 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3285 | 555 | 81 | 800 | 800 | liveness 5669, constraints 4197, moves 899 |
| 11398 | 96818 | axum | `axum::serve::handle_connection` | 3440 | 1155 | 57 | 1231 | 1212 | constraints 7752, liveness 1659, moves 898 |
| 11021 | 25504 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3133 | 506 | 74 | 725 | 725 | liveness 5192, constraints 4231, moves 856 |
| 10737 | 23410 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3080 | 519 | 76 | 746 | 746 | liveness 5206, constraints 3903, moves 860 |
| 10399 | 23220 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12)>>::call` | 2925 | 470 | 69 | 671 | 671 | liveness 4828, constraints 3988, moves 794 |
| 10305 | 33678 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3991 | 618 | 72 | 901 | 901 | constraints 5406, liveness 2790, moves 1207 |
| 10021 | 11908 | axum | `axum_macros::from_request::impl_struct_by_extracting_all_at_once` | 8125 | 3371 | 866 | 2681 | 2681 | liveness 3764, constraints 2488, moves 1545 |
| 9874 | 20957 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12)>>::call` | 2875 | 483 | 71 | 692 | 692 | liveness 4777, constraints 3543, moves 871 |
| 9865 | 29651 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3561 | 601 | 86 | 873 | 873 | liveness 4506, constraints 3511, moves 1028 |
| 9775 | 30371 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3768 | 582 | 68 | 847 | 847 | constraints 5016, liveness 2608, moves 1119 |
| 9321 | 26898 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3359 | 565 | 81 | 819 | 819 | liveness 4262, constraints 3317, moves 899 |
| 9118 | 20074 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11)>>::call` | 2717 | 434 | 64 | 617 | 617 | liveness 4162, constraints 3566, moves 734 |
| 9024 | 18597 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11)>>::call` | 2670 | 447 | 66 | 638 | 638 | liveness 4207, constraints 3371, moves 734 |
| 9003 | 27330 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3545 | 546 | 64 | 793 | 793 | constraints 4636, liveness 2394, moves 1028 |
