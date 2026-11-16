// The bind sequence of giong's UDP rebind test, without giong: does the
// kernel refuse a bind of a UDP port that nothing holds (or that only
// sockets which allow it hold)? Prints every refusal with the step that
// failed and whether the same bind succeeds right after.
//
//   udp-rebind-repro ITERATIONS [connect=0|1] [late_close=0|1] [noise=N]
//
// connect=1: a released socket connects to its own address, as giong does.
// late_close=1: released sockets are closed by another thread a moment
// later, as giong's io_uring driver does; 0 closes them at once (epoll).
// noise=N: N threads bind port 0, send and close all the time, like the
// other tests of the binary.
// hold=N: N sockets bound to port 0 stay open for the whole run, as in the
// test that binds 1,000 of them (more sockets per slot of the kernel's
// UDP hash table).
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

static int opt_connect = 1, opt_late = 0;
static atomic_int stop;

static struct sockaddr_in addr_of(int port) {
    struct sockaddr_in a = {0};
    a.sin_family = AF_INET;
    a.sin_port = htons(port);
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    return a;
}

static int sock(int reuse_addr, int reuse_port) {
    int s = socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    if (s < 0) { perror("socket"); exit(2); }
    setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &reuse_addr, sizeof reuse_addr);
    setsockopt(s, SOL_SOCKET, SO_REUSEPORT, &reuse_port, sizeof reuse_port);
    return s;
}

static int try_bind(int s, int port) {
    struct sockaddr_in a = addr_of(port);
    return bind(s, (struct sockaddr *)&a, sizeof a) == 0 ? 0 : errno;
}

// Late closes: a tiny queue drained by a closer thread.
#define QN 4096
static int q[QN];
static atomic_uint qhead, qtail;
static void *closer(void *arg) {
    (void)arg;
    while (!atomic_load(&stop) || atomic_load(&qtail) != atomic_load(&qhead)) {
        unsigned t = atomic_load(&qtail);
        if (t == atomic_load(&qhead)) { usleep(20); continue; }
        close(q[t % QN]);
        atomic_store(&qtail, t + 1);
    }
    return NULL;
}

static void release_and_close(int s, int port) {
    int one = 1;
    setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    if (opt_connect) {
        struct sockaddr_in a = addr_of(port);
        connect(s, (struct sockaddr *)&a, sizeof a);
    }
    if (opt_late) {
        unsigned h = atomic_load(&qhead);
        q[h % QN] = s;
        atomic_store(&qhead, h + 1);
    } else {
        close(s);
    }
}

static void *noise(void *arg) {
    (void)arg;
    char buf[16] = "x";
    while (!atomic_load(&stop)) {
        int a = sock(0, 0), b = sock(0, 0);
        try_bind(a, 0);
        try_bind(b, 0);
        struct sockaddr_in ba; socklen_t l = sizeof ba;
        getsockname(b, (struct sockaddr *)&ba, &l);
        sendto(a, buf, 1, 0, (struct sockaddr *)&ba, l);
        close(a);
        close(b);
    }
    return NULL;
}

static int free_port(unsigned *seed) {
    for (;;) {
        int port = 1024 + rand_r(seed) % (32768 - 1024);
        int s = sock(0, 0);
        int e = try_bind(s, port);
        close(s);
        if (!e) return port;
    }
}

int main(int argc, char **argv) {
    long iters = argc > 1 ? atol(argv[1]) : 1000;
    int nnoise = 2, hold = 0;
    for (int i = 2; i < argc; i++) {
        if (!strncmp(argv[i], "connect=", 8)) opt_connect = atoi(argv[i] + 8);
        else if (!strncmp(argv[i], "late_close=", 11)) opt_late = atoi(argv[i] + 11);
        else if (!strncmp(argv[i], "noise=", 6)) nnoise = atoi(argv[i] + 6);
        else if (!strncmp(argv[i], "hold=", 5)) hold = atoi(argv[i] + 5);
    }
    for (int i = 0; i < hold; i++) try_bind(sock(0, 0), 0);
    pthread_t th[64], ct;
    for (int i = 0; i < nnoise && i < 64; i++) pthread_create(&th[i], NULL, noise, NULL);
    pthread_create(&ct, NULL, closer, NULL);
    unsigned seed = (unsigned)time(NULL) ^ (unsigned)getpid();
    long fails = 0, binds = 0;
    for (long it = 0; it < iters; it++) {
        int port = free_port(&seed);
        int s1 = sock(0, 1), s2 = sock(0, 1);
        if (try_bind(s1, port) || try_bind(s2, port)) { close(s1); close(s2); continue; }
        release_and_close(s1, port);
        release_and_close(s2, port);
        for (int round = 0; round < 20; round++) {
            // giong's probe: SO_REUSEADDR (the port has released sockets).
            int p = sock(1, 0);
            int e = try_bind(p, port);
            close(p);
            binds++;
            const char *step = "probe (SO_REUSEADDR)";
            int s = -1;
            if (!e) {
                s = sock(0, 1);
                e = try_bind(s, port);
                binds++;
                step = "socket (SO_REUSEPORT)";
            }
            if (e) {
                // Is the port refused still? The same bind, at once.
                int r = sock(e && s >= 0 ? 0 : 1, s >= 0 ? 1 : 0);
                int again = try_bind(r, port);
                close(r);
                fails++;
                printf("REFUSED it=%ld round=%d port=%d step=%s errno=%d (%s); same bind right after: %s\n",
                       it, round, port, step, e, strerror(e), again ? strerror(again) : "ok");
                fflush(stdout);
                if (s >= 0) close(s);
                break;
            }
            release_and_close(s, port);
        }
    }
    atomic_store(&stop, 1);
    for (int i = 0; i < nnoise && i < 64; i++) pthread_join(th[i], NULL);
    pthread_join(ct, NULL);
    printf("refused %ld of %ld binds (connect=%d late_close=%d noise=%d hold=%d)\n", fails,
           binds, opt_connect, opt_late, nnoise, hold);
    return fails ? 1 : 0;
}
