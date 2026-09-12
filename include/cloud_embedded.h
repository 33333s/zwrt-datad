#ifndef CLOUD_EMBEDDED_H
#define CLOUD_EMBEDDED_H

int CloudStart(char *data_dir, char *state_url);
void CloudStop(void);
char *CloudHandle(char *method, char *path, char *body, int *status);
void CloudFree(char *reply);

#endif
