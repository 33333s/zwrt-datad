#ifndef CLOUD_PROXY_H
#define CLOUD_PROXY_H
void cloud_proxy(int client, const char *method, const char *path, const char *body);
int cloud_runtime_start(const char *data_dir, const char *state_url);
void cloud_runtime_stop(void);
#endif
