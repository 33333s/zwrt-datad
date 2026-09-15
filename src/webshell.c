#include "webshell.h"

#include <arpa/inet.h>
#include <ctype.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <termios.h>
#include <unistd.h>
#ifdef __APPLE__
#include <util.h>
#endif

#define WEBSHELL_IDLE_SECONDS (15 * 60)
#define WEBSHELL_FRAME_MAX 16384
#define WEBSHELL_PTY_READ_MAX 8192
#define WEBSHELL_DEFAULT_COLS 80
#define WEBSHELL_DEFAULT_ROWS 24

extern char **environ;

struct sha1_state {
    uint32_t h[5];
    uint64_t bytes;
    unsigned char block[64];
    size_t used;
};

static uint32_t rol32(uint32_t value, unsigned int bits)
{
    return (value << bits) | (value >> (32U - bits));
}

static void sha1_transform(struct sha1_state *state, const unsigned char block[64])
{
    uint32_t w[80], a, b, c, d, e;
    for (size_t i = 0; i < 16; i++) {
        size_t p = i * 4;
        w[i] = ((uint32_t)block[p] << 24) | ((uint32_t)block[p + 1] << 16) |
               ((uint32_t)block[p + 2] << 8) | block[p + 3];
    }
    for (size_t i = 16; i < 80; i++) w[i] = rol32(w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16], 1);
    a = state->h[0]; b = state->h[1]; c = state->h[2]; d = state->h[3]; e = state->h[4];
    for (size_t i = 0; i < 80; i++) {
        uint32_t f, k, next;
        if (i < 20) { f = (b & c) | ((~b) & d); k = 0x5a827999U; }
        else if (i < 40) { f = b ^ c ^ d; k = 0x6ed9eba1U; }
        else if (i < 60) { f = (b & c) | (b & d) | (c & d); k = 0x8f1bbcdcU; }
        else { f = b ^ c ^ d; k = 0xca62c1d6U; }
        next = rol32(a, 5) + f + e + k + w[i];
        e = d; d = c; c = rol32(b, 30); b = a; a = next;
    }
    state->h[0] += a; state->h[1] += b; state->h[2] += c;
    state->h[3] += d; state->h[4] += e;
}

static void sha1_init(struct sha1_state *state)
{
    memset(state, 0, sizeof *state);
    state->h[0] = 0x67452301U; state->h[1] = 0xefcdab89U;
    state->h[2] = 0x98badcfeU; state->h[3] = 0x10325476U;
    state->h[4] = 0xc3d2e1f0U;
}

static void sha1_update(struct sha1_state *state, const void *data, size_t len)
{
    const unsigned char *src = data;
    state->bytes += len;
    while (len) {
        size_t take = sizeof state->block - state->used;
        if (take > len) take = len;
        memcpy(state->block + state->used, src, take);
        state->used += take; src += take; len -= take;
        if (state->used == sizeof state->block) {
            sha1_transform(state, state->block);
            state->used = 0;
        }
    }
}

static void sha1_final(struct sha1_state *state, unsigned char out[20])
{
    uint64_t bits = state->bytes * 8U;
    unsigned char tail[72];
    size_t pad = state->used < 56 ? 56 - state->used : 120 - state->used;
    memset(tail, 0, sizeof tail);
    tail[0] = 0x80;
    for (size_t i = 0; i < 8; i++) tail[pad + i] = (unsigned char)(bits >> (56U - i * 8U));
    sha1_update(state, tail, pad + 8);
    for (size_t i = 0; i < 5; i++) {
        out[i * 4] = (unsigned char)(state->h[i] >> 24);
        out[i * 4 + 1] = (unsigned char)(state->h[i] >> 16);
        out[i * 4 + 2] = (unsigned char)(state->h[i] >> 8);
        out[i * 4 + 3] = (unsigned char)state->h[i];
    }
}

