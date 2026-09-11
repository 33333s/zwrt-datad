/* SPDX-License-Identifier: MIT */
#ifndef ZWRT_WIFI_CONTROL_H
#define ZWRT_WIFI_CONTROL_H
#include <stddef.h>
int wifi_control_execute(const char *action, const char *params, char *result, size_t len,
                         char *err, size_t errlen);
void wifi_runtime_tick(void);
#endif
