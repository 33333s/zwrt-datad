/* Process-isolated QTrace collector. See docs/NEIGHBOR.md. */
#include "neighbor.h"
#include "json.h"
#include "neighbor/qtrace_mask.h"
#include <ctype.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <limits.h>
#include <signal.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/resource.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#ifdef __linux__
#include <sys/prctl.h>
#endif
#ifndef MSG_NOSIGNAL
#define MSG_NOSIGNAL 0
#endif

#define RUNTIME_DEFAULT "/tmp/zwrt-datad-neighbor"
#define CONFIG_DEFAULT "/data/zwrt-datad/neighbor.json"
#define DIAG_DEFAULT "/usr/bin/diag_mdlog"
#define MAX_CAPTURE_BYTES (32ULL * 1024ULL * 1024ULL)
#define MAX_FILES 32
#define MAX_SCAN_ENTRIES 256
#define READ_BUDGET (4 * 1024 * 1024)
#define STALE_TRANSPORT_MS 90000
#define RETRY_MS 5000
#define WORKER_TIMEOUT_MS 15000
#define WIRE_MAGIC 0x4e425231U

enum status_code { DISABLED, STARTING, COLLECTING, READY, EMPTY, STALE,
                   BLOCKED, DEPENDENCY_MISSING, ERROR, STOPPING };
enum reason_code { NONE, LOCKED, DIAG_BUSY, TOOL_MISSING, RUNTIME_ERROR, MASK_ERROR,
                   COLLECTOR_EXITED, CAPTURE_LIMIT, READ_ERROR, NO_PROGRESS,
                   WORKER_EXITED, WORKER_TIMEOUT, CONFIG_ERROR, MEMORY_ERROR, NO_SUPPORTED_REPORTS };
static const char *const statuses[] = {"disabled","starting","collecting","ready","empty","stale",
                                     "blocked","dependency_missing","error","stopping"};
static const char *const reasons[] = {"none","another_neighbor_instance","diag_in_use","diag_mdlog_missing",
    "runtime_directory_error","qtrace_mask_error","collector_exited","capture_limit","capture_read_error",
    "capture_stalled","worker_exited","worker_timeout","invalid_config","memory_limit","no_supported_reports"};