static int base64_encode(const unsigned char *src, size_t len, char *out, size_t out_len)
{
    static const char table[] = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    size_t need = ((len + 2) / 3) * 4, pos = 0;
    if (!out || out_len <= need) return 0;
    for (size_t i = 0; i < len; i += 3) {
        size_t remain = len - i;
        uint32_t value = (uint32_t)src[i] << 16;
        if (remain > 1) value |= (uint32_t)src[i + 1] << 8;
        if (remain > 2) value |= src[i + 2];
        out[pos++] = table[(value >> 18) & 63];
        out[pos++] = table[(value >> 12) & 63];
        out[pos++] = remain > 1 ? table[(value >> 6) & 63] : '=';
        out[pos++] = remain > 2 ? table[value & 63] : '=';
    }
    out[pos] = 0;
    return 1;
}

static int set_nonblocking_cloexec(int fd)
{
    int flags = fcntl(fd, F_GETFL, 0);
    int fd_flags = fcntl(fd, F_GETFD, 0);
    if (flags < 0 || fd_flags < 0 || fcntl(fd, F_SETFL, flags | O_NONBLOCK) < 0 ||
        fcntl(fd, F_SETFD, fd_flags | FD_CLOEXEC) < 0) return 0;
    return 1;
}

static const char *header_value(const char *request, const char *name,
                                char *out, size_t out_len)
{
    const char *line = request ? strstr(request, "\r\n") : NULL;
    size_t name_len = strlen(name);
    if (!line || !out || out_len < 2) return NULL;
    line += 2;
    while (*line && strncmp(line, "\r\n", 2)) {
        const char *end = strstr(line, "\r\n"), *value;
        size_t len;
        if (!end) return NULL;
        if ((size_t)(end - line) > name_len && !strncasecmp(line, name, name_len) &&
            line[name_len] == ':') {
            value = line + name_len + 1;
            while (value < end && isspace((unsigned char)*value)) value++;
            len = (size_t)(end - value);
            while (len && isspace((unsigned char)value[len - 1])) len--;
            if (len >= out_len) return NULL;
            memcpy(out, value, len); out[len] = 0;
            return out;
        }
        line = end + 2;
    }
    return NULL;
}

static int header_has_token(const char *value, const char *wanted)
{
    const char *p = value;
    size_t wanted_len = strlen(wanted);
    while (p && *p) {
        const char *end;
        while (*p == ',' || isspace((unsigned char)*p)) p++;
        end = strchr(p, ',');
        if (!end) end = p + strlen(p);
        while (end > p && isspace((unsigned char)end[-1])) end--;
        if ((size_t)(end - p) == wanted_len && !strncasecmp(p, wanted, wanted_len)) return 1;
        p = *end ? end + 1 : end;
    }
    return 0;
}

static int websocket_accept_value(const char *key, char out[32])
{
    static const char guid[] = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    static const char base64_table[] = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    struct sha1_state state;
    unsigned char digest[20];
    size_t key_len;
    if (!key) return 0;
    key_len = strlen(key);
    if (key_len != 24 || key[22] != '=' || key[23] != '=') return 0;
    for (size_t i = 0; i < 22; i++) {
        unsigned char c = (unsigned char)key[i];
        if (!(isalnum(c) || c == '+' || c == '/')) return 0;
    }
    /* A 16-byte nonce leaves four unused low bits in the final Base64 digit. */
    {
        const char *digit = strchr(base64_table, key[21]);
        if (!digit || (((unsigned int)(digit - base64_table)) & 0x0fU)) return 0;
    }
    sha1_init(&state);
    sha1_update(&state, key, key_len);
    sha1_update(&state, guid, sizeof guid - 1);
    sha1_final(&state, digest);
    return base64_encode(digest, sizeof digest, out, 32);
}

static int write_full(int fd, const char *data, size_t length)
{
    size_t offset = 0;
    while (offset < length) {
        ssize_t count = write(fd, data + offset, length - offset);
        if (count > 0) { offset += (size_t)count; continue; }
        if (count < 0 && errno == EINTR) continue;
        return 0;
    }
    return 1;
}

