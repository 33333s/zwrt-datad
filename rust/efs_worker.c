// Crash-isolated Linux libdiag EFS worker. See docs/EFS.md.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <stdarg.h>
#include <stdatomic.h>
#include <errno.h>
#include <limits.h>
#include <time.h>
#include <unistd.h>
#include <fcntl.h>
#ifdef DIAG_EFS_TEST
#include "../tests/efs_mock/mock_diag.h"
#else
#include <dlfcn.h>
#include <signal.h>
#endif

#define DIAG_OK 1001
#define MAX_PKT 1024
#define MAX_RX 2048
#define READ_CHUNK 160
#define WRITE_CHUNK 150
#define MAX_PREVIEW (1024U * 1024U)
#define MAX_ENTRIES 4096U
#define MAX_TRANSFER (1024U * 1024U)
enum { E_OPEN=2, E_CLOSE=3, E_READ=4, E_WRITE=5, E_UNLINK=8,
       E_MKDIR=9, E_RMDIR=10, E_OPENDIR=11, E_READDIR=12,
       E_CLOSEDIR=13, E_RENAME=14, E_STAT=15 };
typedef int (*init_fn)(void *);
typedef void (*deinit_fn)(void);
typedef int (*register_fn)(int *, void *, void *, int *);
typedef int (*support_fn)(void *);
typedef int (*send_fn)(int, void *, int, void *, int,
                       void (*)(unsigned char *, int), void *);
typedef int (*release_fn)(int *);
static init_fn diag_init_p;
static deinit_fn diag_deinit_p;
static register_fn diag_register_p;
static support_fn diag_support_p;
static send_fn diag_send_p;
static release_fn diag_release_p;
static int client, inited, registered, debug_enabled, json_mode;
static int sig_number=18;
static uint8_t support[256], rx[MAX_RX];
static int rx_len, rx_bad;
static atomic_int reply_ready;
static FILE *data_out, *result_out;
static const char *command="";
static struct {
    const char *code;
    char message[512];
    uint32_t efs_errno;
    int diag_status, local_errno;
    uint64_t transferred;
} error_info;
typedef struct { uint8_t bytes[MAX_PKT]; size_t len; uint16_t op; } Packet;

