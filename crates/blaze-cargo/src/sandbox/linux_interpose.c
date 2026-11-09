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
// says so as the run ends. What walks `environ` is seen by no call. A run the launcher gives
// its whole environment it keys by all of it. A run it lends variables to (`sandbox/v2.rs`)
// gets each as an entry `__BLAZE_LENT=NAME=VALUE`, and the visible names in
// `__BLAZE_VISIBLE`: this library takes the entries out of the environment as it loads and
// answers `getenv` from them, saying the read (a library's constructor that runs first is
// answered from the entries, and its reads said as this one loads). What walks the environment
// then sees the visible variables alone, which the run's key has, until the process says it
// walks it (std's `__blaze_environ_walked`; `system`, `popen`): then every lent variable is put
// back in the environment, and the whole environment is said read, `/.blaze-env/* <why>`.
//
// A program the process runs with its own environment gets the lent variables back in it, as
// with no sandbox, and the visible names in `__BLAZE_VISIBLE_CHILD`: running it is no read.
// Its own copy of this library lends it what is not visible when its executable exports
// `__blaze_std_says_walks` (blaze's linker proves std alone walks its environment), and says
// `/.blaze-env/* <program> does not export ...` as it loads otherwise: a C program, or a shell
// that takes its environment from `main`'s `envp`, may walk it unseen.
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
#include <sched.h>
#include <stdio.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mman.h>
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
#define LENT "__BLAZE_LENT"
// The visible names, `NAME=NAME=...` (`sandbox/v2.rs`, `VISIBLE`): from the launcher, to the
// program it lends to; from this library, to each program a process runs, which lends to
// itself when it proves it says its walks.
#define VISIBLE "__BLAZE_VISIBLE"
#define CHILD_VISIBLE "__BLAZE_VISIBLE_CHILD"
#define PRELOAD "LD_PRELOAD"
// The longest name said: the supervisor reads a path up to 4096 bytes. A longer one is said as
// the whole environment, read.
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
static int (*real_unsetenv)(const char *);
static int (*real_clearenv)(void);
static int (*real_putenv)(char *);
static int (*real_system)(const char *);
static FILE *(*real_popen)(const char *, const char *);

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
    if (len > LONGEST_NAME) name = "* a variable's name is too long to say", len = strlen(name);
    if (!first_time('E', name)) return;
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

static int named(const char *entry, const char *name) {
    size_t len = strlen(name);
    return !strncmp(entry, name, len) && entry[len] == '=';
}

// The value of `name` in `environ`, as the C library's `getenv` finds it.
static char *lookup(const char *name) {
    size_t len = strlen(name);
    if (!len || !environ) return NULL;
    for (char **it = environ; *it; it++)
        if (!strncmp(*it, name, len) && (*it)[len] == '=') return *it + len + 1;
    return NULL;
}

// The variables lent to the process (`NAME=VALUE`), and of each whether the process took it
// away (`unsetenv`, `clearenv`). Each is the rest of an entry `LENT` the launcher started the
// process with, or an entry the process was started with whose name is not visible; either
// stays where the kernel put it. Set as the library loads, before the program runs.
static char **lent;
static unsigned char *lent_gone;
static size_t lent_count;
// Whether every lent variable is back in `environ`: the process walked its environment.
static int lent_back;
static int lent_busy;
// The names a lending run keeps visible (`NAME=NAME=...`, the value of `VISIBLE` the process
// was started with), passed on to each program it runs; `NULL` when the run does not lend.
static const char *visible;
static size_t visible_len;
// Set first as the library loads: until then `getenv` comes from other libraries' constructors.
static int started;

// Whether `entry` (`NAME=VALUE`) is of the variable `name`, `len` bytes.
static int entry_of(const char *entry, const char *name, size_t len) {
    return !strncmp(entry, name, len) && entry[len] == '=';
}

// Whether variables are lent rather than in `environ`.
static int lending(void) {
    return lent_count && !__atomic_load_n(&lent_back, __ATOMIC_ACQUIRE);
}

// The value of `name` lent and not taken away, while it is lent rather than in `environ`.
static char *lent_value(const char *name) {
    if (!lending() || !name) return NULL;
    size_t len = strlen(name);
    for (size_t i = 0; i < lent_count; i++)
        if (!lent_gone[i] && entry_of(lent[i], name, len)) return lent[i] + len + 1;
    return NULL;
}

