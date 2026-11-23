/* capcheck.c -- Phase 0 / 0-F container capability probe.
 *
 * Compiled on the host as a FULLY STATIC binary so it can be dropped into any
 * distribution container without a glibc ABI dependency (that is itself part
 * of the experiment: static is the only universally runnable transport).
 *
 * It answers, with real syscalls and real errno strings:
 *   - can we create an inotify instance and add a watch?
 *   - can we bind + listen + connect on a unix socket under /tmp?
 *   - can we open/read /proc and list it?
 *   - can we create a pipe, fork, and a netlink socket (seccomp-sensitive)?
 *
 * Output: one JSON object. Never exits non-zero on a failed *capability*;
 * a failed capability is data, not an error. Exits 2 only on usage failure.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <dirent.h>
#include <fcntl.h>
#include <sys/inotify.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/un.h>
#include <sys/wait.h>

static void jstr(const char *s) {
    putchar('"');
    for (const unsigned char *p = (const unsigned char *)s; *p; p++) {
        if (*p == '"' || *p == '\\')
            printf("\\%c", *p);
        else if (*p < 0x20)
            printf("\\u%04x", *p);
        else
            putchar(*p);
    }
    putchar('"');
}

/* Report a syscall outcome as: "ok" or "EACCES: Permission denied". */
static void jres(const char *label, int ok, int err) {
    printf("    \"%s\": ", label);
    if (ok) {
        jstr("ok");
    } else {
        char buf[256];
        snprintf(buf, sizeof buf, "%s: %s",
                 err ? strerrorname_np(err) : "?", strerror(err));
        jstr(buf);
    }
    printf(",\n");
}

static int count_proc_numeric(void) {
    DIR *d = opendir("/proc");
    if (!d) return -1;
    int n = 0;
    struct dirent *e;
    while ((e = readdir(d)) != NULL) {
        if (e->d_name[0] >= '0' && e->d_name[0] <= '9') n++;
    }
    closedir(d);
    return n;
}

int main(void) {
    printf("{\n");

    /* ---- inotify: instance + watch ---- */
    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    int ifd_err = errno;
    int wd = -1, wd_err = 0;
    if (ifd >= 0) {
        wd = inotify_add_watch(ifd, "/tmp", IN_CREATE | IN_DELETE | IN_MODIFY);
        wd_err = errno;
    }
    printf("  \"inotify\": {\n");
    jres("init", ifd >= 0, ifd_err);
    jres("add_watch_tmp", wd >= 0, wd_err);
    printf("    \"watch_descriptor\": %d\n", wd);
    printf("  },\n");

    /* ---- unix socket bind in /tmp ---- */
    char path[128];
    snprintf(path, sizeof path, "/tmp/li-0f-cap-%d.sock", (int)getpid());
    unlink(path);
    int sfd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    int sfd_err = errno;
    int bind_ok = 0, bind_err = 0, listen_ok = 0, listen_err = 0, conn_ok = 0,
        conn_err = 0;
    if (sfd >= 0) {
        struct sockaddr_un sa;
        memset(&sa, 0, sizeof sa);
        sa.sun_family = AF_UNIX;
        size_t plen = strlen(path);
        if (plen >= sizeof sa.sun_path) plen = sizeof sa.sun_path - 1;
        memcpy(sa.sun_path, path, plen);
        sa.sun_path[plen] = '\0';
        bind_ok = (bind(sfd, (struct sockaddr *)&sa, sizeof sa) == 0);
        bind_err = errno;
        if (bind_ok) {
            listen_ok = (listen(sfd, 4) == 0);
            listen_err = errno;
        }
        if (listen_ok) {
            int cfd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
            if (cfd >= 0) {
                conn_ok = (connect(cfd, (struct sockaddr *)&sa, sizeof sa) == 0);
                conn_err = errno;
                close(cfd);
            } else {
                conn_err = errno;
            }
        }
    }
    printf("  \"unix_socket\": {\n");
    jres("socket", sfd >= 0, sfd_err);
    jres("bind_tmp", bind_ok, bind_err);
    jres("listen", listen_ok, listen_err);
    jres("connect", conn_ok, conn_err);
    printf("    \"path\": ");
    jstr(path);
    printf("\n  },\n");
    if (bind_ok) unlink(path);
    if (sfd >= 0) close(sfd);

    /* ---- procfs access ---- */
    int proc_n = count_proc_numeric();
    int proc_err = errno;
    char comm[64] = {0};
    int fd = open("/proc/1/comm", O_RDONLY | O_CLOEXEC);
    int comm_err = errno;
    if (fd >= 0) {
        ssize_t r = read(fd, comm, sizeof comm - 1);
        if (r > 0) {
            comm[r] = 0;
            char *nl = strchr(comm, '\n');
            if (nl) *nl = 0;
        }
        close(fd);
    }
    printf("  \"procfs\": {\n");
    jres("opendir_proc", proc_n >= 0, proc_err);
    printf("    \"numeric_entries\": %d,\n", proc_n);
    jres("read_proc_1_comm", fd >= 0, comm_err);
    printf("    \"pid1_comm\": ");
    jstr(comm);
    printf("\n  },\n");

    /* ---- seccomp-sensitive syscalls ---- */
    int nls = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, 0);
    int nls_err = errno;
    if (nls >= 0) close(nls);

    int pfd[2];
    int pipe_err = 0, pipe_ok = (pipe2(pfd, O_CLOEXEC) == 0);
    pipe_err = errno;
    if (pipe_ok) {
        close(pfd[0]);
        close(pfd[1]);
    }

    int fork_ok = 0, fork_err = 0;
    pid_t pid = fork();
    fork_err = errno;
    if (pid == 0) _exit(0);
    if (pid > 0) {
        int st = 0;
        fork_ok = (waitpid(pid, &st, 0) == pid) && WIFEXITED(st);
    }

    printf("  \"syscalls\": {\n");
    jres("netlink_socket", nls >= 0, nls_err);
    jres("pipe2", pipe_ok, pipe_err);
    jres("fork_waitpid", fork_ok, fork_err);
    printf("    \"uid\": %d,\n", (int)getuid());
    printf("    \"euid\": %d\n", (int)geteuid());
    printf("  }\n");

    printf("}\n");
    if (ifd >= 0) close(ifd);
    return 0;
}
