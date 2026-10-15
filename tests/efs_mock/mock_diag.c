#define _POSIX_C_SOURCE 200809L
#ifdef _WIN32
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#endif
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <stdarg.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <time.h>
#include "mock_diag.h"

static uint64_t clock_ms;
static unsigned writes;
static int file_open,dir_open;
static const char *scenario(void) {
    static char configured[64];
    const char *path=getenv("ZWRT_DATAD_EFS_TEST_CONFIG");
    if (path) {
        FILE *f=fopen(path,"r");
        if (f) {
            if (!fgets(configured,sizeof(configured),f)) configured[0]=0;
            fclose(f); configured[strcspn(configured,"\r\n")]=0;
            return configured;
        }
    }
    const char *s=getenv("MOCK_SCENARIO"); return s?s:"";
}
static int is(const char *s) { return !strcmp(scenario(),s); }
static void event(const char *fmt,...) {
    const char *path=getenv("MOCK_TRACE");
    if (!path) return;
    FILE *f=fopen(path,"a"); if (!f) abort();
    va_list ap; va_start(ap,fmt); vfprintf(f,fmt,ap); va_end(ap);
    fputc('\n',f); fclose(f);
}
static uint32_t u32(const unsigned char *p) {
    return (uint32_t)p[0] | (uint32_t)p[1]<<8 | (uint32_t)p[2]<<16 | (uint32_t)p[3]<<24;
}
static void put(unsigned char *p,uint32_t v) {
    p[0]=v&255; p[1]=(v>>8)&255; p[2]=(v>>16)&255; p[3]=v>>24;
}
static void require(int yes) { if (!yes) { event("BAD_PACKET"); abort(); } }
static int init(void *p) {
    (void)p; event("INIT"); puts("mock libdiag init log on stdout");
    return !is("init_error");
}
static void deinit(void) { event("DEINIT"); puts("mock libdiag deinit log on stdout"); fflush(stdout); }
static int reg(int *c,void *s,void *p,int *sig) {
    (void)s; (void)p; require(*sig==18); event("REGISTER"); *c=123;
    return is("register_error")?77:1001;
}
static int supported(void *p) { (void)p; event("SUPPORT"); return is("support_error")?88:1001; }
static int release(int *c) { require(*c==123); event("RELEASE"); return is("release_error")?99:1001; }
static int send(int c,void *buffer,int len,void *response,int max,
                void (*cb)(unsigned char *,int),void *userdata) {
    (void)response; (void)userdata; require(c==123 && len>0 && len<=1024 && max>=2048);
    const unsigned char *tx=buffer;
    static char parent[1024];
    char hex[2050];
    for (int i=0;i<len;i++) sprintf(hex+2*i,"%02x",tx[i]);
    event("PACKET %s",hex);
    if (is("slow")) {
#ifdef _WIN32
        Sleep(350);
#else
        struct timespec delay={0,350000000}; nanosleep(&delay,NULL);
#endif
    }
    unsigned char rx[2048]={0}; int n=8;
    if (is("timeout")) return 1001;
    if (is("send_error")) return 55;
    if (is("negative_callback")) { cb(rx,-1); return 1001; }
    if (is("oversized_callback")) { cb(rx,2049); return 1001; }
    if (tx[0]==0x26 && len==133 && tx[1]==0x26 && tx[2]==0x02) {
        const unsigned char fixture[9]={0x08,0x4a,0x09,0x51,0x24,0x30,0x32,0x57,0x81};
        memcpy(rx,tx,(size_t)len); memcpy(rx+3,fixture,9);
        if (is("nv_status")) rx[131]=5;
        if (is("nv_status_high")) rx[132]=1;
        if (is("nv_item")) rx[1]++;
        if (is("nv_command")) rx[0]=0x14;
        if (is("nv_length")) rx[3]=7;
        if (is("nv_marker")) rx[4]=0x4f;
        if (is("nv_digit")) rx[6]=0xfa;
        cb(rx,is("nv_short")?132:len); return 1001;
    }
    if (tx[0]!=0x4b) { memcpy(rx,tx,(size_t)len); cb(rx,len); return 1001; }
    require(len>=4 && tx[1]==0x13);
    memcpy(rx,tx,4); unsigned op=tx[2]|tx[3]<<8;
    event("OP %u",op);
    switch (op) {
    case 2:
        require(len>=13 && memchr(tx+12,0,(size_t)len-12));
        event("OPEN_FLAGS %u",u32(tx+4)); file_open=1; put(rx+4,7); n=12;
        if (is("open_error")) { put(rx+4,UINT32_MAX); put(rx+8,2); file_open=0; }
        break;
    case 3:
        require(len==8 && u32(tx+4)==7 && file_open);
        file_open=0; if (is("close_error")) put(rx+4,5); break;
    case 4: {
        require(len==16 && file_open && u32(tx+4)==7);
        uint32_t want=u32(tx+8),offset=u32(tx+12);
        require(want<=160); put(rx+4,7); put(rx+8,offset); n=20;
        if (is("read_error")) { put(rx+16,13); break; }
        uint32_t count=offset>=512?0:512-offset;
        if (count>want) count=want;
        if (is("short_reads") && count>7) count=7;
        put(rx+12,count);
        for (uint32_t i=0;i<count;i++) rx[20+i]=(unsigned char)(offset+i);
        n+=(int)count;
        if (is("bad_read_length")) put(rx+12,want+1);
        if (is("wrong_offset")) put(rx+8,offset+1);
        if (is("wrong_descriptor")) put(rx+4,8);
        break;
    }
    case 5: {
        require(len>12 && len<=162 && file_open && u32(tx+4)==7);
        uint32_t offset=u32(tx+8),count=(uint32_t)len-12;
        put(rx+4,7); put(rx+8,offset); n=20; writes++;
        if (is("second_write_error") && writes==2) { put(rx+16,28); break; }
        if (is("partial_writes") && count>3) count/=2;
        if (is("zero_write")) count=0;
        if (is("bad_write_count")) count=(uint32_t)len;
        put(rx+12,count); event("WRITE %u %u",offset,count); break;
    }
    case 8: case 10:
        require(len>=6 && tx[4]=='/' && tx[len-1]==0);
        if (is("delete_error")) put(rx+4,13);
        break;
    case 9:
        require(len>=8 && tx[6]=='/' && tx[len-1]==0);
        event("MKDIR_MODE %u",tx[4]|tx[5]<<8); break;
    case 11:
        require(len>=6 && tx[4]=='/' && tx[len-1]==0);
        snprintf(parent,sizeof(parent),"%s",(const char *)tx+4);
        dir_open=1; put(rx+4,42); n=12;
        if (is("opendir_error")) { dir_open=0; put(rx+8,13); }
        if (is("short_opendir")) n=8;
        break;
    case 12: {
        require(len==12 && dir_open && u32(tx+4)==42);
        uint32_t seq=u32(tx+8); put(rx+4,42); put(rx+8,seq); n=41;
        if (is("readdir_error")) { put(rx+12,5); n=16; break; }
        if (is("end_error") && seq>4) { put(rx+12,0x40000004); n=16; break; }
        const char *name=seq==1?".":seq==2?"a\"b\\\n":seq==3?"directory":seq==4?"\xe4\xb8\xad\xe6\x96\x87":"";
        const char *guard_names[]={"file","empty","old","link","item","hard"};
        if (getenv("MOCK_GUARD_TREE")) name=!strcmp(parent,"/")?(seq==1?"nvefs":""):(seq<=6?guard_names[seq-1]:"");
        char longname[181]; memset(longname,'x',180); longname[180]=0;
        if (is("long_name") && seq==2) name=longname;
        put(rx+20,seq==3?0040755:seq==4?0160666:0100644);
        if (getenv("MOCK_GUARD_TREE")) put(rx+20,!strcmp(parent,"/")?0040755:seq==2?0040755:seq==4?0120777:seq==5?0160666:0100644);
        put(rx+24,512); put(rx+28,10); put(rx+32,20); put(rx+36,30);
        memcpy(rx+40,name,strlen(name)+1); n=40+(int)strlen(name)+1;
        if (is("unterminated_name")) { rx[n-1]='x'; }
        if (is("wrong_sequence")) put(rx+8,seq+1);
        break;
    }
    case 13:
        require(len==8 && dir_open && u32(tx+4)==42); dir_open=0; break;
    case 14: {
        require(len>=8 && tx[4]=='/' && tx[len-1]==0);
        const unsigned char *end=memchr(tx+4,0,(size_t)len-4);
        require(end && end+1<tx+len && end[1]=='/'); break;
    }
    case 15:
        require(len>=6 && tx[4]=='/' && tx[len-1]==0);
        if (is("stat_error")) { put(rx+4,2); break; }
        n=32; put(rx+8,0100644); put(rx+12,512); put(rx+16,1);
        if (getenv("MOCK_GUARD_TREE") && strstr((const char *)tx+4,"/hard")) put(rx+16,2);
        put(rx+20,10); put(rx+24,20); put(rx+28,30); break;
    default: require(0);
    }
    if (is("wrong_header")) rx[0]=0;
    if (is("truncated_response")) n=3;
    cb(rx,n); return 1001;
}
void *mock_dlopen(const char *s,int flags) {
    (void)s; (void)flags; event("DLOPEN"); return is("library_error")?NULL:(void *)(uintptr_t)1;
}
const char *mock_dlerror(void) { return "mock library unavailable"; }
void *mock_dlsym(void *h,const char *s) {
    (void)h;
    if (is("symbol_error")) return NULL;
    if (!strcmp(s,"Diag_LSM_Init")) return (void *)init;
    if (!strcmp(s,"Diag_LSM_DeInit")) return (void *)deinit;
    if (!strcmp(s,"diag_register_dci_client")) return (void *)reg;
    if (!strcmp(s,"diag_get_dci_support_list")) return (void *)supported;
    if (!strcmp(s,"diag_send_dci_async_req")) return (void *)send;
    if (!strcmp(s,"diag_release_dci_client")) return (void *)release;
    return NULL;
}
uint64_t mock_clock_ms(void) { return clock_ms; }
void mock_wait_tick(void) { clock_ms+=1000; }
#ifdef _WIN32
int mock_binary_open(const char *path,int flags,int mode) { return _open(path,flags|_O_BINARY,mode); }
// MSVCRT tmpfile() chooses the drive root. Keep test artifacts in the fixture.
FILE *mock_tmpfile(void) { return fopen(getenv("MOCK_TMP_FILE"),"w+b"); }
#endif
