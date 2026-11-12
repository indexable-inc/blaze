// The shared library main.c imports its thread-local from.
//
//   gcc -c -O1 -fPIC -o tv.o tv.c
__thread int imported = 5;
