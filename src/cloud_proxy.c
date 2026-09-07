#include "cloud_proxy.h"
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/time.h>
#include <unistd.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <fcntl.h>

static int send_all(int fd, const char *p, size_t n) {
 while(n) {ssize_t w=send(fd,p,n,0);if(w<0&&errno==EINTR)continue;if(w<=0)return -1;p+=w;n-=(size_t)w;}return 0;
}
void cloud_proxy(int client,const char *method,const char *path,const char *body){
 int fd=-1;char header[512],reply[49152];size_t used=0;struct sockaddr_un addr;struct timeval tv={0,300000};
 const char *sock=getenv("ZWRT_DATAD_CLOUD_SOCKET");if(!sock||!*sock)sock="/data/zwrt-datad/cloud.sock";
 memset(&addr,0,sizeof addr);addr.sun_family=AF_UNIX;
 if(strlen(sock)>=sizeof addr.sun_path)goto unavailable;
 memcpy(addr.sun_path,sock,strlen(sock)+1);
 fd=socket(AF_UNIX,SOCK_STREAM,0);if(fd<0)goto unavailable;
 fcntl(fd,F_SETFD,FD_CLOEXEC);setsockopt(fd,SOL_SOCKET,SO_RCVTIMEO,&tv,sizeof tv);setsockopt(fd,SOL_SOCKET,SO_SNDTIMEO,&tv,sizeof tv);
 if(connect(fd,(struct sockaddr *)&addr,sizeof addr)<0)goto unavailable;
 size_t size=body?strlen(body):0;
 int len=snprintf(header,sizeof header,"%s %s HTTP/1.0\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: %zu\r\nConnection: close\r\n\r\n",method,path,size);
 if(len<0||(size_t)len>=sizeof header||send_all(fd,header,(size_t)len)<0||(size&&send_all(fd,body,size)<0))goto unavailable;
 for(;;){ssize_t n=recv(fd,reply+used,sizeof reply-used,0);if(n<0&&errno==EINTR)continue;if(n<0)goto unavailable;if(n==0)break;used+=(size_t)n;if(used==sizeof reply)goto unavailable;}
 close(fd);send_all(client,reply,used);return;
unavailable:
 if(fd>=0)close(fd);
 const char *error="HTTP/1.0 503 Service Unavailable\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{\"error\":\"datad cloud worker unavailable\"}";
 send_all(client,error,strlen(error));
}
