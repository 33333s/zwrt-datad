#ifndef ZWRT_DATAD_WEBSHELL_H
#define ZWRT_DATAD_WEBSHELL_H

#include <stddef.h>
#include <sys/select.h>
#include <sys/types.h>
#include <time.h>

#define WEBSHELL_MAX_SESSIONS 4
#define WEBSHELL_NET_IN_MAX 65536
#define WEBSHELL_NET_OUT_MAX 65536
#define WEBSHELL_PTY_IN_MAX 32768

struct webshell_session {
    int client_fd;
    int pty_fd;
    pid_t child_pid;
    time_t last_activity;
    unsigned char net_in[WEBSHELL_NET_IN_MAX];
    size_t net_in_len;
    unsigned char net_out[WEBSHELL_NET_OUT_MAX];
    size_t net_out_len;
    unsigned char pty_in[WEBSHELL_PTY_IN_MAX];
    size_t pty_in_len;
};

struct webshell_manager {
    int enabled;
    struct webshell_session sessions[WEBSHELL_MAX_SESSIONS];
};

void webshell_manager_init(struct webshell_manager *manager, int enabled);
int webshell_upgrade(struct webshell_manager *manager, int client_fd, const char *request);
void webshell_add_fds(struct webshell_manager *manager, fd_set *read_fds,
                      fd_set *write_fds, int *max_fd);
void webshell_process_ready(struct webshell_manager *manager,
                            const fd_set *read_fds, const fd_set *write_fds);
void webshell_expire(struct webshell_manager *manager, time_t now);
void webshell_stop(struct webshell_manager *manager);
size_t webshell_status_json(const struct webshell_manager *manager,
                            char *out, size_t out_len);

#endif
