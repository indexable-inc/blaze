#pragma once
int backtrace(void **buf, int size);
char **backtrace_symbols(void *const *buf, int size);