static int fail(const char *code, const char *fmt, ...) {
    if (!error_info.code) {
        error_info.code=code;
        va_list ap; va_start(ap,fmt);
        vsnprintf(error_info.message,sizeof(error_info.message),fmt,ap);
        va_end(ap);
    }
    return 0;
}
static int local_fail(const char *stage) {
    int e=errno;
    if (!error_info.code) error_info.local_errno=e;
    return fail("LOCAL_IO_ERROR","%s: %s",stage,strerror(e));
}
static void trace(const char *stage) {
    if (debug_enabled) { fprintf(stderr,"[diag] %s\n",stage); fflush(stderr); }
}
static size_t utf8_width(const unsigned char *p) {
    size_t n;
    if (*p>=0xc2 && *p<=0xdf) n=2;
    else if (*p>=0xe0 && *p<=0xef) n=3;
    else if (*p>=0xf0 && *p<=0xf4) n=4;
    else return 0;
    for (size_t i=1;i<n;i++) if (p[i]<0x80 || p[i]>0xbf) return 0;
    if ((*p==0xe0 && p[1]<0xa0) || (*p==0xed && p[1]>=0xa0) ||
        (*p==0xf0 && p[1]<0x90) || (*p==0xf4 && p[1]>=0x90)) return 0;
    return n;
}
static void json_string(FILE *f, const char *s) {
    fputc('"',f);
    const unsigned char *p=(const unsigned char *)s;
    while (*p) {
        unsigned char c=*p;
        if (c>=128) {
            size_t width=utf8_width(p);
            if (width) { fwrite(p,1,width,f); p+=width; continue; }
        }
        p++;
        if (c=='"' || c=='\\') { fputc('\\',f); fputc(c,f); }
        else if (c<32 || c>=128) fprintf(f,"\\u%04x",(unsigned)c);
        else fputc(c,f);
    }
    fputc('"',f);
}
static void hex_out(FILE *f, const uint8_t *b, size_t n, int pretty) {
    for (size_t i=0;i<n;i++) {
        fprintf(f,"%02x",b[i]);
        if (pretty) fputc((i+1)%16?' ':'\n',f);
    }
    if (pretty && n%16) fputc('\n',f);
}
static uint16_t le16(const uint8_t *p) { return p[0] | (uint16_t)p[1]<<8; }
static uint32_t le32(const uint8_t *p) {
    return (uint32_t)p[0] | (uint32_t)p[1]<<8 | (uint32_t)p[2]<<16 | (uint32_t)p[3]<<24;
}
static Packet packet(uint16_t op) {
    Packet p={{0x4b,0x13,op&255,op>>8},4,op}; return p;
}
static int append(Packet *p, const void *b, size_t n) {
    if (n>MAX_PKT-p->len) return fail("INVALID_ARGUMENT","Request exceeds %u bytes",MAX_PKT);
    memcpy(p->bytes+p->len,b,n); p->len+=n; return 1;
}
static int put16(Packet *p, uint16_t v) {
    uint8_t b[2]={v&255,v>>8}; return append(p,b,2);
}
static int put32(Packet *p, uint32_t v) {
    uint8_t b[4]={v&255,(v>>8)&255,(v>>16)&255,v>>24}; return append(p,b,4);
}
static int putpath(Packet *p, const char *path) { return append(p,path,strlen(path)+1); }
static int valid_path(const char *path, size_t overhead) {
    if (path[0]!='/') return fail("INVALID_ARGUMENT","EFS path must be absolute");
    if (strlen(path)+1>MAX_PKT-overhead) return fail("INVALID_ARGUMENT","EFS path is too long for this request");
    for (const unsigned char *c=(const unsigned char *)path;*c;c++)
        if (*c<32 || *c==127) return fail("INVALID_ARGUMENT","Control byte in EFS path");
    if (strstr(path,"//") || (strlen(path)>1 && path[strlen(path)-1]=='/'))
        return fail("INVALID_ARGUMENT","Noncanonical EFS path");
    for (const char *c=path+1;*c;) {
        const char *e=strchr(c,'/'); size_t n=e?(size_t)(e-c):strlen(c);
        if ((n==1 && c[0]=='.') || (n==2 && c[0]=='.' && c[1]=='.'))
            return fail("INVALID_ARGUMENT","Dot component in EFS path");
        if (!e) break;
        c=e+1;
    }
    return 1;
}
static int number(const char *s, uint32_t *v, int base) {
    if (!*s) return fail("INVALID_ARGUMENT","Invalid number: %s",s);
    for (const char *p=s;*p;p++) if (*p<'0' || *p>='0'+base)
        return fail("INVALID_ARGUMENT","Invalid number: %s",s);
    char *end; errno=0; unsigned long long n=strtoull(s,&end,base);
    if (errno || *end || n>UINT32_MAX) return fail("INVALID_ARGUMENT","Invalid number: %s",s);
    *v=(uint32_t)n; return 1;
}
static int nibble(char c) {
    if (c>='0' && c<='9') return c-'0';
    if (c>='a' && c<='f') return c-'a'+10;
    if (c>='A' && c<='F') return c-'A'+10;
    return -1;
}
static int valid_hex(const char *s) {
    size_t n=strlen(s);
    if (n%2) return fail("INVALID_ARGUMENT","Hex length must be even");
    for (size_t i=0;i<n;i++) if (nibble(s[i])<0) return fail("INVALID_ARGUMENT","Invalid hex character at index %zu",i);
    return 1;
}
static uint64_t monotonic_ms(void) {
#ifdef DIAG_EFS_TEST
    return mock_clock_ms();
#else
    struct timespec ts; clock_gettime(CLOCK_MONOTONIC,&ts);
    return (uint64_t)ts.tv_sec*1000+(uint64_t)ts.tv_nsec/1000000;
#endif
}
static void wait_tick(void) {
#ifdef DIAG_EFS_TEST
    mock_wait_tick();
#else
    struct timespec ts={0,10000000}; nanosleep(&ts,NULL);
#endif
}
#ifndef DIAG_EFS_TEST
static void signal_handler(int signo) { (void)signo; }
#endif
static void callback(unsigned char *buf, int len) {
    rx_bad=!buf || len<0 || len>MAX_RX;
    rx_len=rx_bad?0:len;
    if (!rx_bad) memcpy(rx,buf,(size_t)len);
    atomic_store_explicit(&reply_ready,1,memory_order_release);
}
static int load_libdiag(void) {
    trace("dlopen libdiag.so.1");
    void *h=dlopen("libdiag.so.1",RTLD_NOW|RTLD_GLOBAL);
    if (!h) { trace("dlopen libdiag.so"); h=dlopen("libdiag.so",RTLD_NOW|RTLD_GLOBAL); }
    if (!h) return fail("LIBRARY_ERROR","%s",dlerror());
    diag_init_p=(init_fn)dlsym(h,"Diag_LSM_Init");
    diag_deinit_p=(deinit_fn)dlsym(h,"Diag_LSM_DeInit");
    diag_register_p=(register_fn)dlsym(h,"diag_register_dci_client");
    diag_support_p=(support_fn)dlsym(h,"diag_get_dci_support_list");
    diag_send_p=(send_fn)dlsym(h,"diag_send_dci_async_req");
    diag_release_p=(release_fn)dlsym(h,"diag_release_dci_client");
    if (!diag_init_p || !diag_deinit_p || !diag_register_p || !diag_support_p || !diag_send_p || !diag_release_p)
        return fail("LIBRARY_ERROR","Required libdiag symbol is missing");
    return 1;
}
static int diag_result(int r, const char *stage) {
    if (r==DIAG_OK) return 1;
    if (!error_info.code) error_info.diag_status=r;
    return fail("DIAG_ERROR","%s returned %d",stage,r);
}
static int diag_init_once(void) {
    if (registered) return 1;
#ifndef DIAG_EFS_TEST
    struct sigaction sa;
    memset(&sa,0,sizeof(sa)); sa.sa_handler=signal_handler; sigemptyset(&sa.sa_mask);
    if (sigaction(SIGCONT,&sa,NULL)) return local_fail("sigaction");
#endif
    trace("Diag_LSM_Init");
    if (!diag_init_p(NULL)) return fail("DIAG_ERROR","Diag_LSM_Init failed");
    inited=1;
    trace("diag_register_dci_client");
    if (!diag_result(diag_register_p(&client,support,NULL,&sig_number),"diag_register_dci_client")) return 0;
    registered=1;
    trace("diag_get_dci_support_list");
    return diag_result(diag_support_p(support),"diag_get_dci_support_list");
}
static void diag_cleanup(void) {
    if (registered) {
        trace("diag_release_dci_client"); diag_result(diag_release_p(&client),"diag_release_dci_client"); registered=0;
    }
    if (inited) { trace("Diag_LSM_DeInit"); diag_deinit_p(); inited=0; }
}
static int send_raw(const uint8_t *tx, size_t len) {
    // Persistent buffers remain alive until libdiag has stopped its threads.
    static uint8_t tx_copy[MAX_PKT],rx_tmp[MAX_RX];
    if (!len || len>MAX_PKT) return fail("INVALID_ARGUMENT","Invalid packet length");
    memcpy(tx_copy,tx,len); memset(rx_tmp,0,sizeof(rx_tmp)); rx_len=0; rx_bad=0;
    atomic_store_explicit(&reply_ready,0,memory_order_relaxed);
    trace("diag_send_dci_async_req");
    if (!diag_result(diag_send_p(client,tx_copy,(int)len,rx_tmp,MAX_RX,callback,NULL),"diag_send_dci_async_req")) return 0;
    uint64_t start=monotonic_ms();
    while (!atomic_load_explicit(&reply_ready,memory_order_acquire)) {
        if (monotonic_ms()-start>=5000) return fail("TIMEOUT","Timeout waiting for DIAG response");
        wait_tick();
    }
    if (rx_bad) return fail("PROTOCOL_ERROR","Invalid or oversized DIAG response");
    return 1;
}
static int request(Packet *p, size_t min_len) {
    if (!send_raw(p->bytes,p->len)) return 0;
    if ((size_t)rx_len<min_len || rx[0]!=0x4b || rx[1]!=0x13 || le16(rx+2)!=p->op)
        return fail("PROTOCOL_ERROR","Invalid response to EFS command %u (length %d)",p->op,rx_len);
    return 1;
}
static int efs_result(uint32_t err, const char *stage) {
    if (!err) return 1;
    if (!error_info.code) error_info.efs_errno=err;
    return fail("EFS_ERROR","%s: EFS errno %u",stage,err);
}
static int efs_open(const char *path, uint32_t flags, uint32_t mode, int *fd) {
    Packet p=packet(E_OPEN);
    if (!put32(&p,flags) || !put32(&p,mode) || !putpath(&p,path) || !request(&p,12) || !efs_result(le32(rx+8),"OPEN")) return 0;
    *fd=(int32_t)le32(rx+4);
    if (*fd<0) return fail("PROTOCOL_ERROR","OPEN returned an invalid descriptor");
    return 1;
}
static int efs_close(int fd) {
    Packet p=packet(E_CLOSE); put32(&p,(uint32_t)fd);
    return request(&p,8) && efs_result(le32(rx+4),"CLOSE");
}
static int efs_read(int fd, uint32_t offset, uint32_t want, uint32_t *count) {
    Packet p=packet(E_READ); put32(&p,(uint32_t)fd); put32(&p,want); put32(&p,offset);
    if (!request(&p,20) || !efs_result(le32(rx+16),"READ")) return 0;
    uint32_t n=le32(rx+12);
    if (le32(rx+4)!=(uint32_t)fd || le32(rx+8)!=offset || n>want || n>(uint32_t)rx_len-20)
        return fail("PROTOCOL_ERROR","READ returned inconsistent descriptor, offset or length");
    *count=n; return 1;
}
static int efs_write(int fd, uint32_t offset, const uint8_t *buf, uint32_t len, uint32_t *count) {
    Packet p=packet(E_WRITE); put32(&p,(uint32_t)fd); put32(&p,offset);
    if (!append(&p,buf,len) || !request(&p,20) || !efs_result(le32(rx+16),"WRITE")) return 0;
    uint32_t n=le32(rx+12);
    if (le32(rx+4)!=(uint32_t)fd || le32(rx+8)!=offset || !n || n>len)
        return fail("PROTOCOL_ERROR","WRITE returned inconsistent descriptor, offset or length");
    *count=n; return 1;
}
static const char *file_type(uint32_t mode) {
    switch (mode&0170000) {
    case 0040000: return "directory"; case 0100000: return "file";
    case 0120000: return "symlink"; case 0160000: return "item"; default: return "other";
    }
}
static void metadata(uint32_t mode, uint32_t size, uint32_t atime, uint32_t mtime, uint32_t ctime) {
    fprintf(data_out,"\"type\":\"%s\",\"mode\":%u,\"size\":%u,\"atime\":%u,\"mtime\":%u,\"ctime\":%u",
            file_type(mode),mode,size,atime,mtime,ctime);
}
static void path_field(const char *path) { fputs("\"path\":",data_out); json_string(data_out,path); }
static int cmd_stat(const char *path) {
    Packet p=packet(E_STAT);
    if (!putpath(&p,path) || !request(&p,8) || !efs_result(le32(rx+4),"STAT")) return 0;
    if (rx_len<32) return fail("PROTOCOL_ERROR","STAT response is truncated");
    if (json_mode) {
        fputc('{',data_out); path_field(path); fputc(',',data_out);
        metadata(le32(rx+8),le32(rx+12),le32(rx+20),le32(rx+24),le32(rx+28));
        fprintf(data_out,",\"links\":%u}",le32(rx+16));
    } else fprintf(data_out,"%s size=%u mode=%o mtime=%u %s\n",file_type(le32(rx+8)),le32(rx+12),le32(rx+8),le32(rx+24),path);
    return 1;
}
static int cmd_ls(const char *path) {
    Packet p=packet(E_OPENDIR);
    if (!putpath(&p,path) || !request(&p,12) || !efs_result(le32(rx+8),"OPENDIR")) return 0;
    uint32_t dir=le32(rx+4),seq; int ok=1,first=1,ended=0;
    if (json_mode) { fputc('{',data_out); path_field(path); fputs(",\"entries\":[",data_out); }
    for (seq=1;seq<=MAX_ENTRIES;seq++) {
        p=packet(E_READDIR); put32(&p,dir); put32(&p,seq);
        if (!request(&p,16)) { ok=0; break; }
        uint32_t err=le32(rx+12);
        if (err==0x40000004) { ended=1; break; }
        if (!efs_result(err,"READDIR")) { ok=0; break; }
        if (rx_len<41 || le32(rx+4)!=dir || le32(rx+8)!=seq) {
            ok=fail("PROTOCOL_ERROR","READDIR response is truncated or has the wrong sequence"); break;
        }
        char *name=(char *)rx+40;
        if (!memchr(name,0,(size_t)rx_len-40)) { ok=fail("PROTOCOL_ERROR","READDIR name is unterminated"); break; }
        if (!*name) { ended=1; break; }
        if (!strcmp(name,".") || !strcmp(name,"..")) continue;
        uint32_t mode=le32(rx+20),size=le32(rx+24);
        if (json_mode) {
            if (!first) fputc(',',data_out);
            first=0; fputs("{\"name\":",data_out); json_string(data_out,name);
            fputs(",\"name_hex\":\"",data_out); hex_out(data_out,(uint8_t *)name,strlen(name),0);
            fputs("\",",data_out); metadata(mode,size,le32(rx+28),le32(rx+32),le32(rx+36)); fputc('}',data_out);
        } else fprintf(data_out,"%c %8u  %s\n",(mode&0170000)==0040000?'d':'-',size,name);
    }
    if (ok && !ended) ok=fail("LIMIT_EXCEEDED","Directory exceeds %u entries",MAX_ENTRIES);
    if (json_mode) fputs("]}",data_out);
    if (!error_info.code || strcmp(error_info.code,"TIMEOUT")) {
        p=packet(E_CLOSEDIR); put32(&p,dir);
        if (!request(&p,8) || !efs_result(le32(rx+4),"CLOSEDIR")) ok=0;
    }
    return ok;
}
static int cmd_path(uint16_t op, const char *path, const char *dst, uint32_t mode) {
    Packet p=packet(op);
    if (op==E_MKDIR && !put16(&p,(uint16_t)(0040000|mode))) return 0;
    if (!putpath(&p,path) || (dst && !putpath(&p,dst)) || !request(&p,8) || !efs_result(le32(rx+4),command)) return 0;
    if (json_mode) {
        fputc('{',data_out); path_field(path);
        if (dst) { fputs(",\"destination\":",data_out); json_string(data_out,dst); } fputc('}',data_out);
    } else fprintf(data_out,"%s succeeded: %s%s%s\n",command,path,dst?" -> ":"",dst?dst:"");
    return 1;
}

