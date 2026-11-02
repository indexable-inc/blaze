// The interposer of the sandbox on Linux (`sandbox/linux.rs`): a library every process of a
// traced run loads through `LD_PRELOAD`, which tells the supervisor what its seccomp filter
// can't see. It says each thing by a `faccessat(path, F_OK)` the filter asks the supervisor
// about, which fails as nothing is there:
//
//     /.blaze-env/<name>   a variable read by name (`getenv`, `secure_getenv`)
//     /.blaze-clock        the realtime clock read (`clock_gettime`, `gettimeofday`, `time`,
//                          `timespec_get`), which the vDSO answers without a call
//     /.blaze-loaded       this library is in the process, said as it loads
//
// A process that ran a program which never said it loaded is not traced whole: the supervisor
// says so as the run ends. What walks `environ` is seen by no call; the launcher keys every
// traced run by the whole environment for it (`sandbox/v2.rs`).
//
// Each thing is said once in a thread's scope, which the harness's marker
// (`access("/.blaze-test/<name>")`, which the supervisor sees itself) starts. The program sees
// the environment it was given: as it loads, this library takes itself out of `LD_PRELOAD`
// (`<this>:<theirs>`, or `<this>` when there was none) and takes away `__BLAZE_INTERPOSE`,
// which names it, and it puts both back in the environment of each program the process runs.

#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <spawn.h>
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

extern char **environ;

#define MARK "/.blaze-test/"
#define ENV_MARK "/.blaze-env/"
#define CLOCK_MARK "/.blaze-clock"
#define LOADED_MARK "/.blaze-loaded"
#define INTERPOSING "__BLAZE_INTERPOSE"
#define PRELOAD "LD_PRELOAD"
// The longest name said: the supervisor reads a path up to 4096 bytes. A longer one is in the
// whole environment every traced run reads.
#define LONGEST_NAME 4000

static char self[4096];
static size_t self_len;
// Whether this library was given its path: it is in a sandbox's process.
static int alive;

#define TLS __thread __attribute__((tls_model("initial-exec")))
#define SEEN 512
// What the thread said in its scope, by a hash of it; 0 for an empty slot. Full, everything
// is said again: the supervisor keeps each line once anyway.
static TLS uint64_t seen[SEEN];
static TLS unsigned seen_count;

static int (*real_access)(const char *, int);
static int (*real_clock_gettime)(clockid_t, struct timespec *);
static int (*real_gettimeofday)(struct timeval *, void *);
static time_t (*real_time)(time_t *);
static int (*real_timespec_get)(struct timespec *, int);
static int (*real_execve)(const char *, char *const[], char *const[]);
static int (*real_execvpe)(const char *, char *const[], char *const[]);
static int (*real_fexecve)(int, char *const[], char *const[]);
static int (*real_posix_spawn)(pid_t *, const char *, const posix_spawn_file_actions_t *,
                               const posix_spawnattr_t *, char *const[], char *const[]);
static int (*real_posix_spawnp)(pid_t *, const char *, const posix_spawn_file_actions_t *,
                                const posix_spawnattr_t *, char *const[], char *const[]);

#define REAL(name) (real_##name ? real_##name : (real_##name = dlsym(RTLD_NEXT, #name)))

// ---------------------------------------------------------------------------------------
// Saying.

static void say(const char *path) {
    int saved = errno;
    syscall(SYS_faccessat, AT_FDCWD, path, F_OK);
    errno = saved;
}

static inline uint64_t mum(uint64_t a, uint64_t b) {
    __uint128_t r = (__uint128_t)a * b;
    return (uint64_t)r ^ (uint64_t)(r >> 64);
}

static inline uint64_t word(const char *s, size_t n) {
    uint64_t w = 0;
    memcpy(&w, s, n);
    return w;
}

// `kind` and `name` hashed, sixteen bytes a multiply (wyhash's mixing).
static uint64_t hash(char kind, const char *name) {
    uint64_t h = 0x1d8e4e27c47d124full ^ (uint8_t)kind;
    size_t n = strlen(name), len = n;
    for (; n > 16; name += 16, n -= 16) {
        uint64_t a = word(name, 8), b = word(name + 8, 8);
        h ^= mum(a ^ 0xa0761d6478bd642full, b ^ h ^ 0xe7037ed1a0b428dbull);
    }
    uint64_t a = word(name, n > 8 ? 8 : n), b = n > 8 ? word(name + 8, n - 8) : 0;
    return (h ^ mum(a ^ 0x8ebc6af09c88c6e3ull ^ len, b ^ h ^ 0x589965cc75374cc3ull)) | 1;
}