// The value of `name` in an entry `LENT` still in `environ`: what a library's constructor
// that runs before this library's is answered.
static char *encoded_value(const char *name) {
    size_t len = strlen(name);
    for (char **it = environ; it && *it; it++)
        if (named(*it, LENT) && entry_of(*it + sizeof LENT, name, len)) return *it + sizeof LENT + len + 1;
    return NULL;
}

// The process took `name` away: it is not lent any more.
static void forget(const char *name) {
    if (!lent_count || !name) return;
    size_t len = strlen(name);
    for (size_t i = 0; i < lent_count; i++)
        if (entry_of(lent[i], name, len)) lent_gone[i] = 1;
}

// Whether `env` has the variable of `entry` (`NAME=VALUE`).
static int has(char *const env[], const char *entry) {
    size_t len = strcspn(entry, "=");
    for (char *const *it = env; it && *it; it++)
        if (entry_of(*it, entry, len)) return 1;
    return 0;
}

// Whether the variable of `entry` (`NAME=VALUE`) is one the run keeps visible.
static int is_visible(const char *entry) {
    size_t len = strcspn(entry, "=");
    for (const char *at = visible; at && *at;) {
        size_t n = strcspn(at, "=");
        if (n == len && !strncmp(at, entry, len)) return 1;
        at += n;
        if (*at) at++;
    }
    return 0;
}