static int queue_frame(struct webshell_session *session, unsigned char opcode,
                       const unsigned char *payload, size_t payload_len)
{
    unsigned char header[10];
    size_t header_len;
    if (!session || payload_len > WEBSHELL_FRAME_MAX) return 0;
    header[0] = 0x80U | (opcode & 0x0fU);
    if (payload_len <= 125) {
        header[1] = (unsigned char)payload_len; header_len = 2;
    } else {
        header[1] = 126;
        header[2] = (unsigned char)(payload_len >> 8);
        header[3] = (unsigned char)payload_len;
        header_len = 4;
    }
    if (header_len + payload_len > sizeof session->net_out - session->net_out_len) return 0;
    memcpy(session->net_out + session->net_out_len, header, header_len);
    session->net_out_len += header_len;
    if (payload_len) {
        memcpy(session->net_out + session->net_out_len, payload, payload_len);
        session->net_out_len += payload_len;
    }
    return 1;
}

static void close_child_fds(void)
{
    DIR *dir = opendir("/proc/self/fd");
    if (dir) {
        int keep = dirfd(dir);
        struct dirent *entry;
        while ((entry = readdir(dir)) != NULL) {
            char *end = NULL;
            long fd = strtol(entry->d_name, &end, 10);
            if (end && !*end && fd > STDERR_FILENO && fd != keep) close((int)fd);
        }
        closedir(dir);
        return;
    }
    for (int fd = 3; fd < 1024; fd++) close(fd);
}

static const char *select_shell(void)
{
    static const char *const candidates[] = {"/system/bin/sh", "/bin/ash", "/bin/sh"};
    struct stat st;
    for (size_t i = 0; i < sizeof candidates / sizeof candidates[0]; i++)
        if (stat(candidates[i], &st) == 0 && S_ISREG(st.st_mode) &&
            access(candidates[i], X_OK) == 0) return candidates[i];
    return NULL;
}

static int set_window_size(int fd, unsigned int cols, unsigned int rows)
{
    struct winsize size;
    if (cols < 20 || cols > 500 || rows < 5 || rows > 300) return 0;
    memset(&size, 0, sizeof size);
    size.ws_col = (unsigned short)cols;
    size.ws_row = (unsigned short)rows;
    return ioctl(fd, TIOCSWINSZ, &size) == 0;
}

static int spawn_shell(struct webshell_session *session)
{
    const char *shell = select_shell();
    int master = -1, slave = -1;
    char slave_name[256];
    pid_t pid;
    if (!shell) return 0;
#ifdef __APPLE__
    if (openpty(&master, &slave, slave_name, NULL, NULL) < 0) return 0;
#else
    master = posix_openpt(O_RDWR | O_NOCTTY | O_CLOEXEC);
    if (master < 0 || grantpt(master) < 0 || unlockpt(master) < 0 ||
        ptsname_r(master, slave_name, sizeof slave_name) != 0) {
        if (master >= 0) close(master);
        return 0;
    }
#endif
    if (!set_window_size(master, WEBSHELL_DEFAULT_COLS, WEBSHELL_DEFAULT_ROWS)) {
        close(master);
#ifdef __APPLE__
        close(slave);
#endif
        return 0;
    }
    pid = fork();
    if (pid < 0) {
        close(master);
#ifdef __APPLE__
        close(slave);
#endif
        return 0;
    }
    if (pid == 0) {
        if (setsid() < 0) _exit(126);
#ifndef __APPLE__
        slave = open(slave_name, O_RDWR | O_NOCTTY);
        if (slave < 0) _exit(126);
#endif
        if (ioctl(slave, TIOCSCTTY, 0) < 0 || dup2(slave, STDIN_FILENO) < 0 ||
            dup2(slave, STDOUT_FILENO) < 0 || dup2(slave, STDERR_FILENO) < 0) _exit(126);
        if (slave > STDERR_FILENO) close(slave);
        close_child_fds();
        if (environ) environ[0] = NULL;
        if (setenv("TERM", "xterm-256color", 1) != 0 ||
            setenv("HOME", "/root", 1) != 0 ||
            setenv("PATH", "/usr/sbin:/usr/bin:/sbin:/bin:/system/bin", 1) != 0 ||
            setenv("HISTFILE", "/dev/null", 1) != 0) _exit(126);
        execl(shell, shell, "-i", (char *)NULL);
        _exit(127);
    }
#ifdef __APPLE__
    close(slave);
#endif
    if (!set_nonblocking_cloexec(master)) {
        close(master); kill(pid, SIGKILL); (void)waitpid(pid, NULL, 0); return 0;
    }
    session->pty_fd = master;
    session->child_pid = pid;
    return 1;
}

