#pragma once
#include <stddef.h>
void *malloc(size_t n);
void *calloc(size_t c, size_t n);
void *realloc(void *p, size_t n);
void free(void *p);
_Noreturn void abort(void);
_Noreturn void exit(int code);
void qsort(void *base, size_t n, size_t sz, int (*cmp)(const void *, const void *));
long strtol(const char *restrict s, char **restrict end, int base);
unsigned long strtoul(const char *restrict s, char **restrict end, int base);
long long strtoll(const char *restrict s, char **restrict end, int base);
unsigned long long strtoull(const char *restrict s, char **restrict end, int base);
double strtod(const char *restrict s, char **restrict end);
float strtof(const char *restrict s, char **restrict end);
double atof(const char *s);
int atoi(const char *s);
long atol(const char *s);
long long atoll(const char *s);
int abs(int x);
long labs(long x);
long long llabs(long long x);
char *getenv(const char *name);
#define EXIT_SUCCESS 0
#define EXIT_FAILURE 1
#define RAND_MAX 0x7fffffff
int rand(void);
