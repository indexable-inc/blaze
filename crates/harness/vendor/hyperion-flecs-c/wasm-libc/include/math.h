#pragma once
#define isnan(x) __builtin_isnan(x)
#define isinf(x) __builtin_isinf(x)
#define isfinite(x) __builtin_isfinite(x)
#define NAN __builtin_nan("")
#define INFINITY __builtin_inf()
#define HUGE_VAL __builtin_huge_val()
double fabs(double x);
float fabsf(float x);
double floor(double x);
double ceil(double x);
double fmod(double x, double y);
double sqrt(double x);
double pow(double x, double y);
double round(double x);
double trunc(double x);
double log10(double x);
