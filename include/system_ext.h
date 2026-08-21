/* SPDX-License-Identifier: MIT */
#ifndef ZWRT_SYSTEM_EXT_H
#define ZWRT_SYSTEM_EXT_H

#include <stddef.h>

/* Build realtime system data plus an optional normalized thermal-zone array. */
int system_ext_build_json(char *out, size_t outlen,
                          char *thermal_out, size_t thermal_outlen);

#endif
