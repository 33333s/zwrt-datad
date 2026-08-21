/* SPDX-License-Identifier: MIT */
#include "device_exec.h"

#include <ctype.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/select.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef DEVICE_COMMAND_TIMEOUT_MS
#define DEVICE_COMMAND_TIMEOUT_MS 5000
#endif

static int valid_command_name(const char *s)
{
    if (!s || !*s) return 0;
    for (; *s; s++) {
        unsigned char c = (unsigned char)*s;
        if (!isalnum(c) && c != '_' && c != '-' && c != '.') return 0;
    }
    return 1;
}

static long long monotonic_ms(void)
{
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) return 0;
    return (long long)ts.tv_sec * 1000LL + ts.tv_nsec / 1000000LL;
}

static int wait_child(pid_t pid, int *status, int timeout_ms)
{
    long long deadline = monotonic_ms() + timeout_ms;
    for (;;) {
        pid_t rc = waitpid(pid, status, WNOHANG);
        if (rc == pid) return 0;
        if (rc < 0 && errno != EINTR) return -1;
        if (monotonic_ms() >= deadline) {
            kill(pid, SIGKILL);
            while (waitpid(pid, status, 0) < 0 && errno == EINTR) {}
            errno = ETIMEDOUT;
            return -1;
        }
        struct timespec pause = {0, 10 * 1000 * 1000};
        nanosleep(&pause, NULL);
    }
}

int device_run_capture(const char *const argv[], char *out, size_t outlen)
{
    int pipefd[2];
    pid_t pid;
    size_t used = 0;
    int status = 0;
    int timed_out = 0;
    long long deadline;

    if (!argv || !argv[0] || !out || outlen == 0) return -1;
    out[0] = 0;
    if (pipe(pipefd) != 0) return -1;

    pid = fork();
    if (pid < 0) {
        close(pipefd[0]);
        close(pipefd[1]);
        return -1;
    }
    if (pid == 0) {
        int devnull = open("/dev/null", O_WRONLY);
        close(pipefd[0]);
        if (dup2(pipefd[1], STDOUT_FILENO) < 0) _exit(126);
        if (devnull >= 0) {
            (void)dup2(devnull, STDERR_FILENO);
            if (devnull > STDERR_FILENO) close(devnull);
        }
        if (pipefd[1] > STDOUT_FILENO) close(pipefd[1]);
        execvp(argv[0], (char *const *)argv);
        _exit(127);
    }

    close(pipefd[1]);
    (void)fcntl(pipefd[0], F_SETFL, fcntl(pipefd[0], F_GETFL, 0) | O_NONBLOCK);
    deadline = monotonic_ms() + DEVICE_COMMAND_TIMEOUT_MS;

    for (;;) {
        char scratch[1024];
        char *dst = used + 1 < outlen ? out + used : scratch;
        size_t room = used + 1 < outlen ? outlen - 1 - used : sizeof scratch;
        if (monotonic_ms() >= deadline) {
            kill(pid, SIGKILL);
            timed_out = 1;
            break;
        }
        ssize_t n = read(pipefd[0], dst, room);
        if (n > 0) {
            if (dst != scratch) used += (size_t)n;
            continue;
        }
        if (n == 0) break;
        if (errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR) break;

        long long remain = deadline - monotonic_ms();
        if (remain <= 0) {
            kill(pid, SIGKILL);
            timed_out = 1;
            break;
        }
        fd_set rfds;
        struct timeval tv;
        FD_ZERO(&rfds);
        FD_SET(pipefd[0], &rfds);
        tv.tv_sec = (time_t)(remain / 1000);
        tv.tv_usec = (suseconds_t)((remain % 1000) * 1000);
        (void)select(pipefd[0] + 1, &rfds, NULL, NULL, &tv);
    }
    close(pipefd[0]);
    out[used] = 0;

    if (timed_out) {
        while (waitpid(pid, &status, 0) < 0 && errno == EINTR) {}
        errno = ETIMEDOUT;
        return -1;
    }
    if (wait_child(pid, &status, DEVICE_COMMAND_TIMEOUT_MS) != 0) return -1;
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) return -1;
    return 0;
}

int device_run_quiet(const char *const argv[])
{
    char ignored[2];
    return device_run_capture(argv, ignored, sizeof ignored);
}

