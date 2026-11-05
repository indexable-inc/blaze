#pragma once
#include <stddef.h>
#include <stdarg.h>
typedef struct stub_FILE FILE;
extern FILE *stdin;
extern FILE *stdout;
extern FILE *stderr;
#define EOF (-1)
#define SEEK_SET 0
#define SEEK_CUR 1
#define SEEK_END 2
int vsnprintf(char *restrict buf, size_t n, const char *restrict fmt, va_list ap);
int snprintf(char *restrict buf, size_t n, const char *restrict fmt, ...);
int sprintf(char *restrict buf, const char *restrict fmt, ...);
int vsprintf(char *restrict buf, const char *restrict fmt, va_list ap);
int fprintf(FILE *restrict f, const char *restrict fmt, ...);
int vfprintf(FILE *restrict f, const char *restrict fmt, va_list ap);
int printf(const char *restrict fmt, ...);
int fputs(const char *restrict s, FILE *restrict f);
int fputc(int c, FILE *f);
int putc(int c, FILE *f);
int puts(const char *s);
int putchar(int c);
int fflush(FILE *f);
FILE *fopen(const char *restrict path, const char *restrict mode);
int fclose(FILE *f);
size_t fread(void *restrict p, size_t sz, size_t n, FILE *restrict f);
size_t fwrite(const void *restrict p, size_t sz, size_t n, FILE *restrict f);
int fseek(FILE *f, long off, int whence);
long ftell(FILE *f);
void rewind(FILE *f);
int fgetc(FILE *f);
int getc(FILE *f);
int sscanf(const char *restrict s, const char *restrict fmt, ...);