// Whether `kind` and `name` are new to the thread's scope.
static int first_time(char kind, const char *name) {
    uint64_t h = hash(kind, name);
    if (seen_count * 2 >= SEEN) return 1;
    unsigned at = h & (SEEN - 1);
    while (seen[at] && seen[at] != h) at = (at + 1) & (SEEN - 1);
    if (seen[at]) return 0;
    seen[at] = h;
    seen_count++;
    return 1;
}

static void read_var(const char *name) {
    if (!alive || !name) return;
    size_t len = strlen(name);
    if (len > LONGEST_NAME || !first_time('E', name)) return;
    char path[sizeof ENV_MARK + LONGEST_NAME];
    memcpy(path, ENV_MARK, sizeof ENV_MARK - 1);
    memcpy(path + sizeof ENV_MARK - 1, name, len + 1);
    say(path);
}

static void read_clock(void) {
    if (alive && first_time('T', "clock")) say(CLOCK_MARK);
}

// ---------------------------------------------------------------------------------------
// Variables.

// The value of `name` in `environ`, as the C library's `getenv` finds it.
static char *lookup(const char *name) {
    size_t len = strlen(name);
    if (!len || !environ) return NULL;
    for (char **it = environ; *it; it++)
        if (!strncmp(*it, name, len) && (*it)[len] == '=') return *it + len + 1;
    return NULL;
}

char *getenv(const char *name) {
    read_var(name);
    return lookup(name);
}

char *secure_getenv(const char *name) {
    read_var(name);
    return getauxval(AT_SECURE) ? NULL : lookup(name);
}

// ---------------------------------------------------------------------------------------
// The clock.

static int realtime(clockid_t clock) {
    return clock == CLOCK_REALTIME || clock == CLOCK_REALTIME_COARSE ||
           clock == CLOCK_REALTIME_ALARM || clock == CLOCK_TAI;
}

int clock_gettime(clockid_t clock, struct timespec *ts) {
    if (realtime(clock)) read_clock();
    if (!REAL(clock_gettime)) return syscall(SYS_clock_gettime, clock, ts);
    return real_clock_gettime(clock, ts);
}

int gettimeofday(struct timeval *tv, void *tz) {
    read_clock();
    return REAL(gettimeofday)(tv, tz);
}

time_t time(time_t *t) {
    read_clock();
    return REAL(time)(t);
}

int timespec_get(struct timespec *ts, int base) {
    if (base == TIME_UTC) read_clock();
    return REAL(timespec_get)(ts, base);
}

// The harness's marker starts a scope: what the thread says is new to it.
int access(const char *path, int mode) {
    if (alive && path && !strncmp(path, MARK, sizeof MARK - 1)) {
        memset(seen, 0, sizeof seen);
        seen_count = 0;
    }
    return REAL(access)(path, mode);
}

// ---------------------------------------------------------------------------------------
// Programs.

static size_t count(char *const envp[]) {
    size_t n = 0;
    while (envp && envp[n]) n++;
    return n;
}

static int named(const char *entry, const char *name) {
    size_t len = strlen(name);
    return !strncmp(entry, name, len) && entry[len] == '=';
}

// How long `preload` is to be for the `LD_PRELOAD` of `envp` (none: `NULL`), with this library
// first.
static size_t preload_len(char *const envp[]) {
    for (char *const *it = envp; it && *it; it++)
        if (named(*it, PRELOAD)) return sizeof PRELOAD + self_len + 1 + strlen(*it);
    return sizeof PRELOAD + self_len + 1;
}

// `envp` with this library in it, in `out` (`count(envp) + 3` entries): `LD_PRELOAD` with it
// first, in its place, or last when there was none, and its path. `preload` and `interposing`
// are where those two entries are written. Made on the stack: the process may be a `vfork`
// child, which must not allocate.
static void environment(char *const envp[], char **out, char *preload, char *interposing) {
    size_t n = 0;
    int put = 0;
    for (char *const *it = envp; it && *it; it++) {
        if (named(*it, INTERPOSING)) continue;
        if (named(*it, PRELOAD)) {
            const char *theirs = *it + sizeof PRELOAD;
            int ours = !strncmp(theirs, self, self_len) &&
                       (theirs[self_len] == ':' || theirs[self_len] == 0);
            if (ours) {
                strcpy(preload, *it);
            } else {
                strcpy(preload, PRELOAD "=");
                strcat(preload, self);
                strcat(preload, ":");
                strcat(preload, theirs);
            }
            out[n++] = preload;
            put = 1;
            continue;
        }
        out[n++] = *it;
    }
    if (!put) {
        strcpy(preload, PRELOAD "=");
        strcat(preload, self);
        out[n++] = preload;
    }
    strcpy(interposing, INTERPOSING "=");
    strcat(interposing, self);
    out[n++] = interposing;
    out[n] = NULL;
}

