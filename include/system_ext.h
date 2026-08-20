/* SPDX-License-Identifier: MIT */
#ifndef ZWRT_SYSTEM_EXT_H
#define ZWRT_SYSTEM_EXT_H

#include <stddef.h>

/* Build the realtime system extension object and return aggregate CPU usage. */
int system_ext_build_json(char *out, size_t outlen);

#endif
