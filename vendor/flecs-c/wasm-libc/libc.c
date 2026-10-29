/* The few C library functions flecs calls, for wasm32-unknown-unknown (which has no libc).
 * Adapted from den (github.com/indexable-inc/den, vendor/hyperion-flecs-c/wasm-libc), MIT.
 *
 * Single-threaded. Not here, because Rust provides them (crates/flecs-c/src/lib.rs): malloc,
 * calloc, realloc, free, abort, exit, and strtod/strtof/atof, which must round exactly like a
 * native libc so a script's number literals give the same bits on every target. memcpy, memset,
 * memmove, memcmp and the math functions come from Rust's compiler-builtins. Files and backtraces
 * do not exist: fopen fails and backtrace finds no frames. Output to stdout and stderr is dropped;
 * flecs logs through its OS API, which the Rust side captures. */

#include <stddef.h>
#include <stdint.h>
#include <stdarg.h>
#include <stdbool.h>
#include <limits.h>

#include <ctype.h>
#include <errno.h>
#include <execinfo.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>
#include <time.h>

int errno;

struct stub_FILE { int unused; };
static struct stub_FILE stub_stdin, stub_stdout, stub_stderr;
FILE *stdin = &stub_stdin;
FILE *stdout = &stub_stdout;
FILE *stderr = &stub_stderr;

/* ---- strings ---- */

size_t strlen(const char *s) {
    const char *p = s;
    while (*p) p++;
    return (size_t)(p - s);
}

size_t strnlen(const char *s, size_t n) {
    size_t i = 0;
    while (i < n && s[i]) i++;
    return i;
}

int strcmp(const char *a, const char *b) {
    while (*a && *a == *b) { a++; b++; }
    return (unsigned char)*a - (unsigned char)*b;
}

int strncmp(const char *a, const char *b, size_t n) {
    for (; n; n--, a++, b++) {
        if (*a != *b || !*a) return (unsigned char)*a - (unsigned char)*b;
    }
    return 0;
}

int strcasecmp(const char *a, const char *b) {
    while (*a && tolower((unsigned char)*a) == tolower((unsigned char)*b)) { a++; b++; }
    return tolower((unsigned char)*a) - tolower((unsigned char)*b);
}

int strncasecmp(const char *a, const char *b, size_t n) {
    for (; n; n--, a++, b++) {
        int x = tolower((unsigned char)*a), y = tolower((unsigned char)*b);
        if (x != y || !*a) return x - y;
    }
    return 0;
}

char *strcpy(char *restrict d, const char *restrict s) {
    char *r = d;
    while ((*d++ = *s++)) {}
    return r;
}

char *strncpy(char *restrict d, const char *restrict s, size_t n) {
    size_t i = 0;
    for (; i < n && s[i]; i++) d[i] = s[i];
    for (; i < n; i++) d[i] = 0;
    return d;
}

char *strcat(char *restrict d, const char *restrict s) {
    strcpy(d + strlen(d), s);
    return d;
}

char *strncat(char *restrict d, const char *restrict s, size_t n) {
    char *e = d + strlen(d);
    size_t i = 0;
    for (; i < n && s[i]; i++) e[i] = s[i];
    e[i] = 0;
    return d;
}

char *strchr(const char *s, int c) {
    for (;; s++) {
        if (*s == (char)c) return (char *)s;
        if (!*s) return NULL;
    }
}

char *strrchr(const char *s, int c) {
    const char *found = NULL;
    for (;; s++) {
        if (*s == (char)c) found = s;
        if (!*s) return (char *)found;
    }
}

char *strstr(const char *h, const char *n) {
    size_t len = strlen(n);
    if (!len) return (char *)h;
    for (; *h; h++) {
        if (*h == *n && !strncmp(h, n, len)) return (char *)h;
    }
    return NULL;
}

void *memchr(const void *s, int c, size_t n) {
    const unsigned char *p = s;
    for (; n; n--, p++) {
        if (*p == (unsigned char)c) return (void *)p;
    }
    return NULL;
}

