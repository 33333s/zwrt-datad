#ifndef PASSWORD_AUTH_H
#define PASSWORD_AUTH_H
#include <stddef.h>
void password_sha256_hex(const void *data, size_t length, char out[65]);
/* Vendor login: SHA256(uppercase SHA256(password) + fresh web_login_info salt). */
int password_vendor_response(const char *password, const char *salt, char out[65]);
#endif
