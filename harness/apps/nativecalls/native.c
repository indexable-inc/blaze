/* Calls a function of the program by its unmangled name, as bun's C and C++ call its
   `#[no_mangle]` functions. */
#include <stdint.h>

uint64_t nativecalls_step(uint64_t);

uint64_t nativecalls_iterate(uint64_t seed, uint64_t rounds) {
    uint64_t x = seed;
    for (uint64_t i = 0; i < rounds; i++) {
        x = nativecalls_step(x);
    }
    return x;
}