size_t strspn(const char *s, const char *a) {
    size_t i = 0;
    while (s[i] && strchr(a, s[i])) i++;
    return i;
}

size_t strcspn(const char *s, const char *r) {
    size_t i = 0;
    while (s[i] && !strchr(r, s[i])) i++;
    return i;
}

char *strdup(const char *s) {
    size_t n = strlen(s) + 1;
    char *d = malloc(n);
    if (d) memcpy(d, s, n);
    return d;
}

char *strerror(int e) {
    (void)e;
    return (char *)"error";
}

/* ---- ctype (ASCII) ---- */

int isdigit(int c) { return c >= '0' && c <= '9'; }
int isupper(int c) { return c >= 'A' && c <= 'Z'; }
int islower(int c) { return c >= 'a' && c <= 'z'; }
int isalpha(int c) { return isupper(c) || islower(c); }
int isalnum(int c) { return isalpha(c) || isdigit(c); }
int isspace(int c) { return c == ' ' || (c >= '\t' && c <= '\r'); }
int isxdigit(int c) { return isdigit(c) || (c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F'); }
int iscntrl(int c) { return (c >= 0 && c < 32) || c == 127; }
int isprint(int c) { return c >= 32 && c < 127; }
int isgraph(int c) { return c > 32 && c < 127; }
int ispunct(int c) { return isgraph(c) && !isalnum(c); }
int toupper(int c) { return islower(c) ? c - 32 : c; }
int tolower(int c) { return isupper(c) ? c + 32 : c; }

/* ---- numbers ---- */

/* The digits of an integer, as strtoull reads them. `*over` is set when the magnitude does not fit 64 bits; the value
 * is then ULLONG_MAX. */
static unsigned long long parse_unsigned(const char *s, char **end, int base, bool *neg, bool *over) {
    const char *p = s;
    while (isspace((unsigned char)*p)) p++;
    *neg = false;
    *over = false;
    if (*p == '-' || *p == '+') { *neg = *p == '-'; p++; }
    if ((base == 0 || base == 16) && p[0] == '0' && (p[1] == 'x' || p[1] == 'X')) {
        p += 2;
        base = 16;
    } else if (base == 0 && p[0] == '0') {
        base = 8;
    } else if (base == 0) {
        base = 10;
    }
    unsigned long long v = 0;
    const char *digits = p;
    for (;; p++) {
        int d;
        if (isdigit((unsigned char)*p)) d = *p - '0';
        else if (*p >= 'a' && *p <= 'z') d = *p - 'a' + 10;
        else if (*p >= 'A' && *p <= 'Z') d = *p - 'A' + 10;
        else break;
        if (d >= base) break;
        if (v > (ULLONG_MAX - (unsigned long long)d) / (unsigned long long)base) *over = true;
        if (!*over) v = v * (unsigned long long)base + (unsigned long long)d;
    }
    if (*over) v = ULLONG_MAX;
    if (end) *end = (char *)(p == digits ? s : p);
    return v;
}

/* Saturating, with errno = ERANGE on overflow, as C requires (and as native libcs do). */
long long strtoll(const char *restrict s, char **restrict end, int base) {
    bool neg, over;
    unsigned long long v = parse_unsigned(s, end, base, &neg, &over);
    if (neg) {
        if (over || v > (unsigned long long)LLONG_MAX + 1) {
            errno = ERANGE;
            return LLONG_MIN;
        }
        /* v == LLONG_MAX + 1 is LLONG_MIN; negate in unsigned arithmetic so nothing overflows. */
        return (long long)(0ULL - v);
    }
    if (over || v > (unsigned long long)LLONG_MAX) {
        errno = ERANGE;
        return LLONG_MAX;
    }
    return (long long)v;
}

unsigned long long strtoull(const char *restrict s, char **restrict end, int base) {
    bool neg, over;
    unsigned long long v = parse_unsigned(s, end, base, &neg, &over);
    if (over) {
        errno = ERANGE;
        return ULLONG_MAX;
    }
    return neg ? 0ULL - v : v;
}

long strtol(const char *restrict s, char **restrict end, int base) {
    long long v = strtoll(s, end, base);
    if (v > LONG_MAX) { errno = ERANGE; return LONG_MAX; }
    if (v < LONG_MIN) { errno = ERANGE; return LONG_MIN; }
    return (long)v;
}

unsigned long strtoul(const char *restrict s, char **restrict end, int base) {
    bool neg, over;
    unsigned long long v = parse_unsigned(s, end, base, &neg, &over);
    if (over || v > ULONG_MAX) {
        errno = ERANGE;
        return ULONG_MAX;
    }
    return neg ? 0UL - (unsigned long)v : (unsigned long)v;
}

long long atoll(const char *s) { return strtoll(s, NULL, 10); }
long atol(const char *s) { return strtol(s, NULL, 10); }
int atoi(const char *s) { return (int)strtol(s, NULL, 10); }
/* abs(INT_MIN) is undefined in C; these wrap instead of trapping. */
int abs(int x) { return x < 0 ? (int)(0U - (unsigned)x) : x; }
long labs(long x) { return x < 0 ? (long)(0UL - (unsigned long)x) : x; }
long long llabs(long long x) { return x < 0 ? (long long)(0ULL - (unsigned long long)x) : x; }

char *getenv(const char *name) { (void)name; return NULL; }
int rand(void) { static unsigned x = 2463534242u; x ^= x << 13; x ^= x >> 17; x ^= x << 5; return (int)(x & RAND_MAX); }

/* qsort: insertion sort for short runs, otherwise a heap sort (no recursion, no allocation). */
static void swap_bytes(unsigned char *a, unsigned char *b, size_t n) {
    while (n--) { unsigned char t = *a; *a++ = *b; *b++ = t; }
}

static void sift_down(unsigned char *base, size_t sz, size_t root, size_t n, int (*cmp)(const void *, const void *)) {
    for (;;) {
        size_t child = 2 * root + 1;
        if (child >= n) return;
        if (child + 1 < n && cmp(base + child * sz, base + (child + 1) * sz) < 0) child++;
        if (cmp(base + root * sz, base + child * sz) >= 0) return;
        swap_bytes(base + root * sz, base + child * sz, sz);
        root = child;
    }
}

void qsort(void *ptr, size_t n, size_t sz, int (*cmp)(const void *, const void *)) {
    unsigned char *base = ptr;
    if (n < 2) return;
    if (n <= 16) {
        for (size_t i = 1; i < n; i++) {
            for (size_t j = i; j > 0 && cmp(base + (j - 1) * sz, base + j * sz) > 0; j--) {
                swap_bytes(base + (j - 1) * sz, base + j * sz, sz);
            }
        }
        return;
    }
    for (size_t i = n / 2; i-- > 0;) sift_down(base, sz, i, n, cmp);
    for (size_t end = n - 1; end > 0; end--) {
        swap_bytes(base, base + end * sz, sz);
        sift_down(base, sz, 0, end, cmp);
    }
}

/* ---- time: the browser has no clock here; flecs takes time from its OS API (Rust). ---- */

int clock_gettime(clockid_t id, struct timespec *ts) {
    (void)id;
    ts->tv_sec = 0;
    ts->tv_nsec = 0;
    return 0;
}

time_t time(time_t *t) {
    if (t) *t = 0;
    return 0;
}

clock_t clock(void) { return 0; }

/* ---- files and backtraces do not exist ---- */

FILE *fopen(const char *restrict path, const char *restrict mode) { (void)path; (void)mode; errno = EINVAL; return NULL; }
int fclose(FILE *f) { (void)f; return 0; }
size_t fread(void *restrict p, size_t sz, size_t n, FILE *restrict f) { (void)p; (void)sz; (void)n; (void)f; return 0; }
size_t fwrite(const void *restrict p, size_t sz, size_t n, FILE *restrict f) { (void)p; (void)sz; (void)f; return n; }
int fseek(FILE *f, long off, int whence) { (void)f; (void)off; (void)whence; return -1; }
long ftell(FILE *f) { (void)f; return -1; }
void rewind(FILE *f) { (void)f; }
int fgetc(FILE *f) { (void)f; return EOF; }
int getc(FILE *f) { (void)f; return EOF; }
int fputs(const char *restrict s, FILE *restrict f) { (void)s; (void)f; return 0; }
int fputc(int c, FILE *f) { (void)f; return c; }
int putc(int c, FILE *f) { (void)f; return c; }
int puts(const char *s) { (void)s; return 0; }
int putchar(int c) { return c; }
int fflush(FILE *f) { (void)f; return 0; }
int backtrace(void **buf, int size) { (void)buf; (void)size; return 0; }
char **backtrace_symbols(void *const *buf, int size) { (void)buf; (void)size; return NULL; }

/* ---- printf: flags - + space 0 #, width and precision (also *), lengths hh h l ll z j t L,
 * conversions d i u o x X c s p f F e E g G %. Floats are printed by digit extraction, exact to
 * about 15 significant digits, which is what flecs' messages need. ---- */

typedef struct {
    char *buf;
    size_t cap;
    size_t len;
} out_t;

static void put(out_t *o, char c) {
    if (o->len + 1 < o->cap) o->buf[o->len] = c;
    o->len++;
}

static void put_n(out_t *o, const char *s, size_t n) {
    for (size_t i = 0; i < n; i++) put(o, s[i]);
}

static void pad(out_t *o, char c, int n) {
    for (; n > 0; n--) put(o, c);
}

typedef struct {
    bool left, plus, space, zero, alt;
    int width;
    int prec;
} spec_t;

/* Emit `body` (n chars) with a sign/prefix, honouring width, '-' and '0'. */
static void emit(out_t *o, const spec_t *sp, const char *prefix, const char *body, size_t n, int zeros) {
    size_t pl = strlen(prefix);
    int total = (int)(pl + n) + zeros;
    int fill = sp->width > total ? sp->width - total : 0;
    if (!sp->left && !sp->zero) pad(o, ' ', fill);
    put_n(o, prefix, pl);
    if (!sp->left && sp->zero) pad(o, '0', fill);
    pad(o, '0', zeros);
    put_n(o, body, n);
    if (sp->left) pad(o, ' ', fill);
}

static void fmt_uint(out_t *o, const spec_t *sp, unsigned long long v, unsigned base, bool upper, const char *prefix) {
    char tmp[72];
    size_t n = 0;
    const char *digits = upper ? "0123456789ABCDEF" : "0123456789abcdef";
    if (v == 0 && sp->prec == 0) {
        n = 0;
    } else {
        do { tmp[sizeof tmp - 1 - n++] = digits[v % base]; v /= base; } while (v);
    }
    int zeros = sp->prec > (int)n ? sp->prec - (int)n : 0;
    spec_t s = *sp;
    if (sp->prec >= 0) s.zero = false;
    emit(o, &s, prefix, tmp + sizeof tmp - n, n, zeros);
}

static double pow10i(int e) {
    double r = 1;
    double b = 10;
    if (e < 0) { b = 0.1; e = -e; }
    while (e--) r *= b;
    return r;
}

/* Fixed notation of |v| with `prec` fraction digits into buf; returns length. */
static size_t fixed(char *buf, size_t cap, double v, int prec, bool keep_point) {
    size_t n = 0;
    if (prec > 17) prec = 17;
    double scale = pow10i(prec);
    double r = v * scale + 0.5;
    unsigned long long bits;
    __builtin_memcpy(&bits, &v, sizeof bits);
    int exp = (int)((bits >> 52) & 0x7ff) - 1075;
    if (r >= 1.8e19 && exp >= 0) {
        /* Too large for integer digits, and an exact integer mant * 2^exp: print every digit, as native libc does,
         * with a base-1e9 big integer (2^1024 has 309 digits; body[] in fmt_float holds 512). */
        unsigned long long mant = (bits & ((1ULL << 52) - 1)) | (1ULL << 52);
        unsigned int limb[40];
        int nl = 0;
        while (mant) { limb[nl++] = (unsigned int)(mant % 1000000000u); mant /= 1000000000u; }
        for (int i = 0; i < exp; i++) {
            unsigned int carry = 0;
            for (int j = 0; j < nl; j++) {
                unsigned long long x = (unsigned long long)limb[j] * 2 + carry;
                limb[j] = (unsigned int)(x % 1000000000u);
                carry = (unsigned int)(x / 1000000000u);
            }
            if (carry) limb[nl++] = carry;
        }
        char tmp[16];
        int t = 0;
        unsigned int top = limb[nl - 1];
        do { tmp[t++] = (char)('0' + top % 10); top /= 10; } while (top);
        while (t) buf[n++] = tmp[--t];
        for (int j = nl - 2; j >= 0; j--) {
            unsigned int x = limb[j];
            for (int k = 8; k >= 0; k--) { buf[n + (size_t)k] = (char)('0' + x % 10); x /= 10; }
            n += 9;
        }
        if (prec > 0 || keep_point) buf[n++] = '.';
        for (int i = 0; i < prec; i++) buf[n++] = '0';
        return n;
    }
    if (r >= 1.8e19) {
        /* A large value with a fraction (only at a high precision): the integer part by repeated division,
         * approximate. */
        char tmp[400];
        size_t t = 0;
        double ip = v;
        while (ip >= 1 && t < sizeof tmp) {
            double q = ip / 10;
            double fl = (double)(unsigned long long)(q >= 1.8e19 ? 0 : q);
            int d = (int)(ip - fl * 10);
            if (q >= 1.8e19) { d = 0; fl = q; }
            tmp[t++] = (char)('0' + (d < 0 ? 0 : d > 9 ? 9 : d));
            ip = fl;
            if (q >= 1.8e19) ip = q - (double)(long long)0;
            if (ip < 1) break;
        }
        while (t && n + 1 < cap) buf[n++] = tmp[--t];
        return n;
    }
    unsigned long long whole = (unsigned long long)r;
    unsigned long long ip = whole / (unsigned long long)scale;
    unsigned long long fp = whole % (unsigned long long)scale;
    char tmp[32];
    size_t t = 0;
    do { tmp[t++] = (char)('0' + ip % 10); ip /= 10; } while (ip);
    while (t && n + 1 < cap) buf[n++] = tmp[--t];
    if (prec > 0 || keep_point) buf[n++] = '.';
    for (int i = prec - 1; i >= 0; i--) {
        unsigned long long d = fp / (unsigned long long)pow10i(i);
        buf[n++] = (char)('0' + d % 10);
    }
    return n;
}

static size_t expo(char *buf, size_t cap, double v, int prec, bool upper, bool keep_point) {
    int e = 0;
    if (v != 0) {
        while (v >= 10) { v /= 10; e++; }
        while (v < 1) { v *= 10; e--; }
        /* rounding may carry into a new digit */
        double r = v + 0.5 * pow10i(-prec);
        if (r >= 10) { v /= 10; e++; }
    }
    size_t n = fixed(buf, cap, v, prec, keep_point);
    buf[n++] = upper ? 'E' : 'e';
    buf[n++] = e < 0 ? '-' : '+';
    if (e < 0) e = -e;
    if (e >= 100) buf[n++] = (char)('0' + e / 100);
    buf[n++] = (char)('0' + (e / 10) % 10);
    buf[n++] = (char)('0' + e % 10);
    return n;
}

static void fmt_float(out_t *o, const spec_t *sp, double v, char conv) {
    char body[512];
    size_t n;
    const char *sign = "";
    bool upper = conv == 'F' || conv == 'E' || conv == 'G';
    if (__builtin_signbit(v)) { sign = "-"; v = -v; }
    else if (sp->plus) sign = "+";
    else if (sp->space) sign = " ";
    if (__builtin_isnan(v) || __builtin_isinf(v)) {
        const char *t = __builtin_isnan(v) ? (upper ? "NAN" : "nan") : (upper ? "INF" : "inf");
        spec_t s = *sp;
        s.zero = false;
        emit(o, &s, sign, t, 3, 0);
        return;
    }
    int prec = sp->prec < 0 ? 6 : sp->prec;
    if (conv == 'f' || conv == 'F') {
        n = fixed(body, sizeof body, v, prec, sp->alt);
    } else if (conv == 'e' || conv == 'E') {
        n = expo(body, sizeof body, v, prec, upper, sp->alt);
    } else {
        if (prec == 0) prec = 1;
        int e = 0;
        double t = v;
        if (t != 0) {
            while (t >= 10) { t /= 10; e++; }
            while (t < 1) { t *= 10; e--; }
        }
        if (e < -4 || e >= prec) n = expo(body, sizeof body, v, prec - 1, upper, sp->alt);
        else n = fixed(body, sizeof body, v, prec - 1 - e, sp->alt);
        if (!sp->alt) {
            /* strip trailing zeros of the fraction (before an exponent, if any) */
            size_t ex = n;
            for (size_t i = 0; i < n; i++) if (body[i] == 'e' || body[i] == 'E') { ex = i; break; }
            size_t dot = ex;
            for (size_t i = 0; i < ex; i++) if (body[i] == '.') { dot = i; break; }
            if (dot < ex) {
                size_t cut = ex;
                while (cut > dot + 1 && body[cut - 1] == '0') cut--;
                if (cut == dot + 1) cut = dot;
                memmove(body + cut, body + ex, n - ex);
                n = cut + (n - ex);
            }
        }
    }
    emit(o, sp, sign, body, n, 0);
}

int vsnprintf(char *restrict buf, size_t cap, const char *restrict fmt, va_list ap) {
    out_t o = { buf, cap, 0 };
    for (const char *p = fmt; *p; p++) {
        if (*p != '%') { put(&o, *p); continue; }
        p++;
        spec_t sp = { false, false, false, false, false, 0, -1 };
        for (;; p++) {
            if (*p == '-') sp.left = true;
            else if (*p == '+') sp.plus = true;
            else if (*p == ' ') sp.space = true;
            else if (*p == '0') sp.zero = true;
            else if (*p == '#') sp.alt = true;
            else break;
        }
        if (*p == '*') {
            sp.width = va_arg(ap, int);
            if (sp.width < 0) { sp.left = true; sp.width = -sp.width; }
            p++;
        } else {
            while (isdigit((unsigned char)*p)) sp.width = sp.width * 10 + (*p++ - '0');
        }
        if (*p == '.') {
            p++;
            sp.prec = 0;
            if (*p == '*') {
                sp.prec = va_arg(ap, int);
                p++;
            } else {
                while (isdigit((unsigned char)*p)) sp.prec = sp.prec * 10 + (*p++ - '0');
            }
        }
        if (sp.left) sp.zero = false;
        int len = 0; /* 0 int, 1 long, 2 long long, -1 short, -2 char, 3 size_t/ptrdiff_t, 4 intmax_t */
        for (;; p++) {
            if (*p == 'h') len = len == -1 ? -2 : -1;
            else if (*p == 'l') len = len == 1 ? 2 : 1;
            else if (*p == 'z' || *p == 't') len = 3;
            else if (*p == 'j') len = 4;
            else if (*p == 'L') len = 2;
            else break;
        }
        char c = *p;
        if (!c) break;
        switch (c) {
        case 'd':
        case 'i': {
            long long v;
            if (len == 2) v = va_arg(ap, long long);
            else if (len == 1) v = va_arg(ap, long);
            else if (len == 3) v = (long long)va_arg(ap, ptrdiff_t);
            else if (len == 4) v = (long long)va_arg(ap, intmax_t);
            else v = va_arg(ap, int);
            if (len == -1) v = (short)v;
            if (len == -2) v = (signed char)v;
            const char *sign = v < 0 ? "-" : sp.plus ? "+" : sp.space ? " " : "";
            unsigned long long u = v < 0 ? (unsigned long long)(-(v + 1)) + 1 : (unsigned long long)v;
            fmt_uint(&o, &sp, u, 10, false, sign);
            break;
        }
        case 'u':
        case 'x':
        case 'X':
        case 'o': {
            unsigned long long v;
            if (len == 2) v = va_arg(ap, unsigned long long);
            else if (len == 1) v = va_arg(ap, unsigned long);
            else if (len == 3) v = va_arg(ap, size_t);
            else if (len == 4) v = (unsigned long long)va_arg(ap, uintmax_t);
            else v = va_arg(ap, unsigned int);
            if (len == -1) v = (unsigned short)v;
            if (len == -2) v = (unsigned char)v;
            unsigned base = c == 'u' ? 10 : c == 'o' ? 8 : 16;
            const char *prefix = "";
            if (sp.alt && v) prefix = c == 'x' ? "0x" : c == 'X' ? "0X" : c == 'o' ? "0" : "";
            fmt_uint(&o, &sp, v, base, c == 'X', prefix);
            break;
        }
        case 'p': {
            uintptr_t v = (uintptr_t)va_arg(ap, void *);
            spec_t s = sp;
            s.prec = -1;
            fmt_uint(&o, &s, v, 16, false, "0x");
            break;
        }
        case 'c': {
            char ch = (char)va_arg(ap, int);
            spec_t s = sp;
            s.zero = false;
            emit(&o, &s, "", &ch, 1, 0);
            break;
        }
        case 's': {
            const char *s = va_arg(ap, const char *);
            if (!s) s = "(null)";
            size_t n = sp.prec >= 0 ? strnlen(s, (size_t)sp.prec) : strlen(s);
            spec_t t = sp;
            t.zero = false;
            emit(&o, &t, "", s, n, 0);
            break;
        }
        case 'f':
        case 'F':
        case 'e':
        case 'E':
        case 'g':
        case 'G': {
            double v = len == 2 ? (double)va_arg(ap, long double) : va_arg(ap, double);
            fmt_float(&o, &sp, v, c);
            break;
        }
        case '%':
            put(&o, '%');
            break;
        default:
            put(&o, '%');
            put(&o, c);
            break;
        }
    }
    if (cap) buf[o.len < cap ? o.len : cap - 1] = 0;
    return (int)o.len;
}

int snprintf(char *restrict buf, size_t n, const char *restrict fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int r = vsnprintf(buf, n, fmt, ap);
    va_end(ap);
    return r;
}

int vsprintf(char *restrict buf, const char *restrict fmt, va_list ap) {
    return vsnprintf(buf, (size_t)INT_MAX, fmt, ap);
}

int sprintf(char *restrict buf, const char *restrict fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int r = vsnprintf(buf, (size_t)INT_MAX, fmt, ap);
    va_end(ap);
    return r;
}

int vfprintf(FILE *restrict f, const char *restrict fmt, va_list ap) {
    (void)f;
    return vsnprintf(NULL, 0, fmt, ap);
}

int fprintf(FILE *restrict f, const char *restrict fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int r = vfprintf(f, fmt, ap);
    va_end(ap);
    return r;
}

int printf(const char *restrict fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int r = vfprintf(stdout, fmt, ap);
    va_end(ap);
    return r;
}