#define WITH_US(envp, env)                                                                 \
    char *env[count(envp) + 3];                                                            \
    char preload_##env[preload_len(envp)];                                                 \
    char interposing_##env[sizeof INTERPOSING + self_len + 1];                             \
    environment(envp, env, preload_##env, interposing_##env)

int execve(const char *path, char *const argv[], char *const envp[]) {
    if (!alive) return REAL(execve)(path, argv, envp);
    WITH_US(envp, env);
    return REAL(execve)(path, argv, env);
}

int execv(const char *path, char *const argv[]) {
    return execve(path, argv, environ);
}

int execvpe(const char *file, char *const argv[], char *const envp[]) {
    if (!alive) return REAL(execvpe)(file, argv, envp);
    WITH_US(envp, env);
    return REAL(execvpe)(file, argv, env);
}

int execvp(const char *file, char *const argv[]) {
    return execvpe(file, argv, environ);
}

int fexecve(int fd, char *const argv[], char *const envp[]) {
    if (!alive) return REAL(fexecve)(fd, argv, envp);
    WITH_US(envp, env);
    return REAL(fexecve)(fd, argv, env);
}

// How many arguments follow the first of an `execl`, up to and with the null one.
static size_t variadic_count(va_list ap) {
    size_t n = 1;
    while (va_arg(ap, char *)) n++;
    return n;
}

// The arguments of an `execl` from `first` up to the null one in `argv`, and the environment
// after them in `envp` when `with_env` (`environ` otherwise).
#define ARGV(first, argv, envp, with_env)                                                  \
    va_list ap;                                                                            \
    va_start(ap, first);                                                                   \
    size_t argc_ = first ? variadic_count(ap) : 0;                                         \
    va_end(ap);                                                                            \
    char *argv[argc_ + 1];                                                                 \
    char *const *envp = environ;                                                           \
    argv[0] = (char *)first;                                                               \
    va_start(ap, first);                                                                   \
    for (size_t i = 1; i <= argc_; i++) argv[i] = va_arg(ap, char *);                      \
    if (with_env) envp = va_arg(ap, char *const *);                                        \
    va_end(ap)

int execl(const char *path, const char *arg, ...) {
    ARGV(arg, argv, envp, 0);
    return execve(path, argv, envp);
}

int execle(const char *path, const char *arg, ...) {
    ARGV(arg, argv, envp, 1);
    return execve(path, argv, envp);
}

int execlp(const char *file, const char *arg, ...) {
    ARGV(arg, argv, envp, 0);
    return execvpe(file, argv, envp);
}

int posix_spawn(pid_t *pid, const char *path, const posix_spawn_file_actions_t *actions,
                const posix_spawnattr_t *attr, char *const argv[], char *const envp[]) {
    if (!alive) return REAL(posix_spawn)(pid, path, actions, attr, argv, envp);
    WITH_US(envp, env);
    return REAL(posix_spawn)(pid, path, actions, attr, argv, env);
}

int posix_spawnp(pid_t *pid, const char *file, const posix_spawn_file_actions_t *actions,
                 const posix_spawnattr_t *attr, char *const argv[], char *const envp[]) {
    if (!alive) return REAL(posix_spawnp)(pid, file, actions, attr, argv, envp);
    WITH_US(envp, env);
    return REAL(posix_spawnp)(pid, file, actions, attr, argv, env);
}

// ---------------------------------------------------------------------------------------
// Loading.

__attribute__((constructor(101))) static void start(void) {
    REAL(access);
    REAL(clock_gettime);
    REAL(gettimeofday);
    REAL(time);
    REAL(timespec_get);
    REAL(execve);
    REAL(execvpe);
    REAL(fexecve);
    REAL(posix_spawn);
    REAL(posix_spawnp);
    const char *me = lookup(INTERPOSING);
    if (!me || !*me || strlen(me) >= sizeof self) return;
    strcpy(self, me);
    self_len = strlen(self);
    const char *preload = lookup(PRELOAD);
    if (preload && !strncmp(preload, self, self_len)) {
        if (preload[self_len] == ':') setenv(PRELOAD, preload + self_len + 1, 1);
        else if (preload[self_len] == 0) unsetenv(PRELOAD);
    }
    unsetenv(INTERPOSING);
    alive = 1;
    say(LOADED_MARK);
}
