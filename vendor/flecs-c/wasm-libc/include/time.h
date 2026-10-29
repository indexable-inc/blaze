#pragma once
#include <stddef.h>
typedef long long time_t;
typedef long clock_t;
struct timespec { time_t tv_sec; long tv_nsec; };
typedef int clockid_t;
#define CLOCK_REALTIME 0
#define CLOCK_MONOTONIC 1
#define CLOCKS_PER_SEC 1000000L
int clock_gettime(clockid_t id, struct timespec *ts);
time_t time(time_t *t);
clock_t clock(void);
