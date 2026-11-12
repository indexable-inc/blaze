// A program with no C runtime whose thread-locals are all reached through TLS descriptors
// (`-fPIC`, and `-mtls-dialect=desc`, AArch64's default): `own` and `zeroed` are its own,
// `imported` is `libtv.so`'s (tv.c). `_start` exits 0 when every one reads what it holds;
// each bit of another status names what read wrong.
//
//   gcc -c -O1 -fPIC -ffreestanding -fno-stack-protector -fno-asynchronous-unwind-tables \
//       -mtls-dialect=desc -o main.o main.c
extern __thread int imported;
int *imported_here(void);
__thread int own = 40;
static __thread long zeroed;

__attribute__((noinline)) int *own_at(void) { return &own; }
__attribute__((noinline)) long *zeroed_at(void) { return &zeroed; }
__attribute__((noinline)) int *imported_at(void) { return &imported; }

__attribute__((noreturn)) static void leave(long status) {
    register long x0 __asm__("x0") = status;
    register long x8 __asm__("x8") = 93; // exit
    __asm__ volatile("svc #0" : : "r"(x0), "r"(x8));
    __builtin_unreachable();
}

void _start(void) {
    long bad = 0;
    if (*own_at() != 40) bad |= 1;
    *own_at() = 41;
    if (own != 41) bad |= 2;
    if (*zeroed_at() != 0) bad |= 4;
    *zeroed_at() = 7;
    if (zeroed != 7) bad |= 8;
    if (*imported_at() != 5) bad |= 16;
    *imported_at() = 6;
    if (imported != 6) bad |= 32;
    if ((void *)own_at() == (void *)zeroed_at() || (void *)own_at() == (void *)imported_at()) bad |= 64;
    if (imported_here() != imported_at()) bad |= 128;
    leave(bad);
}
