/*
 * Safe process helpers for device-side commands.
 *
 * Commands are executed with fork/exec rather than through a shell, so JSON
 * values received by the private control API cannot escape into shell syntax.
 *
 * SPDX-License-Identifier: MIT
 */
#ifndef ZWRT_DEVICE_EXEC_H
#define ZWRT_DEVICE_EXEC_H

#include <stddef.h>

int device_run_capture(const char *const argv[], char *out, size_t outlen);
int device_run_quiet(const char *const argv[]);
int device_ubus_call(const char *service, const char *method, const char *args,
                     char *out, size_t outlen);
int device_uci_set(const char *path, const char *value);
int device_uci_get(const char *path, char *out, size_t outlen);
int device_uci_list(const char *operation, const char *path, const char *value);
int device_uci_commit(const char *package_name);
int device_wifi_reload(void);

#endif