static void session_close(struct webshell_session *session)
{
    if (!session) return;
    if (session->client_fd >= 0) close(session->client_fd);
    if (session->pty_fd >= 0) close(session->pty_fd);
    if (session->child_pid > 0) {
        kill(-session->child_pid, SIGHUP);
        kill(session->child_pid, SIGKILL);
        while (waitpid(session->child_pid, NULL, 0) < 0 && errno == EINTR) {}
    }
    memset(session, 0, sizeof *session);
    session->client_fd = -1;
    session->pty_fd = -1;
    session->child_pid = -1;
}

static int parse_resize(const unsigned char *payload, size_t len,
                        unsigned int *cols, unsigned int *rows)
{
    char text[256], type[32] = "";
    unsigned int c = 0, r = 0;
    int consumed = 0;
    if (!payload || len == 0 || len >= sizeof text || memchr(payload, 0, len)) return 0;
    memcpy(text, payload, len); text[len] = 0;
    if (sscanf(text, "{\"type\":\"%31[^\"]\",\"cols\":%u,\"rows\":%u}%n",
               type, &c, &r, &consumed) != 3 || consumed != (int)len ||
        strcmp(type, "resize") || c < 20 || c > 500 || r < 5 || r > 300) return 0;
    *cols = c; *rows = r;
    return 1;
}

static int consume_frames(struct webshell_session *session)
{
    size_t offset = 0;
    while (session->net_in_len - offset >= 2) {
        const unsigned char *frame = session->net_in + offset;
        unsigned char opcode = frame[0] & 0x0fU;
        int fin = (frame[0] & 0x80U) != 0;
        int masked = (frame[1] & 0x80U) != 0;
        uint64_t payload_len = frame[1] & 0x7fU;
        size_t header_len = 2;
        unsigned char payload[WEBSHELL_FRAME_MAX];
        if ((frame[0] & 0x70U) || !fin || !masked) return 0;
        if (payload_len == 126) {
            if (session->net_in_len - offset < 4) break;
            payload_len = ((uint64_t)frame[2] << 8) | frame[3];
            header_len = 4;
        } else if (payload_len == 127) {
            if (session->net_in_len - offset < 10) break;
            if (frame[2] || frame[3] || frame[4] || frame[5]) return 0;
            payload_len = ((uint64_t)frame[6] << 24) | ((uint64_t)frame[7] << 16) |
                          ((uint64_t)frame[8] << 8) | frame[9];
            header_len = 10;
        }
        if (payload_len > WEBSHELL_FRAME_MAX ||
            ((opcode & 0x08U) && payload_len > 125)) return 0;
        if (session->net_in_len - offset < header_len + 4 + (size_t)payload_len) break;
        const unsigned char *mask = frame + header_len;
        const unsigned char *encoded = mask + 4;
        for (size_t i = 0; i < (size_t)payload_len; i++) payload[i] = encoded[i] ^ mask[i & 3U];
        if (opcode == 0x8) return 0;
        if (opcode == 0x9) {
            if (!queue_frame(session, 0xA, payload, (size_t)payload_len)) return 0;
        } else if (opcode == 0x2) {
            if (payload_len > sizeof session->pty_in - session->pty_in_len) return 0;
            memcpy(session->pty_in + session->pty_in_len, payload, (size_t)payload_len);
            session->pty_in_len += (size_t)payload_len;
        } else if (opcode == 0x1) {
            unsigned int cols, rows;
            if (!parse_resize(payload, (size_t)payload_len, &cols, &rows) ||
                !set_window_size(session->pty_fd, cols, rows)) return 0;
        } else if (opcode != 0xA) {
            return 0;
        }
        offset += header_len + 4 + (size_t)payload_len;
        session->last_activity = time(NULL);
    }
    if (offset) {
        memmove(session->net_in, session->net_in + offset, session->net_in_len - offset);
        session->net_in_len -= offset;
    }
    return 1;
}

