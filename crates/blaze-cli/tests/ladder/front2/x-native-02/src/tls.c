/* x-native-02: a C thread-local set by a C constructor. `init` runs when the image is loaded,
   before `main`, on the first thread (GCC/Clang `constructor` attribute: Mach-O
   `__mod_init_func`, ELF `.init_array`), so the first thread's `n` is 5 when Rust reads it.
   `n` is `__thread` (Mach-O `__thread_vars`/`__thread_bss`, ELF `.tbss`): each thread's copy
   starts at zero; only the first thread's was set. */

__thread int n;

__attribute__((constructor)) static void init(void) { n = 5; }

int read_n(void) { return n; }
