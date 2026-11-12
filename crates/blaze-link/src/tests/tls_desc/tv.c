// The shared library main.c imports its thread-local from. `imported_here` is where the
// library itself finds it, through a descriptor the loader resolves.
//
//   gcc -c -O1 -fPIC -o tv.o tv.c
__thread int imported = 5;

int *imported_here(void) { return &imported; }