int device_ubus_call(const char *service, const char *method, const char *args,
                     char *out, size_t outlen)
{
    const char *ubus = getenv("ZWRT_DATAD_UBUS_BIN");
    const char *argv[8];
    size_t n = 0;

    if (!valid_command_name(service) || !valid_command_name(method)) return -1;
    if (!ubus || !*ubus) ubus = "ubus";
    argv[n++] = ubus;
    argv[n++] = "-t";
    argv[n++] = "3";
    argv[n++] = "call";
    argv[n++] = service;
    argv[n++] = method;
    if (args && *args) argv[n++] = args;
    argv[n] = NULL;
    if (device_run_capture(argv, out, outlen) != 0) return -1;
    return out[0] ? 0 : -1;
}

int device_uci_commit(const char *package_name)
{
    const char *uci = getenv("ZWRT_DATAD_UCI_BIN");
    const char *argv[4];
    if (!valid_command_name(package_name)) return -1;
    if (!uci || !*uci) uci = "uci";
    argv[0] = uci;
    argv[1] = "commit";
    argv[2] = package_name;
    argv[3] = NULL;
    return device_run_quiet(argv);
}

int device_uci_revert(const char *package_name)
{
    const char *uci = getenv("ZWRT_DATAD_UCI_BIN");
    const char *argv[4];
    if (!valid_command_name(package_name)) return -1;
    if (!uci || !*uci) uci = "uci";
    argv[0] = uci;
    argv[1] = "revert";
    argv[2] = package_name;
    argv[3] = NULL;
    return device_run_quiet(argv);
}

static int valid_uci_path(const char *s)
{
    if (!s || !*s) return 0;
    for (; *s; s++) {
        unsigned char c = (unsigned char)*s;
        if (!isalnum(c) && c != '_' && c != '-' && c != '.' && c != '@' && c != '[' && c != ']')
            return 0;
    }
    return 1;
}

int device_uci_set(const char *path, const char *value)
{
    const char *uci = getenv("ZWRT_DATAD_UCI_BIN");
    const char *argv[4];
    char assignment[2048];
    if (!valid_uci_path(path) || !value) return -1;
    if (snprintf(assignment, sizeof assignment, "%s=%s", path, value) >= (int)sizeof assignment) return -1;
    if (!uci || !*uci) uci = "uci";
    argv[0] = uci;
    argv[1] = "set";
    argv[2] = assignment;
    argv[3] = NULL;
    return device_run_quiet(argv);
}

int device_uci_get(const char *path, char *out, size_t outlen)
{
    const char *uci = getenv("ZWRT_DATAD_UCI_BIN");
    size_t n;
    if (!valid_uci_path(path) || !out || outlen == 0) return -1;
    if (!uci || !*uci) uci = "uci";
    {
        const char *full_argv[] = {uci, "-q", "get", path, NULL};
        if (device_run_capture(full_argv, out, outlen) != 0) return -1;
    }
    n = strlen(out);
    while (n && isspace((unsigned char)out[n - 1])) out[--n] = 0;
    return out[0] ? 0 : -1;
}

int device_uci_list(const char *operation, const char *path, const char *value)
{
    const char *uci = getenv("ZWRT_DATAD_UCI_BIN");
    const char *argv[4];
    char assignment[2048];
    if ((!operation || (strcmp(operation, "add_list") && strcmp(operation, "del_list"))) ||
        !valid_uci_path(path) || !value) return -1;
    if (snprintf(assignment, sizeof assignment, "%s=%s", path, value) >= (int)sizeof assignment) return -1;
    if (!uci || !*uci) uci = "uci";
    argv[0] = uci;
    argv[1] = operation;
    argv[2] = assignment;
    argv[3] = NULL;
    return device_run_quiet(argv);
}

int device_wifi_reload(void)
{
    char out[512];
    const char *wifi = getenv("ZWRT_DATAD_WIFI_BIN");
    const char *argv[3];
    if (device_ubus_call("zwrt_wlan", "reload", "{}", out, sizeof out) == 0) return 0;
    if (!wifi || !*wifi) wifi = "wifi";
    argv[0] = wifi;
    argv[1] = "reload";
    argv[2] = NULL;
    return device_run_quiet(argv);
}