// Every lent variable not taken away, and not set by the process since, put back in
// `environ`, once: a new array of what it has and those, mapped rather than allocated (the
// process may be a `vfork` or `fork` child, which must not allocate), and never unmapped (a
// thread may still be reading the one before). One that can't be made ends the process: it
// would see less than it was given.
static void lend_back(void) {
    if (!lending()) return;
    // The other thread putting them back does it in microseconds.
    while (__atomic_exchange_n(&lent_busy, 1, __ATOMIC_ACQUIRE)) sched_yield();
    if (!__atomic_load_n(&lent_back, __ATOMIC_RELAXED)) {
        size_t have = 0, add = 0;
        while (environ && environ[have]) have++;
        for (size_t i = 0; i < lent_count; i++)
            if (!lent_gone[i] && !has(environ, lent[i])) add++;
        size_t bytes = (have + add + 1) * sizeof(char *);
        char **all = mmap(NULL, bytes, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (all == MAP_FAILED) {
            static const char why[] = "blaze sandbox: the variables lent to this process could not be "
                                      "put back in its environment as it walked it: out of memory\n";
            syscall(SYS_write, 2, why, sizeof why - 1);
            abort();
        }
        size_t n = 0;
        for (size_t i = 0; i < have; i++) all[n++] = environ[i];
        for (size_t i = 0; i < lent_count; i++)
            if (!lent_gone[i] && !has(environ, lent[i])) all[n++] = lent[i];
        all[n] = NULL;
        environ = all;
        __atomic_store_n(&lent_back, 1, __ATOMIC_RELEASE);
    }
    __atomic_store_n(&lent_busy, 0, __ATOMIC_RELEASE);
}

// The process walks its whole environment, as `why` (`* <why>`) says: it read every variable,
// lent ones too.
static void walked_whole(const char *why) {
    read_var(why);
    lend_back();
}

// What std calls before it walks the environment (`env::vars`, a `Command` it changes the
// environment of, which it copies).
void __blaze_environ_walked(void) {
    if (alive) walked_whole("* std walked it");
}

static void out_of_memory(void) {
    static const char why[] = "blaze sandbox: the variables lent to this process could not be kept: "
                              "out of memory\n";
    syscall(SYS_write, 2, why, sizeof why - 1);
    abort();
}

// The variables `LENT` entries lend, taken out of `environ` as the library loads: the
// launcher's program, which it lends to (`sandbox/v2.rs`).
static void take_lent(void) {
    size_t n = 0;
    for (char **it = environ; it && *it; it++)
        if (named(*it, LENT)) n++;
    if (!n) return;
    lent = malloc(n * sizeof *lent);
    lent_gone = calloc(n, 1);
    if (!lent || !lent_gone) out_of_memory();
    for (char **it = environ; it && *it; it++)
        if (named(*it, LENT)) lent[lent_count++] = *it + sizeof LENT;
    REAL(unsetenv)(LENT);
}

// The variables of `environ` that are not visible, taken out of it as the library loads: a
// program run by a lending process that proves it says its walks lends them to itself.
static void lend_rest(void) {
    size_t n = 0;
    for (char **it = environ; it && *it; it++)
        if (!is_visible(*it)) n++;
    if (!n) return;
    lent = malloc(n * sizeof *lent);
    lent_gone = calloc(n, 1);
    if (!lent || !lent_gone) out_of_memory();
    for (char **it = environ; it && *it && lent_count < n; it++)
        if (!is_visible(*it)) lent[lent_count++] = *it;
    char name[LONGEST_NAME + 1];
    for (size_t i = 0; i < lent_count; i++) {
        size_t len = strcspn(lent[i], "=");
        if (len > LONGEST_NAME) {
            // Not taken away: in `environ` a walk could see it, so every variable is read.
            lent_gone[i] = 1;
            read_var("* a variable's name is too long to lend");
            continue;
        }
        memcpy(name, lent[i], len);
        name[len] = 0;
        REAL(unsetenv)(name);
    }
}

// Whether the program proves std alone walks its environment: its executable exports
// `__blaze_std_says_walks`, which blaze's linker puts in an executable no other code of which
// reads `environ` (`fed1eb428d:docs/v2/requests/usercode.md`, item 6).
static int proves_walks(void) {
    void *marker = dlsym(RTLD_DEFAULT, "__blaze_std_says_walks");
    Dl_info at, program;
    return marker && dladdr(marker, &at) && dladdr((void *)getauxval(AT_ENTRY), &program) &&
           at.dli_fbase == program.dli_fbase;
}

// The names `getenv` was asked before this library loaded, by other libraries' constructors,
// said as it loads; `early_lost` when one could not be kept.
#define EARLY 64
#define EARLY_NAME 128
static char early[EARLY][EARLY_NAME];
static unsigned early_count;
static int early_lost;

static void read_early(const char *name) {
    if (!name) return;
    unsigned at = __atomic_fetch_add(&early_count, 1, __ATOMIC_RELAXED);
    size_t len = strlen(name);
    if (at >= EARLY || len >= EARLY_NAME) {
        __atomic_store_n(&early_lost, 1, __ATOMIC_RELAXED);
        return;
    }
    memcpy(early[at], name, len + 1);
}

static char *value_of(const char *name) {
    if (!name) return NULL;
    char *value = lookup(name);
    if (value) return value;
    return started ? lent_value(name) : encoded_value(name);
}

char *getenv(const char *name) {
    if (!started) read_early(name);
    read_var(name);
    return value_of(name);
}

char *secure_getenv(const char *name) {
    if (!started) read_early(name);
    read_var(name);
    if (getauxval(AT_SECURE)) return NULL;
    return value_of(name);
}

int unsetenv(const char *name) {
    forget(name);
    return REAL(unsetenv)(name);
}

int clearenv(void) {
    for (size_t i = 0; i < lent_count; i++) lent_gone[i] = 1;
    return REAL(clearenv)();
}

// `putenv("NAME")`, with no `=`, takes the variable away.
int putenv(char *string) {
    if (!strchr(string, '=')) forget(string);
    return REAL(putenv)(string);
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


// How long `preload` is to be for the `LD_PRELOAD` of `envp` (none: `NULL`), with this library
// first.
static size_t preload_len(char *const envp[]) {
    for (char *const *it = envp; it && *it; it++)
        if (named(*it, PRELOAD)) return sizeof PRELOAD + self_len + 1 + strlen(*it);
    return sizeof PRELOAD + self_len + 1;
}

// How many lent variables a program run with `envp` is given back: all of them, when `envp`
// is the process's own environment and they are still lent. A program run with an
// environment the process made itself is given what it made.
static size_t lent_room(char *const envp[]) {
    return envp == environ && lending() ? lent_count : 0;
}

// `envp` with this library in it, in `out` (`count(envp) + lent + 4` entries): every lent
// variable not taken away nor set again when `lent` is not 0, as the program would have had
// them with no sandbox (`lent_room`); `LD_PRELOAD` with this library first, in its place, or
// last when there was none; its path; and the visible names when the run lends (`VISIBLE`),
// for the program's own interposer. `preload`, `interposing` and `shown` are where those three
// entries are written. Made on the stack: the process may be a `vfork` child, which must not
// allocate.
static void environment(char *const envp[], size_t lent_n, char **out, char *preload,
                        char *interposing, char *shown) {
    size_t n = 0;
    int put = 0;
    for (char *const *it = envp; it && *it; it++) {
        if (named(*it, INTERPOSING) || named(*it, VISIBLE) || named(*it, CHILD_VISIBLE) || named(*it, LENT))
            continue;
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
    out[n] = NULL;
    for (size_t i = 0, added = 0; i < lent_count && added < lent_n; i++) {
        if (lent_gone[i] || has(out, lent[i])) continue;
        out[n++] = lent[i];
        out[n] = NULL;
        added++;
    }
    if (!put) {
        strcpy(preload, PRELOAD "=");
        strcat(preload, self);
        out[n++] = preload;
    }
    strcpy(interposing, INTERPOSING "=");
    strcat(interposing, self);
    out[n++] = interposing;
    if (visible) {
        strcpy(shown, CHILD_VISIBLE "=");
        strcat(shown, visible);
        out[n++] = shown;
    }
    out[n] = NULL;
}

#define WITH_US(envp, env)                                                                 \
    size_t lent_##env = lent_room(envp);                                                   \
    char *env[count(envp) + lent_##env + 4];                                               \
    char preload_##env[preload_len(envp)];                                                 \
    char interposing_##env[sizeof INTERPOSING + self_len + 1];                             \
    char shown_##env[sizeof CHILD_VISIBLE + visible_len + 1];                              \
    environment(envp, lent_##env, env, preload_##env, interposing_##env, shown_##env)

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

// The shell `system` and `popen` start is given the process's own environment by the C
// library itself, with no call here to hand it the lent variables: they go back in it first.
int system(const char *command) {
    if (alive) walked_whole("* system runs a shell with the environment whole");
    return REAL(system)(command);
}

FILE *popen(const char *command, const char *type) {
    if (alive) walked_whole("* popen runs a shell with the environment whole");
    return REAL(popen)(command, type);
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
    REAL(unsetenv);
    REAL(clearenv);
    REAL(putenv);
    REAL(system);
    REAL(popen);
    const char *me = lookup(INTERPOSING);
    if (!me || !*me || strlen(me) >= sizeof self) {
        started = 1;
        return;
    }
    strcpy(self, me);
    self_len = strlen(self);
    take_lent();
    // The launcher's program is lent to whatever it is; a program a process runs, when it
    // proves it says its walks.
    int launched = lookup(VISIBLE) != NULL;
    const char *names = launched ? lookup(VISIBLE) : lookup(CHILD_VISIBLE);
    if (names) {
        visible = names;
        visible_len = strlen(names);
        REAL(unsetenv)(VISIBLE);
        REAL(unsetenv)(CHILD_VISIBLE);
    }
    started = 1;
    const char *preload = lookup(PRELOAD);
    if (preload && !strncmp(preload, self, self_len)) {
        if (preload[self_len] == ':') setenv(PRELOAD, preload + self_len + 1, 1);
        else if (preload[self_len] == 0) unsetenv(PRELOAD);
    }
    unsetenv(INTERPOSING);
    alive = 1;
    say(LOADED_MARK);
    unsigned early_n = __atomic_load_n(&early_count, __ATOMIC_RELAXED);
    for (unsigned i = 0; i < early_n && i < EARLY; i++) read_var(early[i]);
    if (__atomic_load_n(&early_lost, __ATOMIC_RELAXED))
        read_var("* a variable was read before the interposer loaded that it could not keep");
    if (visible && !launched) {
        if (proves_walks()) {
            lend_rest();
        } else {
            char why[LONGEST_NAME];
            snprintf(why, sizeof why,
                     "* %s does not export __blaze_std_says_walks, so it may walk its environment unseen",
                     program_invocation_name);
            read_var(why);
        }
    }
}