struct wire_result {
    uint32_t magic, generation;
    int status, reason, collector_pid, exit_code;
    uint64_t capture_bytes;
    struct neighbor_result result;
};
struct wire_command { uint32_t magic, generation; };
struct capture_file { char path[PATH_MAX]; dev_t dev; ino_t ino; off_t size; int64_t mtime_ns; };
struct cursor { dev_t dev; ino_t ino; off_t offset, observed_size; uint64_t capture; int active; };
struct worker {
    int socket, lock_fd;
    pid_t parent, collector;
    uint32_t generation;
    char base[PATH_MAX], run[PATH_MAX], diag[PATH_MAX];
    int status, reason, exit_code, scan_error;
    int64_t progress_at, restart_at;
    uint64_t capture_bytes, next_capture;
    struct cursor cursors[MAX_FILES];
    struct neighbor_parser *parser;
};
static struct {
    int initialized, enabled, socket, config_error;
    pid_t worker;
    uint32_t generation;
    int64_t received_at, retry_at, kill_at;
    char config[PATH_MAX], context[1536], net[32768];
    struct wire_result latest;
} manager = {.socket = -1};
static volatile sig_atomic_t worker_run = 1;
static void worker_signal(int signum) { (void)signum; worker_run = 0; }
static int64_t milliseconds(void) {
    struct timespec t;
    if (clock_gettime(CLOCK_MONOTONIC, &t)) return 0;
    return (int64_t)t.tv_sec * 1000 + t.tv_nsec / 1000000;
}
static void pause_ms(int n) {
    struct timespec t = {n / 1000, (long)(n % 1000) * 1000000};
    while (nanosleep(&t, &t) && errno == EINTR && worker_run) {}
}
static int copy_path(char *out, size_t n, const char *s) {
    int len = snprintf(out, n, "%s", s);
    return len >= 0 && (size_t)len < n;
}
static int path_join(char *out, size_t n, const char *a, const char *b) {
    int len = snprintf(out, n, "%s/%s", a, b);
    return len >= 0 && (size_t)len < n;
}
static int secure_dir(const char *path) {
    struct stat st;
    if (mkdir(path, 0700) && errno != EEXIST) return 0;
    return !lstat(path, &st) && S_ISDIR(st.st_mode) && st.st_uid == geteuid() && !(st.st_mode & 0077);
}
/* Private capture directories only; never follow a symlink into other data. */
static int remove_tree(const char *path, int depth) {
    struct stat st;
    if (lstat(path, &st)) return errno == ENOENT;
    if (!S_ISDIR(st.st_mode)) return unlink(path) == 0;
    if (depth > 4 || st.st_uid != geteuid()) return 0;
    DIR *d = opendir(path); if (!d) return 0;
    struct dirent *e; int ok = 1, count = 0;
    while ((e = readdir(d))) {
        if (!strcmp(e->d_name, ".") || !strcmp(e->d_name, "..")) continue;
        char child[PATH_MAX];
        if (++count > MAX_SCAN_ENTRIES || !path_join(child, sizeof child, path, e->d_name) ||
            !remove_tree(child, depth + 1)) { ok = 0; break; }
    }
    closedir(d);
    return ok && rmdir(path) == 0;
}
static int write_all(int fd, const void *data, size_t n) {
    const unsigned char *p = data;
    while (n) {
        ssize_t sent = write(fd, p, n);
        if (sent < 0 && errno == EINTR) continue;
        if (sent <= 0) return 0;
        p += sent; n -= (size_t)sent;
    }
    return 1;
}
static void close_other_fds(int keep) {
    long n = sysconf(_SC_OPEN_MAX);
    if (n < 0 || n > 65536) n = 65536;
    for (int fd = 3; fd < n; fd++) if (fd != keep) close(fd);
}
static int foreign_diag(pid_t own) {
    DIR *d = opendir("/proc");
    if (!d) return 0; /* non-Linux unit tests use the private flock instead */
    struct dirent *e; int found = 0;
    while ((e = readdir(d))) {
        char *end; long pid = strtol(e->d_name, &end, 10);
        if (!*e->d_name || *end || pid <= 0 || pid == own || pid == getpid()) continue;
        char path[PATH_MAX], name[64];
        if (snprintf(path, sizeof path, "/proc/%ld/comm", pid) >= (int)sizeof path) continue;
        FILE *f = fopen(path, "r"); if (!f) continue;
        if (!fgets(name, sizeof name, f)) name[0] = 0;
        fclose(f);
        name[strcspn(name,"\r\n")] = 0;
        if (!strcmp(name,"diag_mdlog") || !strcmp(name,"diag_socket_log") || !strcmp(name,"diag_uart_log")) {
            found = 1; break;
        }
    }
    closedir(d); return found;
}
static int obtain_lock(struct worker *w) {
    if (w->lock_fd >= 0) return 1;
    if (!secure_dir(w->base)) { w->status=ERROR; w->reason=RUNTIME_ERROR; return 0; }
    char path[PATH_MAX];
    if (!path_join(path,sizeof path,w->base,"owner.lock")) return 0;
    int fd = open(path,O_RDWR|O_CREAT|O_CLOEXEC|O_NOFOLLOW,0600);
    struct stat st;
    if (fd < 0 || fstat(fd,&st) || !S_ISREG(st.st_mode) || st.st_uid != geteuid()) {
        if (fd >= 0) close(fd);
        w->status=ERROR; w->reason=RUNTIME_ERROR; return 0;
    }
    if (flock(fd,LOCK_EX|LOCK_NB)) {
        close(fd); w->status=BLOCKED; w->reason=LOCKED; return 0;
    }
    w->lock_fd=fd;
    /* Clean only our named private capture directories left by an interrupted
     * prior owner. Keeping owner.lock avoids lock-inode replacement races. */
    DIR *d=opendir(w->base); struct dirent *e;
    if (d) {
        int count=0;
        while ((e=readdir(d)) && count++ < MAX_SCAN_ENTRIES) {
            if (strncmp(e->d_name,"capture.",8)) continue;
            if (!path_join(path,sizeof path,w->base,e->d_name) || !remove_tree(path,0)) {
                closedir(d); close(w->lock_fd); w->lock_fd=-1;
                w->status=ERROR; w->reason=RUNTIME_ERROR; return 0;
            }
        }
        closedir(d);
    }
    return 1;
}
static void stop_collector(struct worker *w) {
    if (w->collector <= 0) return;
    pid_t pid = w->collector;
    int status;
    if (waitpid(pid,&status,WNOHANG)==0) {
        kill(pid,SIGTERM);
        int64_t deadline=milliseconds()+2000;
        while (waitpid(pid,&status,WNOHANG)==0 && milliseconds()<deadline) pause_ms(20);
        if (waitpid(pid,&status,WNOHANG)==0) {
            kill(pid,SIGKILL);
            while (waitpid(pid,&status,0)<0 && errno==EINTR) {}
        }
    }
    w->collector=0;
}
static void reset_capture(struct worker *w) {
    stop_collector(w);
    if (w->run[0]) { (void)remove_tree(w->run,0); w->run[0]=0; }
    neighbor_parser_free(w->parser); w->parser=neighbor_parser_new();
    memset(w->cursors,0,sizeof w->cursors);
    w->capture_bytes=0; w->scan_error=0; w->progress_at=milliseconds();
}
static int start_collector(struct worker *w) {
    if (access(w->diag,X_OK)) { w->status=DEPENDENCY_MISSING; w->reason=TOOL_MISSING; return 0; }
    if (foreign_diag(0)) { w->status=BLOCKED; w->reason=DIAG_BUSY; return 0; }
    if (snprintf(w->run,sizeof w->run,"%s/capture.XXXXXX",w->base) >= (int)sizeof w->run || !mkdtemp(w->run)) {
        w->run[0]=0; w->status=ERROR; w->reason=RUNTIME_ERROR; return 0;
    }
    char mask[PATH_MAX], ring[PATH_MAX], log[PATH_MAX];
    if (!path_join(mask,sizeof mask,w->run,"qtrace.cfg") || !path_join(ring,sizeof ring,w->run,"ring") ||
        !path_join(log,sizeof log,w->run,"diag.log") || !secure_dir(ring)) {
        w->status=ERROR; w->reason=RUNTIME_ERROR; return 0;
    }
    int fd=open(mask,O_WRONLY|O_CREAT|O_EXCL|O_CLOEXEC|O_NOFOLLOW,0600);
    int wrote=fd>=0 && write_all(fd,qtrace_mask,sizeof qtrace_mask);
    if (fd>=0) close(fd);
    if (!wrote) { w->status=ERROR; w->reason=MASK_ERROR; return 0; }
    pid_t parent=getpid(), pid=fork();
    if (pid<0) { w->status=ERROR; w->reason=COLLECTOR_EXITED; return 0; }
    if (pid==0) {
        signal(SIGTERM,SIG_DFL); signal(SIGINT,SIG_DFL); signal(SIGHUP,SIG_DFL); signal(SIGPIPE,SIG_DFL);
#ifdef __linux__
        if (prctl(PR_SET_PDEATHSIG,SIGTERM) || getppid()!=parent) _exit(125);
#else
        (void)parent;
#endif
        struct rlimit lim={16*1024*1024,16*1024*1024}; (void)setrlimit(RLIMIT_FSIZE,&lim);
        lim.rlim_cur=lim.rlim_max=0; (void)setrlimit(RLIMIT_CORE,&lim);
        int output=open(log,O_WRONLY|O_CREAT|O_APPEND|O_NOFOLLOW,0600);
        int input=open("/dev/null",O_RDONLY);
        if (output<0 || input<0 || dup2(output,STDOUT_FILENO)<0 || dup2(output,STDERR_FILENO)<0 || dup2(input,STDIN_FILENO)<0) _exit(126);
        close_other_fds(-1);
        execl(w->diag,w->diag,"-f",mask,"-o",ring,"-s","4","-n","4","-c","-d",(char *)NULL);
        _exit(127);
    }
    w->collector=pid; w->progress_at=milliseconds(); w->status=COLLECTING; w->reason=NONE;
    return 1;
}
static int is_qmdl(const char *name) {
    size_t n=strlen(name);
    return n>=5 && !strcmp(name+n-5,".qmdl");
}
static int scan_files(const char *path, struct capture_file *files, size_t *count,
                      uint64_t *bytes, int *entries, int depth) {
    if (depth>3) return 0;
    DIR *d=opendir(path); if (!d) return 0;
    struct dirent *e; int ok=1;
    while ((e=readdir(d))) {
        if (!strcmp(e->d_name,".") || !strcmp(e->d_name,"..")) continue;
        char file[PATH_MAX]; struct stat st;
        if (++*entries>MAX_SCAN_ENTRIES || !path_join(file,sizeof file,path,e->d_name)) { ok=0; break; }
        if (lstat(file,&st)) { if (errno==ENOENT) continue; ok=0; break; }
        if (S_ISLNK(st.st_mode)) { ok=0; break; }
        if (S_ISDIR(st.st_mode)) {
            if (!scan_files(file,files,count,bytes,entries,depth+1)) { ok=0; break; }
        } else if (S_ISREG(st.st_mode)) {
            if (st.st_size<0 || (uint64_t)st.st_size>MAX_CAPTURE_BYTES-*bytes) { ok=0; break; }
            *bytes+=(uint64_t)st.st_size;
            if (!is_qmdl(e->d_name)) continue;
            if (*count==MAX_FILES) { ok=0; break; }
            struct capture_file *f=&files[(*count)++];
            copy_path(f->path,sizeof f->path,file); f->dev=st.st_dev; f->ino=st.st_ino; f->size=st.st_size;
#ifdef __APPLE__
            f->mtime_ns=(int64_t)st.st_mtimespec.tv_sec*1000000000+st.st_mtimespec.tv_nsec;
#else
            f->mtime_ns=(int64_t)st.st_mtim.tv_sec*1000000000+st.st_mtim.tv_nsec;
#endif
        } else { ok=0; break; }
    }
    closedir(d); return ok;
}
static int file_order(const void *a,const void *b) {
    const struct capture_file *x=a,*y=b;
    if (x->mtime_ns!=y->mtime_ns) return x->mtime_ns<y->mtime_ns ? -1:1;
    return strcmp(x->path,y->path);
}
static int read_captures(struct worker *w,int64_t now) {
    struct capture_file files[MAX_FILES]; size_t count=0;
    uint64_t bytes=0; int entries=0;
    if (!scan_files(w->run,files,&count,&bytes,&entries,0)) {
        w->status=ERROR; w->reason=CAPTURE_LIMIT; return 0;
    }
    w->capture_bytes=bytes;
    qsort(files,count,sizeof files[0],file_order);
    for (size_t i=0;i<MAX_FILES;i++) {
        struct cursor *c=&w->cursors[i]; int found=0;
        if (!c->active) continue;
        for (size_t j=0;j<count;j++) if (c->dev==files[j].dev && c->ino==files[j].ino) { found=1; break; }
        if (!found) {
            if (c->offset < c->observed_size) w->scan_error=1;
            neighbor_parser_end_file(w->parser,c->capture); c->active=0;
        }
    }
    size_t budget=READ_BUDGET; unsigned char data[65536];
    for (size_t i=0;i<count && budget;i++) {
        struct capture_file *f=&files[i]; struct cursor *c=NULL;
        for (size_t j=0;j<MAX_FILES;j++) {
            struct cursor *candidate=&w->cursors[j];
            if (candidate->active && candidate->dev==f->dev && candidate->ino==f->ino) { c=candidate; break; }
        }
        if (!c) {
            for (size_t j=0;j<MAX_FILES;j++) if (!w->cursors[j].active) { c=&w->cursors[j]; break; }
            if (!c) { w->status=ERROR; w->reason=CAPTURE_LIMIT; return 0; }
            *c=(struct cursor){.dev=f->dev,.ino=f->ino,.capture=++w->next_capture,.active=1};
        }
        if (f->size<c->offset) { c->offset=0; c->capture=++w->next_capture; w->scan_error=1; }
        c->observed_size=f->size;
        if (f->size==c->offset) continue;
        int fd=open(f->path,O_RDONLY|O_NONBLOCK|O_CLOEXEC|O_NOFOLLOW); struct stat st;
        if (fd<0) { w->scan_error=1; continue; }
        if (fstat(fd,&st) || !S_ISREG(st.st_mode) || st.st_dev!=c->dev || st.st_ino!=c->ino) {
            close(fd); w->scan_error=1; continue;
        }
        while (c->offset<f->size && budget) {
            size_t want=(uint64_t)(f->size-c->offset)<sizeof data ? (size_t)(f->size-c->offset):sizeof data;
            if (want>budget) want=budget;
            ssize_t n=pread(fd,data,want,c->offset);
            if (n<0 && errno==EINTR) continue;
            if (n<=0) { w->scan_error=1; break; }
            neighbor_parser_feed(w->parser,data,(size_t)n,c->capture,now);
            c->offset+=n; budget-=(size_t)n; w->progress_at=now;
        }
        close(fd);
    }
    /* Keep the child's append-only diagnostic log bounded without changing its
     * descriptor. Capture files are never truncated while the child owns them. */
    char log[PATH_MAX]; struct stat st;
    if (path_join(log,sizeof log,w->run,"diag.log") && !lstat(log,&st) && S_ISREG(st.st_mode) && st.st_size>65536) {
        int fd=open(log,O_WRONLY|O_NOFOLLOW|O_CLOEXEC);
        if (fd>=0) { if (ftruncate(fd,0)) w->scan_error=1; close(fd); }
    }
    return 1;
}
static void publish_worker(struct worker *w,int64_t now) {
    struct wire_result msg; memset(&msg,0,sizeof msg);
    msg.magic=WIRE_MAGIC; msg.generation=w->generation; msg.status=w->status; msg.reason=w->reason;
    msg.collector_pid=(int)w->collector; msg.exit_code=w->exit_code; msg.capture_bytes=w->capture_bytes;
    if (w->status==COLLECTING || w->status==READY || w->status==EMPTY || w->status==STALE) {
        neighbor_parser_result(w->parser,now,&msg.result);
        if (w->scan_error) msg.result.partial=1;
        if (msg.result.count) msg.status=READY;
        else if (msg.result.reports) msg.status=msg.result.seen_ms && now-msg.result.seen_ms>NEIGHBOR_TTL_MS ? STALE:EMPTY;
        else if (msg.result.frames) { msg.status=EMPTY; msg.reason=NO_SUPPORTED_REPORTS; }
    }
    (void)send(w->socket,&msg,sizeof msg,MSG_DONTWAIT|MSG_NOSIGNAL);
}
static void worker_main(int socket,pid_t parent,uint32_t generation) {
    signal(SIGTERM,worker_signal); signal(SIGINT,worker_signal); signal(SIGHUP,worker_signal);
    signal(SIGUSR1,SIG_IGN); signal(SIGPIPE,SIG_IGN);
    worker_run=1;
    if (setsid()<0) _exit(125);
#ifdef __linux__
    if (prctl(PR_SET_PDEATHSIG,SIGTERM) || getppid()!=parent) _exit(125);
#endif
    close_other_fds(socket);
    struct worker w; memset(&w,0,sizeof w);
    w.socket=socket; w.parent=parent; w.generation=generation; w.lock_fd=-1; w.status=STARTING;
    const char *base=getenv("ZWRT_DATAD_NEIGHBOR_DIR"), *diag=getenv("ZWRT_DATAD_DIAG_BIN");
    if (!copy_path(w.base,sizeof w.base,base && *base ? base:RUNTIME_DEFAULT) ||
        !copy_path(w.diag,sizeof w.diag,diag && *diag ? diag:DIAG_DEFAULT)) _exit(125);
    w.parser=neighbor_parser_new();
    int failures=0;
    while (worker_run && getppid()==parent) {
        int64_t now=milliseconds(); struct wire_command cmd;
        while (recv(socket,&cmd,sizeof cmd,MSG_DONTWAIT)==sizeof cmd) {
            if (cmd.magic==WIRE_MAGIC && cmd.generation!=w.generation) {
                w.generation=cmd.generation; reset_capture(&w); w.status=STARTING; w.reason=NONE;
                w.restart_at=now+1000; failures=0;
            }
        }
        if (!w.parser) { w.status=ERROR; w.reason=MEMORY_ERROR; publish_worker(&w,now); break; }
        if (w.collector>0) {
            int status; pid_t done=waitpid(w.collector,&status,WNOHANG);
            if (done==w.collector || (done<0 && errno==ECHILD)) {
                w.exit_code=done>0 && WIFEXITED(status) ? WEXITSTATUS(status) : -1;
                w.collector=0; reset_capture(&w); w.status=ERROR; w.reason=COLLECTOR_EXITED;
                if (failures<3) failures++;
                w.restart_at=now+RETRY_MS*(1<<failures);
            } else if (foreign_diag(w.collector)) {
                reset_capture(&w); w.status=BLOCKED; w.reason=DIAG_BUSY; w.restart_at=now+RETRY_MS;
            } else if (!read_captures(&w,now)) {
                reset_capture(&w); w.status=ERROR; w.reason=CAPTURE_LIMIT; w.restart_at=now+30000;
            } else if (now-w.progress_at>STALE_TRANSPORT_MS) {
                reset_capture(&w); w.status=STALE; w.reason=NO_PROGRESS; w.restart_at=now+RETRY_MS;
            }
        } else if (now>=w.restart_at) {
            if (obtain_lock(&w)) {
                if (w.run[0]) reset_capture(&w);
                if (!start_collector(&w)) { w.restart_at=now+RETRY_MS; }
            } else w.restart_at=now+RETRY_MS;
        }
        publish_worker(&w,now);
        fd_set fds; FD_ZERO(&fds); FD_SET(socket,&fds); struct timeval tv={1,0};
        (void)select(socket+1,&fds,NULL,NULL,&tv);
    }
    stop_collector(&w);
    if (w.run[0]) (void)remove_tree(w.run,0);
    neighbor_parser_free(w.parser);
    if (w.lock_fd>=0) close(w.lock_fd);
    close(socket); _exit(0);
}
static int config_read(const char *path,int *enabled) {
    int fd=open(path,O_RDONLY|O_CLOEXEC|O_NOFOLLOW|O_NONBLOCK);
    if (fd<0) { if (errno==ENOENT) { *enabled=0; return 1; } return 0; }
    struct stat st; char data[1024],raw[32];
    if (fstat(fd,&st) || !S_ISREG(st.st_mode) || st.st_size<=0 || st.st_size>=(off_t)sizeof data) { close(fd); return 0; }
    ssize_t n=read(fd,data,sizeof data-1); close(fd);
    if (n!=st.st_size) return 0;
    data[n]=0;
    if (!json_is_valid_object(data) || !json_get(data,"enabled",raw,sizeof raw)) return 0;
    if (!strcmp(raw,"true") || !strcmp(raw,"1")) *enabled=1;
    else if (!strcmp(raw,"false") || !strcmp(raw,"0")) *enabled=0;
    else return 0;
    return 1;
}
static void blank_status(int status,int reason) {
    memset(&manager.latest,0,sizeof manager.latest);
    manager.latest.magic=WIRE_MAGIC; manager.latest.generation=manager.generation;
    manager.latest.status=status; manager.latest.reason=reason;
}
void neighbor_manager_init(int enabled,const char *config_file) {
    memset(&manager,0,sizeof manager); manager.socket=-1; manager.generation=1; manager.initialized=1;
    const char *configured=getenv("ZWRT_DATAD_NEIGHBOR_CONFIG");
    const char *path=config_file ? config_file : configured && *configured ? configured : CONFIG_DEFAULT;
    if (!copy_path(manager.config,sizeof manager.config,path)) { manager.config_error=1; blank_status(ERROR,CONFIG_ERROR); return; }
    if (enabled>=0) manager.enabled=enabled!=0;
    else if (!config_read(manager.config,&manager.enabled)) manager.config_error=1;
    blank_status(manager.config_error ? ERROR : manager.enabled ? STARTING : DISABLED,
                 manager.config_error ? CONFIG_ERROR : NONE);
}
static void request_stop(void) {
    if (manager.worker<=0 || manager.kill_at) return;
    kill(-manager.worker,SIGTERM); kill(manager.worker,SIGTERM);
    manager.kill_at=milliseconds()+3000;
}
int neighbor_manager_set_enabled(int enabled,char *err,size_t errlen) {
    if (!manager.initialized) { snprintf(err,errlen,"neighbor manager is not initialized"); return 0; }
    int previous;
    if (!config_read(manager.config,&previous) || previous!=enabled || access(manager.config,F_OK)) {
        char temp[PATH_MAX],data[64];
        if (snprintf(temp,sizeof temp,"%s.XXXXXX",manager.config)>=(int)sizeof temp) { snprintf(err,errlen,"neighbor config path is too long"); return 0; }
        int fd=mkstemp(temp);
        if (fd<0) { snprintf(err,errlen,"cannot create neighbor config"); return 0; }
        int len=snprintf(data,sizeof data,"{\"enabled\":%s}\n",enabled ? "true":"false");
        int ok=write_all(fd,data,(size_t)len) && fsync(fd)==0; close(fd);
        if (!ok || rename(temp,manager.config)) { unlink(temp); snprintf(err,errlen,"cannot save neighbor config"); return 0; }
    }
    manager.config_error=0;
    if (manager.enabled!=enabled) {
        manager.enabled=enabled; manager.generation++; manager.retry_at=0;
        int collector_pid=manager.latest.collector_pid;
        if (!enabled) request_stop();
        blank_status(enabled ? STARTING:DISABLED,NONE);
        if (!enabled) manager.latest.collector_pid=collector_pid;
    }
    return 1;
}
static void spawn_worker(void) {
    int pair[2];
    if (socketpair(AF_UNIX,SOCK_DGRAM,0,pair)) { blank_status(ERROR,WORKER_EXITED); manager.retry_at=milliseconds()+RETRY_MS; return; }
    for (int i=0;i<2;i++) {
        fcntl(pair[i],F_SETFD,FD_CLOEXEC); fcntl(pair[i],F_SETFL,O_NONBLOCK);
        int size=262144;
        (void)setsockopt(pair[i],SOL_SOCKET,SO_SNDBUF,&size,sizeof size);
        (void)setsockopt(pair[i],SOL_SOCKET,SO_RCVBUF,&size,sizeof size);
    }
    pid_t parent=getpid(),pid=fork();
    if (pid<0) { close(pair[0]); close(pair[1]); blank_status(ERROR,WORKER_EXITED); manager.retry_at=milliseconds()+RETRY_MS; return; }
    if (!pid) { close(pair[0]); worker_main(pair[1],parent,manager.generation); _exit(127); }
    close(pair[1]); manager.socket=pair[0]; manager.worker=pid;
    manager.received_at=milliseconds(); manager.kill_at=0; blank_status(STARTING,NONE);
}
static void make_context(char *out,size_t size,const char *net,const char *sim) {
    static const char *const net_keys[]={"network_type","nr5g_pci","nr5g_action_channel","nr5g_cell_id","lte_pci","wan_active_channel","cell_id","rmcc","rmnc"};
    static const char *const sim_keys[]={"current_sim_slot","sim_iccid","sim_states","modem_main_state"};
    size_t used=0; out[0]=0;
    for (size_t pass=0;pass<2;pass++) {
        const char *const *keys=pass ? sim_keys:net_keys;
        size_t count=pass ? sizeof sim_keys/sizeof sim_keys[0] : sizeof net_keys/sizeof net_keys[0];
        for (size_t i=0;i<count;i++) {
            char value[96]; value[0]=0; (void)json_get(pass ? sim:net,keys[i],value,sizeof value);
            /* MU5250 B28 alternates absent NR identity between 0 and UINT32_MAX.
             * These are equivalent unknowns, not a serving-cell transition. */
            if (!pass && !strcmp(keys[i],"nr5g_cell_id") && value[0]) {
                char *end; errno=0;
                unsigned long long id=strtoull(value,&end,(!strncmp(value,"0x",2) || !strncmp(value,"0X",2)) ? 16:10);
                while (isspace((unsigned char)*end)) end++;
                if ((!errno && end!=value && !*end && (id==0 || id==UINT32_MAX)) || !strcmp(value,"-1")) value[0]=0;
            }
            int n=snprintf(out+used,size-used,"%zu:%s|",strlen(value),value);
            if (n<0 || (size_t)n>=size-used) return;
            used+=(size_t)n;
        }
    }
}
void neighbor_manager_tick(const char *net,const char *sim) {
    if (!manager.initialized) return;
    int64_t now=milliseconds();
    if (net) copy_path(manager.net,sizeof manager.net,net);
    char context[sizeof manager.context]; make_context(context,sizeof context,net ? net:"{}",sim ? sim:"{}");
    if (strcmp(context,manager.context)) {
        int changed=manager.context[0]!=0;
        copy_path(manager.context,sizeof manager.context,context);
        if (changed && manager.enabled) { manager.generation++; blank_status(STARTING,NONE); }
    }
    if (manager.worker>0) {
        struct wire_result msg; ssize_t n;
        while ((n=recv(manager.socket,&msg,sizeof msg,MSG_DONTWAIT))>0) {
            if (n!=sizeof msg || msg.magic!=WIRE_MAGIC || msg.generation!=manager.generation ||
                msg.status<0 || msg.status>ERROR || msg.reason<0 || msg.reason>NO_SUPPORTED_REPORTS ||
                msg.result.count>NEIGHBOR_MAX_CELLS) continue;
            manager.received_at=now;
            if (manager.enabled && !manager.kill_at) manager.latest=msg;
        }
        int status; pid_t done=waitpid(manager.worker,&status,WNOHANG);
        if (done==manager.worker || (done<0 && errno==ECHILD)) {
            /* The worker's private process group owns only its collector. */
            kill(-manager.worker,SIGTERM);
            close(manager.socket); manager.socket=-1; manager.worker=0; manager.kill_at=0;
            if (manager.enabled) { blank_status(ERROR,WORKER_EXITED); manager.retry_at=now+RETRY_MS; }
        } else if (manager.kill_at) {
            if (now>=manager.kill_at) { kill(-manager.worker,SIGKILL); kill(manager.worker,SIGKILL); }
        } else if (!manager.enabled) request_stop();
        else if (now-manager.received_at>WORKER_TIMEOUT_MS) { blank_status(ERROR,WORKER_TIMEOUT); request_stop(); }
        else {
            struct wire_command cmd={WIRE_MAGIC,manager.generation};
            (void)send(manager.socket,&cmd,sizeof cmd,MSG_DONTWAIT|MSG_NOSIGNAL);
        }
    }
    if (manager.enabled && !manager.config_error && manager.worker<=0 && now>=manager.retry_at) spawn_worker();
}
void neighbor_manager_stop(void) {
    request_stop();
    int status;
    int64_t deadline=milliseconds()+4000;
    while (manager.worker>0) {
        pid_t done=waitpid(manager.worker,&status,WNOHANG);
        if (done==manager.worker || (done<0 && errno==ECHILD)) break;
        if (milliseconds()>=manager.kill_at) { kill(-manager.worker,SIGKILL); kill(manager.worker,SIGKILL); }
        if (milliseconds()>=deadline) break; /* Do not hang datad on uninterruptible I/O. */
        pause_ms(20);
    }
    if (manager.socket>=0) close(manager.socket);
    manager.socket=-1; manager.worker=0; manager.enabled=0;
}
struct output_buffer { char *data; size_t size,used; int failed; };
static void append(struct output_buffer *b,const char *fmt,...) {
    if (b->failed) return;
    va_list ap; va_start(ap,fmt); int n=vsnprintf(b->data+b->used,b->size-b->used,fmt,ap); va_end(ap);
    if (n<0 || (size_t)n>=b->size-b->used) b->failed=1;
    else b->used+=(size_t)n;
}
static int allowed_rat(const char *net,int rat) {
    char type[64]; if (!json_get(net,"network_type",type,sizeof type)) return 1;
    for (char *p=type;*p;p++) *p=(char)toupper((unsigned char)*p);
    if (strstr(type,"NSA")) return 1;
    if (!strcmp(type,"SA")) return rat==NEIGHBOR_NR;
    if (strstr(type,"LTE") || strstr(type,"4G") || !strcmp(type,"13")) return rat==NEIGHBOR_LTE;
    return 1; /* Keep both RATs when the firmware uses an unrecognized mode label. */
}
static long current_frequency(const char *net,int rat) {
    if (!allowed_rat(net,rat)) return -1;
    long value=json_get_int(net,rat==NEIGHBOR_NR ? "nr5g_action_channel":"wan_active_channel",-1);
    return rat==NEIGHBOR_NR && value<=0 ? -1:value;
}
static int is_current(const char *net,const struct neighbor_cell *c) {
    long frequency=current_frequency(net,c->rat);
    long pci=json_get_int(net,c->rat==NEIGHBOR_NR ? "nr5g_pci":"lte_pci",-1);
    if (frequency>=0 && (uint64_t)frequency==c->arfcn && pci==c->pci && (c->arfcn || c->rat==NEIGHBOR_LTE)) return 1;
    if (!c->arfcn && c->rat==NEIGHBOR_NR) return 0;
    char ca[8192]; if (!json_get(net,c->rat==NEIGHBOR_NR ? "nrca":"lteca",ca,sizeof ca)) return 0;
    char *row_save=NULL;
    for (char *row=strtok_r(ca,";",&row_save);row;row=strtok_r(NULL,";",&row_save)) {
        /* Preserve empty columns; strtok on commas would shift field indices. */
        char *fields[16],*p=row; size_t count=0;
        while (count<16) { fields[count++]=p; char *comma=strchr(p,','); if (!comma) break; *comma=0; p=comma+1; }
        size_t pi=count>=10 ? 1:0;
        size_t fi=count>=10 ? 4:3;
        if (count<=fi) continue;
        char *end_p,*end_f; errno=0;
        long cp=strtol(fields[pi],&end_p,10), cf=strtol(fields[fi],&end_f,10);
        while (isspace((unsigned char)*end_p)) end_p++;
        while (isspace((unsigned char)*end_f)) end_f++;
        if (!errno && end_p!=fields[pi] && end_f!=fields[fi] && !*end_p && !*end_f && cp==c->pci && cf>=0 && (uint64_t)cf==c->arfcn) return 1;
    }
    return 0;
}
void neighbor_manager_json(char *out,size_t size,const char *net) {
    if (!size) return;
    if (!net) net=manager.net[0] ? manager.net:"{}";
    struct wire_result *msg=&manager.latest; int64_t now=milliseconds();
    int state=manager.config_error ? ERROR : !manager.enabled ? DISABLED : msg->status;
    int reason=manager.config_error ? CONFIG_ERROR : !manager.enabled ? NONE : msg->reason;
    if (!manager.enabled && manager.worker>0) state=STOPPING;
    if (state<0 || state>STOPPING) state=ERROR;
    if (reason<0 || reason>NO_SUPPORTED_REPORTS) reason=WORKER_EXITED;
    int active=manager.enabled && msg->generation==manager.generation && (state==READY || state==EMPTY || state==STALE || state==COLLECTING);
    size_t visible=0;
    if (active) for (size_t i=0;i<msg->result.count;i++) {
        struct neighbor_cell *c=&msg->result.cells[i];
        if (now<c->seen_ms || now-c->seen_ms>NEIGHBOR_TTL_MS || !allowed_rat(net,c->rat) || is_current(net,c)) continue;
        visible++;
    }
    if (state==READY && !visible) state=msg->result.seen_ms && now-msg->result.seen_ms>NEIGHBOR_TTL_MS ? STALE:EMPTY;
    struct output_buffer b={out,size,0,0};
    append(&b,"{\"enabled\":%s,\"status\":\"%s\",\"reason\":\"%s\",\"source\":\"qtrace\",\"generation\":%u,",
           manager.enabled ? "true":"false",statuses[state],reasons[reason],manager.generation);
    append(&b,"\"collector_running\":%s,\"capture_bytes\":%" PRIu64 ",\"sampled_at\":",(active || state==STOPPING) && msg->collector_pid>0 ? "true":"false",msg->capture_bytes);
    int64_t age=msg->result.seen_ms>0 && now>=msg->result.seen_ms ? now-msg->result.seen_ms:-1;
    if (active && age>=0) append(&b,"%lld,\"age_ms\":%lld",(long long)time(NULL)-age/1000,(long long)age);
    else append(&b,"null,\"age_ms\":null");
    append(&b,",\"partial\":%s,\"frames\":%" PRIu64 ",\"malformed\":%" PRIu64 ",\"discarded\":%" PRIu64 ",\"ambiguous_measurements\":%" PRIu64 ",\"cells\":[",
           msg->result.partial ? "true":"false",msg->result.frames,msg->result.malformed,msg->result.discarded,msg->result.ambiguous);
    size_t emitted=0;
    if (active) for (size_t i=0;i<msg->result.count;i++) {
        struct neighbor_cell *c=&msg->result.cells[i];
        if (now<c->seen_ms || now-c->seen_ms>NEIGHBOR_TTL_MS || !allowed_rat(net,c->rat) || is_current(net,c)) continue;
        if (emitted++) append(&b,",");
        append(&b,"{\"rat\":\"%s\",\"pci\":%d,\"arfcn\":",c->rat==NEIGHBOR_NR ? "NR":"LTE",c->pci);
        if (c->arfcn || c->rat==NEIGHBOR_LTE) append(&b,"%u",c->arfcn); else append(&b,"null");
        append(&b,",\"band\":"); if (c->band) append(&b,"%d",c->band); else append(&b,"null");
        append(&b,",\"rsrp_dbm\":"); if (c->has_rsrp) append(&b,"%.2f",c->rsrp_dbm); else append(&b,"null");
        long current=current_frequency(net,c->rat);
        const char *relation=(c->arfcn || c->rat==NEIGHBOR_LTE) && current>=0 ? ((uint64_t)current==c->arfcn ? "intra":"inter"):"unknown";
        const char *evidence=c->evidence==NEIGHBOR_EXPLICIT ? "explicit":c->evidence==NEIGHBOR_ASSOCIATED ? "associated":"unknown";
        append(&b,",\"frequency_relation\":\"%s\",\"frequency_evidence\":\"%s\",\"samples\":%u,\"direct_hits\":%u,\"age_ms\":%lld}",
               relation,evidence,c->samples,c->direct_hits,(long long)(now-c->seen_ms));
    }
    append(&b,"]}");
    if (b.failed) snprintf(out,size,"{\"enabled\":%s,\"status\":\"error\",\"reason\":\"response_too_large\",\"cells\":[]}",manager.enabled ? "true":"false");
}
