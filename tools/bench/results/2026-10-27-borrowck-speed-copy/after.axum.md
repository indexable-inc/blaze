| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| axum | polonius | 1616 | 7374 | 3886 | 1699 | 0 | 43.7% | 8299 | 91 | 19302 | 31971 |
| axum | nll | 1616 | 7374 | 3886 | 1642 | 0 | 42.3% | 8299 | 74 | 19011 | 31696 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 490 | 22 | 4 | 10 | 291 | 9 | 0 | 105 | 29 | 1 | 712 |
| nll | 476 | 23 | 3 | 10 | 285 | 9 | 0 | 102 | 2 | 1 | 708 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 31971 | 35702 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3757 | 614 | 89 | 887 | 887 | walk 17680, liveness 6965, constraints 5467 |
| 30088 | 32301 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3695 | 627 | 91 | 908 | 908 | walk 16515, liveness 6768, constraints 4945 |
| 28802 | 32233 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3549 | 578 | 84 | 833 | 833 | walk 15607, liveness 6290, constraints 5100 |
| 27470 | 29510 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3490 | 591 | 86 | 854 | 854 | walk 14857, liveness 6213, constraints 4619 |
| 26470 | 29673 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3341 | 542 | 79 | 779 | 779 | walk 14085, liveness 5861, constraints 4840 |
| 25121 | 34418 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3991 | 618 | 72 | 901 | 901 | walk 14888, constraints 5575, liveness 2770 |
| 24937 | 26750 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3285 | 555 | 81 | 800 | 800 | walk 13269, liveness 5616, constraints 4331 |
| 24837 | 30545 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3561 | 601 | 86 | 873 | 873 | walk 14809, liveness 4475, constraints 3678 |
| 23502 | 26386 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3133 | 506 | 74 | 725 | 725 | walk 12154, liveness 5309, constraints 4451 |
| 22966 | 31441 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3768 | 582 | 68 | 847 | 847 | walk 13319, constraints 5206, liveness 2533 |
| 22963 | 24660 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3080 | 519 | 76 | 746 | 746 | walk 11964, liveness 5333, constraints 4085 |
| 22477 | 27505 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3359 | 565 | 81 | 819 | 819 | walk 13119, liveness 4169, constraints 3428 |
| 21438 | 23756 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12)>>::call` | 2925 | 470 | 69 | 671 | 671 | walk 10841, liveness 4715, constraints 4328 |
| 20506 | 28250 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3545 | 546 | 64 | 793 | 793 | walk 11726, constraints 4792, liveness 2312 |
| 20027 | 24660 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3157 | 529 | 76 | 765 | 765 | walk 11493, liveness 3702, constraints 3232 |
| 20019 | 21515 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12)>>::call` | 2875 | 483 | 71 | 692 | 692 | walk 10146, liveness 4700, constraints 3710 |
| 19302 | 24292 | axum | `axum::handler::<F>::call` | 3441 | 583 | 80 | 858 | 858 | walk 13105, liveness 2270, constraints 2213 |
| 18889 | 21207 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11)>>::call` | 2717 | 434 | 64 | 617 | 617 | walk 9381, liveness 4292, constraints 3796 |
| 18534 | 25443 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3322 | 510 | 60 | 739 | 739 | walk 10298, constraints 4452, liveness 2079 |
| 18318 | 173518 | axum | `axum_core::extract::tuple::<(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>::from_request` | 3732 | 576 | 62 | 832 | 831 | walk 11431, constraints 2677, liveness 2337 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 31696 | 35702 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3757 | 614 | 89 | 887 | 887 | walk 17661, liveness 6928, constraints 5489 |
| 29903 | 32301 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3695 | 627 | 91 | 908 | 908 | walk 16495, liveness 6800, constraints 4919 |
| 28464 | 32233 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3549 | 578 | 84 | 833 | 833 | walk 15588, liveness 6291, constraints 5057 |
| 27118 | 29510 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3490 | 591 | 86 | 854 | 854 | walk 14792, liveness 6229, constraints 4609 |
| 26107 | 29673 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3341 | 542 | 79 | 779 | 779 | walk 14066, liveness 5856, constraints 4776 |
| 24859 | 34418 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3991 | 618 | 72 | 901 | 901 | walk 14874, constraints 5532, liveness 2780 |
| 24627 | 26750 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3285 | 555 | 81 | 800 | 800 | walk 13226, liveness 5678, constraints 4318 |
| 24484 | 30545 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>>::call` | 3561 | 601 | 86 | 873 | 873 | walk 14772, liveness 4437, constraints 3658 |
| 23178 | 26386 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3133 | 506 | 74 | 725 | 725 | walk 12171, liveness 5285, constraints 4402 |
| 22668 | 31441 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3768 | 582 | 68 | 847 | 847 | walk 13277, constraints 5178, liveness 2454 |
| 22583 | 24660 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3080 | 519 | 76 | 746 | 746 | walk 11944, liveness 5190, constraints 4080 |
| 22106 | 27505 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15)>>::call` | 3359 | 565 | 81 | 819 | 819 | walk 13145, liveness 4102, constraints 3387 |
| 21006 | 23756 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12)>>::call` | 2925 | 470 | 69 | 671 | 671 | walk 10779, liveness 4707, constraints 4187 |
| 20458 | 28250 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3545 | 546 | 64 | 793 | 793 | walk 11727, constraints 4769, liveness 2341 |
| 19811 | 24660 | axum | `axum::middleware::from_fn::<FromFn<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14)>>::call` | 3157 | 529 | 76 | 765 | 765 | walk 11484, liveness 3754, constraints 3178 |
| 19803 | 21515 | axum | `axum::middleware::map_response::<MapResponse<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12)>>::call` | 2875 | 483 | 71 | 692 | 692 | walk 10139, liveness 4711, constraints 3680 |
| 19011 | 24292 | axum | `axum::handler::<F>::call` | 3441 | 583 | 80 | 858 | 858 | walk 13102, liveness 2289, constraints 2169 |
| 18534 | 21207 | axum | `axum::middleware::map_request::<MapRequest<F, S, I, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11)>>::call` | 2717 | 434 | 64 | 617 | 617 | walk 9323, liveness 4235, constraints 3761 |
| 18353 | 173518 | axum | `axum_core::extract::tuple::<(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16)>::from_request` | 3732 | 576 | 62 | 832 | 831 | walk 11353, constraints 2665, liveness 2455 |
| 18198 | 25443 | axum | `axum::error_handling::<HandleError<S, F, (T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13)>>::call` | 3322 | 510 | 60 | 739 | 739 | walk 10303, constraints 4450, liveness 2046 |
