/* SPDX-License-Identifier: MIT */
#ifndef ZWRT_CONTROL_H
#define ZWRT_CONTROL_H

#include <stddef.h>

struct control_result {
    int http_status;
    int refresh_state;
};

const char *control_capabilities_json(void);
void control_restore_cooling_state(void);
void control_cooling_tick(long temperature_celsius);
void control_release_cooling_state(void);
struct control_result control_execute(const char *request_json,
                                      char *response, size_t response_len);

#endif
