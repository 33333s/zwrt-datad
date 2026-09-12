#include "cloud_proxy.h"

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>

#ifdef CLOUD_EMBEDDED
#include "cloud_embedded.h"
#endif

static int send_all(int fd, const char *p, size_t n) {
    while (n) {
        ssize_t written = send(fd, p, n, 0);
        if (written < 0 && errno == EINTR) continue;
        if (written <= 0) return -1;
        p += written;
        n -= (size_t)written;
    }
    return 0;
}

static const char *status_text(int status) {
    switch (status) {
        case 200: return "OK";
        case 400: return "Bad Request";
        case 403: return "Forbidden";
        case 404: return "Not Found";
        case 405: return "Method Not Allowed";
        case 500: return "Internal Server Error";
        default: return "Service Unavailable";
    }
}

int cloud_runtime_start(const char *data_dir, const char *state_url) {
#ifdef CLOUD_EMBEDDED
    return CloudStart((char *)data_dir, (char *)state_url);
#else
    (void)data_dir;
    (void)state_url;
    return 0;
#endif
}

void cloud_runtime_stop(void) {
#ifdef CLOUD_EMBEDDED
    CloudStop();
#endif
}

void cloud_proxy(int client, const char *method, const char *path, const char *body) {
    int status = 503;
    const char *reply = "{\"error\":\"datad cloud runtime is not included in this build\"}\n";
#ifdef CLOUD_EMBEDDED
    char *owned = CloudHandle((char *)method, (char *)path, (char *)(body ? body : ""), &status);
    if (owned) reply = owned;
#else
    (void)method;
    (void)path;
    (void)body;
#endif
    size_t size = strlen(reply);
    char header[320];
    int length = snprintf(header, sizeof header,
                          "HTTP/1.0 %d %s\r\n"
                          "Content-Type: application/json\r\n"
                          "Cache-Control: no-store\r\n"
                          "Content-Length: %zu\r\n"
                          "Connection: close\r\n\r\n",
                          status, status_text(status), size);
    if (length > 0 && (size_t)length < sizeof header) {
        (void)send_all(client, header, (size_t)length);
        (void)send_all(client, reply, size);
    }
#ifdef CLOUD_EMBEDDED
    if (owned) CloudFree(owned);
#endif
}
