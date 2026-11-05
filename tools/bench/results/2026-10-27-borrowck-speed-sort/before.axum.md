| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| axum | polonius | 1616 | 7242 | 3729 | 1024 | 0 | 27.5% | 8117 | 83 | 9631 | 14837 |
| axum | nll | 1616 | 7242 | 3729 | 954 | 0 | 25.6% | 8117 | 67 | 9274 | 14262 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 483 | 20 | 3 | 10 | 292 | 9 | 0 | 107 | 29 | 1 | 45 |
| nll | 462 | 21 | 3 | 9 | 282 | 8 | 0 | 101 | 2 | 1 | 43 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 14837 | 34478 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3757 | 614 | 89 | 887 | 887 | liveness 7114, constraints 5408, moves 1102 |
| 14584 | 17707 | axum | `axum_macros::from_request::extract_fields` | 11070 | 4074 | 1063 | 3174 | 3174 | liveness 4864, constraints 3512, moves 2282 |
| 14074 | 31540 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3695 | 627 | 91 | 908 | 908 | liveness 6848, constraints 4907, moves 1086 |
| 13535 | 31070 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3549 | 578 | 84 | 833 | 833 | liveness 6353, constraints 5000, moves 1023 |
| 12888 | 28139 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3490 | 591 | 86 | 854 | 854 | liveness 6192, constraints 4512, moves 1009 |
| 12570 | 28606 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3341 | 542 | 79 | 779 | 779 | liveness 5752, constraints 4728, moves 941 |
| 12451 | 95855 | axum | `axum::serve::handle_connection` | 3440 | 1155 | 57 | 1231 | 1212 | constraints 7981, liveness 1703, moves 932 |
| 11920 | 25749 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3285 | 555 | 81 | 800 | 800 | liveness 5641, constraints 4252, moves 925 |
| 11690 | 25395 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3133 | 506 | 74 | 725 | 725 | liveness 5354, constraints 4361, moves 923 |
| 11055 | 23146 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3080 | 519 | 76 | 746 | 746 | liveness 5169, constraints 3932, moves 889 |
| 10808 | 11997 | axum | `axum_macros::from_request::impl_struct_by_extracting_all_at_once` | 8125 | 3371 | 866 | 2681 | 2681 | liveness 3836, constraints 2615, moves 1602 |
| 10770 | 33424 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3991 | 618 | 72 | 901 | 901 | constraints 5472, liveness 2913, moves 1187 |
| 10728 | 23000 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12)>>::call` | 2925 | 470 | 69 | 671 | 671 | liveness 4866, constraints 4065, moves 820 |
| 10536 | 29332 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3561 | 601 | 86 | 873 | 873 | liveness 4588, constraints 3631, moves 1133 |
| 10147 | 20493 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12)>>::call` | 2875 | 483 | 71 | 692 | 692 | liveness 4720, constraints 3652, moves 821 |
| 10126 | 30510 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3768 | 582 | 68 | 847 | 847 | constraints 5152, liveness 2712, moves 1106 |
| 9631 | 26203 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3359 | 565 | 81 | 819 | 819 | liveness 4128, constraints 3369, moves 969 |
| 9625 | 19950 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11)>>::call` | 2717 | 434 | 64 | 617 | 617 | liveness 4207, constraints 3690, moves 735 |
| 9429 | 27490 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3545 | 546 | 64 | 793 | 793 | constraints 4770, liveness 2520, moves 1045 |
| 9330 | 18685 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11)>>::call` | 2670 | 447 | 66 | 638 | 638 | liveness 4342, constraints 3379, moves 729 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 14262 | 34478 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3757 | 614 | 89 | 887 | 887 | liveness 7010, constraints 5282, moves 1088 |
| 13908 | 17707 | axum | `axum_macros::from_request::extract_fields` | 11070 | 4074 | 1063 | 3174 | 3174 | liveness 4890, constraints 3521, moves 2195 |
| 13685 | 31540 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3695 | 627 | 91 | 908 | 908 | liveness 6873, constraints 4812, moves 1116 |
| 13057 | 31070 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3549 | 578 | 84 | 833 | 833 | liveness 6273, constraints 4946, moves 1013 |
| 12585 | 28139 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3490 | 591 | 86 | 854 | 854 | liveness 6252, constraints 4460, moves 1040 |
| 12162 | 28606 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3341 | 542 | 79 | 779 | 779 | liveness 5703, constraints 4696, moves 916 |
| 11571 | 95855 | axum | `axum::serve::handle_connection` | 3440 | 1155 | 57 | 1231 | 1212 | constraints 7901, liveness 1654, moves 916 |
| 11518 | 25749 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3285 | 555 | 81 | 800 | 800 | liveness 5623, constraints 4196, moves 908 |
| 11224 | 25395 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3133 | 506 | 74 | 725 | 725 | liveness 5231, constraints 4293, moves 933 |
| 10562 | 23146 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3080 | 519 | 76 | 746 | 746 | liveness 5074, constraints 3882, moves 840 |
| 10415 | 23000 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12)>>::call` | 2925 | 470 | 69 | 671 | 671 | liveness 4781, constraints 4025, moves 819 |
| 10376 | 33424 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3991 | 618 | 72 | 901 | 901 | constraints 5407, liveness 2804, moves 1223 |
| 10178 | 11997 | axum | `axum_macros::from_request::impl_struct_by_extracting_all_at_once` | 8125 | 3371 | 866 | 2681 | 2681 | liveness 3788, constraints 2595, moves 1547 |
| 9956 | 30510 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3768 | 582 | 68 | 847 | 847 | constraints 5072, liveness 2737, moves 1132 |
| 9920 | 29332 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3561 | 601 | 86 | 873 | 873 | liveness 4518, constraints 3536, moves 1037 |
| 9660 | 20493 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12)>>::call` | 2875 | 483 | 71 | 692 | 692 | liveness 4622, constraints 3566, moves 785 |
| 9274 | 19950 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11)>>::call` | 2717 | 434 | 64 | 617 | 617 | liveness 4162, constraints 3626, moves 720 |
| 9093 | 26203 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3359 | 565 | 81 | 819 | 819 | liveness 4049, constraints 3298, moves 893 |
| 9056 | 27490 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3545 | 546 | 64 | 793 | 793 | constraints 4708, liveness 2387, moves 1019 |
| 9052 | 18685 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11)>>::call` | 2670 | 447 | 66 | 638 | 638 | liveness 4261, constraints 3358, moves 731 |
