/* SPDX-License-Identifier: MIT */
#include "device_exec.h"

#include <assert.h>
#include <errno.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static long long monotonic_ms(void)
{
    struct timespec ts;
    assert(clock_gettime(CLOCK_MONOTONIC, &ts) == 0);
    return (long long)ts.tv_sec * 1000LL + ts.tv_nsec / 1000000LL;
}

int main(void)
{
    char out[16];
    const char *ok_argv[] = {"/bin/sh", "-c", "printf ok", NULL};
    const char *noisy_argv[] = {"/bin/sh", "-c", "while :; do printf x; done", NULL};
    long long started;

    assert(device_run_capture(ok_argv, out, sizeof out) == 0);
    assert(strcmp(out, "ok") == 0);

    started = monotonic_ms();
    errno = 0;
    assert(device_run_capture(noisy_argv, out, sizeof out) == -1);
    assert(errno == ETIMEDOUT);
    assert(monotonic_ms() - started < 2000);

    assert(setenv("ZWRT_DATAD_UBUS_BIN", "/usr/bin/true", 1) == 0);
    assert(device_ubus_call_raw("fixture", "empty", NULL, out, sizeof out) == 0);
    assert(out[0] == 0);
    assert(device_ubus_call("fixture", "empty", NULL, out, sizeof out) == -1);
    assert(unsetenv("ZWRT_DATAD_UBUS_BIN") == 0);
    return 0;
}