// READDIR reports the entry itself, unlike STAT which may follow a symlink.
// Walk one component at a time from /; never traverse a link or unknown type.
static int guarded_entry(const char *parent, const char *name, uint32_t *mode, int *found) {
    Packet p=packet(E_OPENDIR);
    if (!putpath(&p,parent) || !request(&p,12) || !efs_result(le32(rx+8),"GUARD OPENDIR")) return 0;
    uint32_t dir=le32(rx+4); int ok=1,ended=0; *found=0;
    for (uint32_t seq=1;seq<=MAX_ENTRIES;seq++) {
        p=packet(E_READDIR); put32(&p,dir); put32(&p,seq);
        if (!request(&p,16)) { ok=0; break; }
        if (le32(rx+12)==0x40000004) { ended=1; break; }
        if (!efs_result(le32(rx+12),"GUARD READDIR")) { ok=0; break; }
        if (rx_len<41 || le32(rx+4)!=dir || le32(rx+8)!=seq || !memchr(rx+40,0,(size_t)rx_len-40)) {
            ok=fail("PROTOCOL_ERROR","Invalid guard directory entry"); break;
        }
        if (!rx[40]) { ended=1; break; }
        if (!strcmp((char *)rx+40,name)) { *mode=le32(rx+20); *found=1; ended=1; break; }
    }
    if (ok && !ended) ok=fail("LIMIT_EXCEEDED","Cannot safely inspect directory");
    if (!error_info.code || strcmp(error_info.code,"TIMEOUT")) {
        p=packet(E_CLOSEDIR); put32(&p,dir);
        if (!request(&p,8) || !efs_result(le32(rx+4),"GUARD CLOSEDIR")) ok=0;
    }
    return ok;
}
static int guarded_path(const char *path, int allow_missing) {
    char parent[MAX_PKT]="/",name[MAX_PKT]; const char *c=path+1;
    for (;;) {
        const char *end=strchr(c,'/'); size_t n=end?(size_t)(end-c):strlen(c);
        memcpy(name,c,n); name[n]=0;
        uint32_t mode=0; int found=0;
        if (!guarded_entry(parent,name,&mode,&found)) return 0;
        if (!found) {
            if (!end && allow_missing) return 1;
            return fail("UNSAFE_PATH","Cannot verify EFS path component");
        }
        uint32_t type=mode&0170000;
        if (end ? type!=0040000 : (type!=0040000 && type!=0100000))
            return fail("UNSAFE_PATH","Links and special files cannot be modified");
        if (!end) {
            if (allow_missing==2) return fail("UNSAFE_PATH","Destination already exists; rename cannot replace it");
            if (type==0100000) {
                Packet p=packet(E_STAT);
                if (!putpath(&p,path) || !request(&p,32) || !efs_result(le32(rx+4),"GUARD STAT")) return 0;
                if ((le32(rx+8)&0170000)!=0100000 || le32(rx+16)!=1)
                    return fail("UNSAFE_PATH","Linked or changed file cannot be modified");
            }
            return 1;
        }
        size_t plen=(size_t)(end-path);
        memcpy(parent,path,plen); parent[plen]=0; c=end+1;
    }
}
static int close_after(int fd, int ok) {
    // Do not reuse receive buffers after an ambiguous timed-out request.
    if (error_info.code && !strcmp(error_info.code,"TIMEOUT")) return 0;
    if (!efs_close(fd)) return 0;
    return ok;
}
static int cmd_read(const char *path, uint32_t length, uint32_t offset) {
    int fd;
    if (!efs_open(path,0,0,&fd)) return 0;
    uint32_t total=0; int ok=1,eof=0;
    if (json_mode) {
        fputc('{',data_out); path_field(path);
        fprintf(data_out,",\"offset\":%u,\"encoding\":\"hex\",\"hex\":\"",offset);
    }
    while (total<length) {
        uint32_t want=length-total,n;
        if (want>READ_CHUNK) want=READ_CHUNK;
        if (!efs_read(fd,offset+total,want,&n)) { ok=0; break; }
        if (!n) { eof=1; break; }
        hex_out(data_out,rx+20,n,!json_mode); total+=n; error_info.transferred=total;
    }
    if (json_mode) fprintf(data_out,"\",\"bytes\":%u,\"next_offset\":%u,\"eof\":%s}",total,offset+total,eof?"true":"false");
    return close_after(fd,ok);
}
static int write_chunk(int fd, uint32_t *offset, const uint8_t *b, uint32_t len) {
    uint32_t done=0;
    while (done<len) {
        uint32_t n;
        if (!efs_write(fd,*offset,b+done,len-done,&n)) return 0;
        *offset+=n; done+=n; error_info.transferred+=n;
    }
    return 1;
}
static void write_result(const char *path, uint32_t start, const char *local) {
    if (json_mode) {
        fputc('{',data_out); path_field(path);
        fprintf(data_out,",\"offset\":%u,\"bytes\":%llu",start,(unsigned long long)error_info.transferred);
        if (local) { fputs(",\"local_path\":",data_out); json_string(data_out,local); } fputc('}',data_out);
    } else fprintf(data_out,"%s %llu bytes\n",command,(unsigned long long)error_info.transferred);
}
static int cmd_write(const char *path, const char *hex, uint32_t offset, int truncate) {
    int fd; uint32_t start=offset;
    if (!efs_open(path,truncate?0x241:0x41,0666,&fd)) return 0;
    size_t size=strlen(hex)/2,done=0; int ok=1;
    while (done<size) {
        uint8_t b[WRITE_CHUNK]; size_t n=size-done;
        if (n>WRITE_CHUNK) n=WRITE_CHUNK;
        for (size_t i=0;i<n;i++) b[i]=(uint8_t)((nibble(hex[2*(done+i)])<<4)|nibble(hex[2*(done+i)+1]));
        if (!write_chunk(fd,&offset,b,(uint32_t)n)) { ok=0; break; } done+=n;
    }
    ok=close_after(fd,ok);
    if (ok) write_result(path,start,NULL);
    return ok;
}
static int cmd_put(const char *local, const char *path, uint32_t offset, int truncate) {
    uint32_t start=offset;
    FILE *f=fopen(local,"rb");
    if (!f) return local_fail("Open upload file");
    // Validate the local file before a remote O_TRUNC can destroy old contents.
    if (fseek(f,0,SEEK_END)) { local_fail("Seek upload file"); fclose(f); return 0; }
    long size=ftell(f);
    if (size<0 || (uint64_t)size>MAX_TRANSFER || (uint64_t)offset+(uint64_t)size>UINT32_MAX) { fclose(f); return fail("INVALID_ARGUMENT","Upload range exceeds limits"); }
    if (fseek(f,0,SEEK_SET)) { local_fail("Rewind upload file"); fclose(f); return 0; }
    int fd;
    if (!efs_open(path,truncate?0x241:0x41,0666,&fd)) { fclose(f); return 0; }
    int ok=1;
    for (;;) {
        uint8_t b[WRITE_CHUNK]; size_t n=fread(b,1,sizeof(b),f);
        if (ferror(f)) { ok=local_fail("Read upload file"); break; }
        if (!n) break;
        if (error_info.transferred+n>MAX_TRANSFER || (uint64_t)offset+n>UINT32_MAX) { ok=fail("LIMIT_EXCEEDED","Upload range exceeds limits"); break; }
        if (!write_chunk(fd,&offset,b,(uint32_t)n)) { ok=0; break; }
    }
    fclose(f); ok=close_after(fd,ok);
    if (ok) write_result(path,start,local);
    return ok;
}
static int cmd_get(const char *path, const char *local) {
    size_t cap=strlen(local)+64; char *temporary=malloc(cap);
    if (!temporary) return fail("INTERNAL_ERROR","Out of memory");
    snprintf(temporary,cap,"%s.part.%ld",local,(long)getpid());
    int local_fd=open(temporary,O_CREAT|O_EXCL|O_WRONLY,0600);
    if (local_fd<0) { free(temporary); return local_fail("Create download temporary file"); }
    FILE *f=fdopen(local_fd,"wb");
    if (!f) { local_fail("Open download stream"); close(local_fd); remove(temporary); free(temporary); return 0; }
    int fd,ok=efs_open(path,0,0,&fd); uint32_t offset=0;
    if (ok) {
        for (;;) {
            uint32_t n,want=READ_CHUNK;
            if (UINT32_MAX-offset<want) want=UINT32_MAX-offset;
            if (!want) { ok=fail("LIMIT_EXCEEDED","Download offset overflow"); break; }
            if (!efs_read(fd,offset,want,&n)) { ok=0; break; }
            if (!n) break;
            if ((uint64_t)offset+n>MAX_TRANSFER) { ok=fail("LIMIT_EXCEEDED","Download file exceeds 1 MiB"); break; }
            if (fwrite(rx+20,1,n,f)!=n) { ok=local_fail("Write download file"); break; }
            offset+=n; error_info.transferred=offset;
        }
        ok=close_after(fd,ok);
    }
    if (fclose(f)) { local_fail("Close download file"); ok=0; }
    if (ok && rename(temporary,local)) ok=local_fail("Commit download file");
    if (!ok) remove(temporary);
    free(temporary);
    if (ok) write_result(path,0,local);
    return ok;
}
static int cmd_raw(const char *hex, int nv, uint32_t item) {
    uint8_t b[MAX_PKT]; size_t n;
    if (nv) { n=133; memset(b,0,n); b[0]=0x26; b[1]=item&255; b[2]=item>>8; }
    else { n=strlen(hex)/2; for (size_t i=0;i<n;i++) b[i]=(uint8_t)((nibble(hex[2*i])<<4)|nibble(hex[2*i+1])); }
    if (!send_raw(b,n)) return 0;
    if (json_mode) { fprintf(data_out,"{\"encoding\":\"hex\",\"bytes\":%d,\"hex\":\"",rx_len); hex_out(data_out,rx,(size_t)rx_len,0); fputs("\"}",data_out); }
    else hex_out(data_out,rx,(size_t)rx_len,1);
    return 1;
}
static void usage(FILE *f) {
    fputs("usage: diag_efs [--json] <command> <args>\n"
          "  ls [efs_dir]                         (default /)\n"
          "  stat <efs_path>\n"
          "  read <efs_file> [length [offset]]     (JSON default 4096, text 176)\n"
          "  write <efs_file> <hex> [offset]       (no offset: replace whole file)\n"
          "  get <efs_file> <local_file>\n"
          "  put <local_file> <efs_file>           (replace whole file)\n"
          "  write-file <efs_file> <local_file> [offset]\n"
          "  rm <efs_file>                        (aliases: unlink, delete)\n"
          "  mkdir <efs_dir> [octal_mode]          (default 0755)\n"
          "  rmdir <empty_efs_dir>\n"
          "  rename <old_efs_path> <new_efs_path>   (alias: mv)\n"
          "  nvread <item> | raw <hex> | help\n",f);
}
static int finish(int ok) {
    diag_cleanup();
    if (error_info.code) ok=0;
    if (data_out && (ferror(data_out) || fflush(data_out))) { local_fail("Flush result"); ok=0; }
    FILE *f=result_out?result_out:stdout;
    if (json_mode) {
        fprintf(f,"{\"ok\":%s,\"command\":",ok?"true":"false"); json_string(f,command);
        if (ok) {
            fputs(",\"data\":",f); rewind(data_out); char b[4096]; size_t n;
            while ((n=fread(b,1,sizeof(b),data_out))) fwrite(b,1,n,f);
        } else {
            fputs(",\"error\":{\"code\":",f); json_string(f,error_info.code?error_info.code:"INTERNAL_ERROR");
            fputs(",\"message\":",f); json_string(f,error_info.message);
            fprintf(f,",\"efs_errno\":%u,\"diag_status\":%d,\"local_errno\":%d,\"bytes_transferred\":%llu}",
                    error_info.efs_errno,error_info.diag_status,error_info.local_errno,(unsigned long long)error_info.transferred);
        }
        fputs("}\n",f);
    } else if (ok && data_out) {
        rewind(data_out); char b[4096]; size_t n;
        while ((n=fread(b,1,sizeof(b),data_out))) fwrite(b,1,n,f);
    } else fprintf(stderr,"%s: %s\n",error_info.code?error_info.code:"ERROR",error_info.message);
    int output_error=ferror(f) || fflush(f);
    if (data_out) fclose(data_out);
    if (result_out) fclose(result_out);
    if (output_error) return 1;
    return ok?0:(error_info.code && !strcmp(error_info.code,"INVALID_ARGUMENT")?2:1);
}
int main(int argc, char **argv) {
    debug_enabled=getenv("DIAG_DEBUG")!=NULL;
    int pos=1;
    if (argc>1 && !strcmp(argv[1],"--json")) { json_mode=1; pos++; }
    int guarded=argc>pos && !strcmp(argv[pos],"--guarded");
    if (guarded) pos++;
    if (json_mode) {
        fflush(stdout); int saved=dup(STDOUT_FILENO);
        if (saved<0) { local_fail("Duplicate stdout"); return finish(0); }
        result_out=fdopen(saved,"w");
        if (!result_out) { close(saved); local_fail("Open JSON stream"); return finish(0); }
        // libdiag writes directly to fd 1 as well as via stdio.
        if (dup2(STDERR_FILENO,STDOUT_FILENO)<0) { local_fail("Redirect libdiag logs"); return finish(0); }
    }
    trace("main");
    if (argc<=pos) { fail("INVALID_ARGUMENT","Missing command"); if (!json_mode) usage(stderr); return finish(0); }
    command=argv[pos++]; int n=argc-pos; char **a=argv+pos;
    if (!strcmp(command,"--help") || !strcmp(command,"help")) {
        if (!json_mode) { usage(stdout); return 0; }
        data_out=tmpfile();
        if (!data_out) { local_fail("Create result buffer"); return finish(0); }
        fputs("{\"version\":1,\"commands\":[\"ls\",\"stat\",\"read\",\"write\",\"get\",\"put\",\"rm\",\"mkdir\",\"rmdir\",\"rename\",\"nvread\",\"raw\"]}",data_out);
        return finish(1);
    }
    uint32_t length=json_mode?4096:176,offset=0,mode=0755,item=0;
    const char *path=NULL,*dst=NULL; int op=0,valid=1,truncate=1;
    if (!strcmp(command,"ls")) { valid=n<=1; path=n?a[0]:"/"; }
    else if (!strcmp(command,"stat")) { valid=n==1; if (valid) path=a[0]; }
    else if (!strcmp(command,"read")) {
        valid=n>=1 && n<=3;
        if (valid) { path=a[0]; if (n>=2) valid=number(a[1],&length,10); if (valid && n==3) valid=number(a[2],&offset,10); }
        if (valid && (length>MAX_PREVIEW || (uint64_t)offset+length>UINT32_MAX)) valid=fail("INVALID_ARGUMENT","Read range exceeds supported limits");
    } else if (!strcmp(command,"write")) {
        valid=n==2 || n==3;
        if (valid) { path=a[0]; valid=valid_hex(a[1]); if (valid && n==3) { truncate=0; valid=number(a[2],&offset,10); } }
        if (valid && (strlen(a[1])/2>MAX_PREVIEW || (uint64_t)offset+strlen(a[1])/2>UINT32_MAX)) valid=fail("INVALID_ARGUMENT","Write range exceeds supported limits");
    } else if (!strcmp(command,"write-file")) {
        valid=n==2 || n==3;
        if (valid) { path=a[0]; if (n==3) { truncate=0; valid=number(a[2],&offset,10); } }
    } else if (!strcmp(command,"get") || !strcmp(command,"put")) {
        valid=n==2; if (valid) path=!strcmp(command,"get")?a[0]:a[1];
    } else if (!strcmp(command,"rm") || !strcmp(command,"unlink") || !strcmp(command,"delete")) {
        valid=n==1; if (valid) path=a[0]; op=E_UNLINK;
    } else if (!strcmp(command,"rmdir")) {
        valid=n==1; if (valid) path=a[0]; op=E_RMDIR;
    } else if (!strcmp(command,"mkdir")) {
        valid=n==1 || n==2; op=E_MKDIR;
        if (valid) { path=a[0]; if (n==2) valid=number(a[1],&mode,8); }
        if (valid && mode>0777) valid=fail("INVALID_ARGUMENT","Directory mode must be 0000..0777");
    } else if (!strcmp(command,"rename") || !strcmp(command,"mv")) {
        valid=n==2; op=E_RENAME; if (valid) { path=a[0]; dst=a[1]; }
    } else if (!strcmp(command,"nvread")) {
        valid=n==1; if (valid) valid=number(a[0],&item,10);
        if (valid && item>65535) valid=fail("INVALID_ARGUMENT","NV item must be 0..65535");
    } else if (!strcmp(command,"raw")) {
        valid=n==1; if (valid) valid=valid_hex(a[0]);
        if (valid && (!*a[0] || strlen(a[0])/2>MAX_PKT)) valid=fail("INVALID_ARGUMENT","Raw packet length must be 1..1024 bytes");
    } else valid=0;
    if (valid && path) valid=valid_path(path,12);
    if (valid && dst) valid=valid_path(dst,4) && strlen(path)+strlen(dst)+6<=MAX_PKT;
    if (!valid) { fail("INVALID_ARGUMENT","Invalid arguments for %s; use help",command); return finish(0); }
    int mutation=op || !strcmp(command,"write") || !strcmp(command,"write-file") || !strcmp(command,"put");
    if (guarded && strcmp(command,"nvread") && (!path || (mutation && (!strchr(path+1,'/') || (dst && !strchr(dst+1,'/')))))) {
        fail("ROOT_READONLY","Root and direct children are read-only"); return finish(0);
    }
    data_out=tmpfile();
    if (!data_out) { local_fail("Create result buffer"); return finish(0); }
    if (!load_libdiag() || !diag_init_once()) return finish(0);
    if (guarded && mutation) {
        int create=op==E_MKDIR || !op;
        if (!guarded_path(path,create) || (dst && !guarded_path(dst,2))) return finish(0);
    }
    int ok;
    if (op) ok=cmd_path((uint16_t)op,path,dst,mode);
    else if (!strcmp(command,"ls")) ok=cmd_ls(path);
    else if (!strcmp(command,"stat")) ok=cmd_stat(path);
    else if (!strcmp(command,"read")) ok=cmd_read(path,length,offset);
    else if (!strcmp(command,"write")) ok=cmd_write(path,a[1],offset,truncate);
    else if (!strcmp(command,"get")) ok=cmd_get(path,a[1]);
    else if (!strcmp(command,"put")) ok=cmd_put(a[0],path,0,1);
    else if (!strcmp(command,"write-file")) ok=cmd_put(a[1],path,offset,truncate);
    else ok=cmd_raw(!strcmp(command,"raw")?a[0]:NULL,!strcmp(command,"nvread"),item);
    return finish(ok);
}