void webshell_manager_init(struct webshell_manager *manager, int enabled)
{
    if (!manager) return;
    memset(manager, 0, sizeof *manager);
    manager->enabled = enabled;
    for (size_t i = 0; i < WEBSHELL_MAX_SESSIONS; i++) {
        manager->sessions[i].client_fd = -1;
        manager->sessions[i].pty_fd = -1;
        manager->sessions[i].child_pid = -1;
    }
}

int webshell_upgrade(struct webshell_manager *manager, int client_fd, const char *request)
{
    char upgrade[64] = "", connection[128] = "", version[32] = "";
    char key[64] = "", accept[32] = "", response[256];
    struct webshell_session *session = NULL;
    int response_len;
    if (!manager || !manager->enabled) return -2;
    for (size_t i = 0; i < WEBSHELL_MAX_SESSIONS; i++)
        if (manager->sessions[i].client_fd < 0) { session = &manager->sessions[i]; break; }
    if (!session) return -3;
    if (!header_value(request, "Upgrade", upgrade, sizeof upgrade) || strcasecmp(upgrade, "websocket") ||
        !header_value(request, "Connection", connection, sizeof connection) || !header_has_token(connection, "upgrade") ||
        !header_value(request, "Sec-WebSocket-Version", version, sizeof version) || strcmp(version, "13") ||
        !header_value(request, "Sec-WebSocket-Key", key, sizeof key) || !websocket_accept_value(key, accept)) return 0;
    memset(session, 0, sizeof *session);
    session->client_fd = client_fd;
    session->pty_fd = -1;
    session->child_pid = -1;
    session->last_activity = time(NULL);
    if (!spawn_shell(session)) {
        session_close(session);
        return -1;
    }
    response_len = snprintf(response, sizeof response,
                            "HTTP/1.1 101 Switching Protocols\r\n"
                            "Upgrade: websocket\r\n"
                            "Connection: Upgrade\r\n"
                            "Sec-WebSocket-Accept: %s\r\n"
                            "Cache-Control: no-store\r\n\r\n", accept);
    if (response_len <= 0 || (size_t)response_len >= sizeof response ||
        !write_full(client_fd, response, (size_t)response_len) ||
        !set_nonblocking_cloexec(client_fd)) {
        session_close(session);
        return -1;
    }
    {
        static const unsigned char ready[] = "{\"type\":\"ready\",\"cols\":80,\"rows\":24}";
        if (!queue_frame(session, 0x1, ready, sizeof ready - 1)) {
            session_close(session);
            return -1;
        }
    }
    return 1;
}

void webshell_add_fds(struct webshell_manager *manager, fd_set *read_fds,
                      fd_set *write_fds, int *max_fd)
{
    if (!manager || !manager->enabled) return;
    for (size_t i = 0; i < WEBSHELL_MAX_SESSIONS; i++) {
        struct webshell_session *session = &manager->sessions[i];
        if (session->client_fd < 0) continue;
        if (session->net_in_len < sizeof session->net_in) FD_SET(session->client_fd, read_fds);
        if (session->net_out_len) FD_SET(session->client_fd, write_fds);
        if (session->pty_in_len) FD_SET(session->pty_fd, write_fds);
        if (session->net_out_len + WEBSHELL_PTY_READ_MAX + 4 <= sizeof session->net_out)
            FD_SET(session->pty_fd, read_fds);
        if (session->client_fd > *max_fd) *max_fd = session->client_fd;
        if (session->pty_fd > *max_fd) *max_fd = session->pty_fd;
    }
}

