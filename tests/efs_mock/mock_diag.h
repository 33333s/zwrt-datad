#ifndef MOCK_DIAG_H
#define MOCK_DIAG_H
#include <stdint.h>
#include <stdio.h>
#define RTLD_NOW 1
#define RTLD_GLOBAL 2
void *mock_dlopen(const char *, int);
void *mock_dlsym(void *, const char *);
const char *mock_dlerror(void);
uint64_t mock_clock_ms(void);
void mock_wait_tick(void);
#define dlopen mock_dlopen
#define dlsym mock_dlsym
#define dlerror mock_dlerror
#ifdef _WIN32
int mock_binary_open(const char *, int, int);
FILE *mock_tmpfile(void);
#define open mock_binary_open
#define tmpfile mock_tmpfile
#endif
#endif
