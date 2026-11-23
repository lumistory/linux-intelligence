/* li_emit.c -- minimal "daemon sink" helper used by experiment 00-a.
 *
 * Reads up to 64 KiB of stdin and writes it to an AF_UNIX SOCK_STREAM socket.
 * Never blocks the caller for long: connect() is non-blocking with a hard
 * timeout, so a hung or absent daemon costs microseconds, not seconds.
 *
 * exit 0 = delivered, 1 = no socket / timeout / EPIPE.  Never prints.
 */
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stddef.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#define MAXPAYLOAD 65536
#define CONNECT_TIMEOUT_MS 20

static int dial(const char *path) {
    struct sockaddr_un sa;
    int fd;
    size_t len = strlen(path);
    if (len == 0 || len >= sizeof(sa.sun_path)) return -1;
    fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (fd < 0) return -1;
    memset(&sa, 0, sizeof sa);
    sa.sun_family = AF_UNIX;
    memcpy(sa.sun_path, path, len);
    if (fcntl(fd, F_SETFL, O_NONBLOCK) < 0) { close(fd); return -1; }
    if (connect(fd, (struct sockaddr *)&sa, sizeof sa) < 0) {
        struct pollfd p = { fd, POLLOUT, 0 };
        if (errno != EINPROGRESS && errno != EAGAIN) { close(fd); return -1; }
        if (poll(&p, 1, CONNECT_TIMEOUT_MS) <= 0) { close(fd); return -1; }
    }
    { int fl = fcntl(fd, F_GETFL, 0);
      if (fl >= 0) (void)fcntl(fd, F_SETFL, fl & ~O_NONBLOCK); }
    return fd;
}

int main(int argc, char **argv) {
    const char *path = (argc > 1) ? argv[1] : NULL;
    static char buf[MAXPAYLOAD];
    ssize_t n = 0, r;
    int fd;
    if (!path || !*path) {
        path = NULL;
        /* fall back to a fixed path so the shell snippet stays tiny */
        path = "/run/li/events.sock";
    }
    fd = dial(path);
    if (fd < 0) {
        /* still drain stdin so the writer never sees EPIPE stalls */
        while ((r = read(0, buf, sizeof buf)) > 0) { }
        return 1;
    }
    while (n < (ssize_t)sizeof buf &&
           (r = read(0, buf + n, sizeof buf - n)) > 0) n += r;
    if (n > 0 && write(fd, buf, (size_t)n) < 0) { close(fd); return 1; }
    close(fd);
    return 0;
}