static int flush_buffer(int fd, unsigned char *buffer, size_t *length)
{
    ssize_t count;
    if (!*length) return 1;
    count = write(fd, buffer, *length);
    if (count < 0) return errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK;
    if (count == 0) return 0;
    memmove(buffer, buffer + count, *length - (size_t)count);
    *length -= (size_t)count;
    return 1;
}

void webshell_process_ready(struct webshell_manager *manager,
                            const fd_set *read_fds, const fd_set *write_fds)
{
    if (!manager || !manager->enabled) return;
    for (size_t i = 0; i < WEBSHELL_MAX_SESSIONS; i++) {
        struct webshell_session *session = &manager->sessions[i];
        if (session->client_fd < 0) continue;
        if (FD_ISSET(session->client_fd, write_fds) &&
            !flush_buffer(session->client_fd, session->net_out, &session->net_out_len)) {
            session_close(session); continue;
        }
        if (session->client_fd < 0) continue;
        if (FD_ISSET(session->pty_fd, write_fds) &&
            !flush_buffer(session->pty_fd, session->pty_in, &session->pty_in_len)) {
            session_close(session); continue;
        }
        if (session->client_fd < 0) continue;
        if (FD_ISSET(session->client_fd, read_fds)) {
            ssize_t count = read(session->client_fd, session->net_in + session->net_in_len,
                                 sizeof session->net_in - session->net_in_len);
            if (count <= 0 && !(count < 0 && (errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK))) {
                session_close(session); continue;
            }
            if (count > 0) {
                session->net_in_len += (size_t)count;
                if (!consume_frames(session)) { session_close(session); continue; }
            }
        }
        if (session->client_fd < 0) continue;
        if (FD_ISSET(session->pty_fd, read_fds)) {
            unsigned char payload[WEBSHELL_PTY_READ_MAX];
            ssize_t count = read(session->pty_fd, payload, sizeof payload);
            if (count <= 0 && !(count < 0 && (errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK))) {
                session_close(session); continue;
            }
            if (count > 0) {
                if (!queue_frame(session, 0x2, payload, (size_t)count)) {
                    session_close(session); continue;
                }
                session->last_activity = time(NULL);
            }
        }
    }
}

void webshell_expire(struct webshell_manager *manager, time_t now)
{
    if (!manager || !manager->enabled) return;
    for (size_t i = 0; i < WEBSHELL_MAX_SESSIONS; i++) {
        struct webshell_session *session = &manager->sessions[i];
        int status;
        if (session->client_fd < 0) continue;
        if (waitpid(session->child_pid, &status, WNOHANG) == session->child_pid) {
            session->child_pid = -1;
            session_close(session);
        } else if (now - session->last_activity >= WEBSHELL_IDLE_SECONDS) {
            session_close(session);
        }
    }
}

void webshell_stop(struct webshell_manager *manager)
{
    if (!manager) return;
    for (size_t i = 0; i < WEBSHELL_MAX_SESSIONS; i++) session_close(&manager->sessions[i]);
}

size_t webshell_status_json(const struct webshell_manager *manager,
                            char *out, size_t out_len)
{
    size_t active = 0;
    int count;
    if (manager)
        for (size_t i = 0; i < WEBSHELL_MAX_SESSIONS; i++)
            if (manager->sessions[i].client_fd >= 0) active++;
    count = snprintf(out, out_len,
                     "{\"enabled\":%s,\"active_sessions\":%zu,\"max_sessions\":%d,"
                     "\"protocol\":\"websocket-binary-v1\"}\n",
                     manager && manager->enabled ? "true" : "false", active, WEBSHELL_MAX_SESSIONS);
    return count > 0 && (size_t)count < out_len ? (size_t)count : 0;
}
