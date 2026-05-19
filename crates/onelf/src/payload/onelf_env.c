/*
 * onelf payload-side environment constructor (freestanding).
 *
 * Built as a tiny `-nostdlib -shared` object with NO DT_NEEDED of its
 * own. It is bundled into a package's lib/ and injected as a DT_NEEDED
 * of the entrypoint binary. Because DT_NEEDED lives in the ELF (not the
 * environment) and the entrypoint carries a baked-in $ORIGIN RUNPATH,
 * this object is loaded on *every* exec of the entrypoint, including
 * after the application re-execs itself in a sandbox that calls
 * clearenv() + execve().
 *
 * Its .init_array constructor runs before main(). It:
 *   1. self-locates via /proc/self/maps (no env / argv / CWD reliance),
 *   2. walks up to the package root (the dir containing `.onelf/`),
 *   3. re-applies `.onelf/env`     (KEY=VALUE, ${ONELF_DIR} expanded,
 *                                   set with overwrite), and
 *   4. re-applies `.onelf/preload` (one lib path per line, dlopen'd).
 *
 * The only libc symbols referenced are `setenv` and `dlopen`, both
 * unversioned and ABI-stable across glibc and musl. They are left
 * UNDEFINED here and resolved at load time from the application's own
 * libc (global symbol scope), so a single per-arch blob works for both
 * libc families. File I/O is done with raw syscalls.
 */

typedef unsigned long  u64;
typedef unsigned int   u32;
typedef long           i64;
typedef unsigned char  u8;

#define NULL ((void *)0)
#define O_RDONLY 0
#define AT_FDCWD (-100)

/* RTLD flags: identical values on glibc and musl. */
#define RTLD_NOW    0x0002
#define RTLD_GLOBAL 0x0100

/* Resolved from the application's libc at load time. */
extern int   setenv(const char *name, const char *value, int overwrite);
extern void *dlopen(const char *filename, int flags);
extern char *getenv(const char *name);

/* ---- raw syscalls -------------------------------------------------- */

#if defined(__x86_64__)
static inline i64 sys3(i64 nr, i64 a, i64 b, i64 c) {
    i64 ret;
    __asm__ volatile("syscall" : "=a"(ret)
        : "a"(nr), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory");
    return ret;
}
static inline i64 sys4(i64 nr, i64 a, i64 b, i64 c, i64 d) {
    i64 ret;
    register i64 r10 __asm__("r10") = d;
    __asm__ volatile("syscall" : "=a"(ret)
        : "a"(nr), "D"(a), "S"(b), "d"(c), "r"(r10) : "rcx", "r11", "memory");
    return ret;
}
#define NR_OPENAT 257
#define NR_READ   0
#define NR_CLOSE  3
static inline i64 sys_openat(const char *p) {
    return sys4(NR_OPENAT, AT_FDCWD, (i64)p, O_RDONLY, 0);
}
static inline i64 sys_read(int fd, void *b, u64 n) {
    return sys3(NR_READ, fd, (i64)b, (i64)n);
}
static inline i64 sys_close(int fd) { return sys3(NR_CLOSE, fd, 0, 0); }

#elif defined(__aarch64__)
static inline i64 svc4(i64 nr, i64 a, i64 b, i64 c, i64 d) {
    register i64 x0 __asm__("x0") = a;
    register i64 x1 __asm__("x1") = b;
    register i64 x2 __asm__("x2") = c;
    register i64 x3 __asm__("x3") = d;
    register i64 x8 __asm__("x8") = nr;
    __asm__ volatile("svc #0" : "+r"(x0)
        : "r"(x1), "r"(x2), "r"(x3), "r"(x8) : "memory");
    return x0;
}
#define NR_OPENAT 56
#define NR_READ   63
#define NR_CLOSE  57
static inline i64 sys_openat(const char *p) {
    return svc4(NR_OPENAT, AT_FDCWD, (i64)p, O_RDONLY, 0);
}
static inline i64 sys_read(int fd, void *b, u64 n) {
    return svc4(NR_READ, fd, (i64)b, (i64)n, 0);
}
static inline i64 sys_close(int fd) { return svc4(NR_CLOSE, fd, 0, 0, 0); }
#else
#error "unsupported architecture"
#endif

/* ---- tiny helpers (no libc string funcs) --------------------------- */

static u64 slen(const char *s) {
    u64 n = 0;
    while (s[n]) n++;
    return n;
}

/* Read an entire file into `buf` (capacity `cap`). Returns the byte
 * count, or -1 on open failure. Truncates silently if larger than cap;
 * env/preload files are tiny so this is fine. */
static i64 read_file(const char *path, char *buf, u64 cap) {
    i64 fd = sys_openat(path);
    if (fd < 0) return -1;
    u64 off = 0;
    while (off < cap) {
        i64 r = sys_read((int)fd, buf + off, cap - off);
        if (r <= 0) break;
        off += (u64)r;
    }
    sys_close((int)fd);
    return (i64)off;
}

/* Copy `src` into `dst` (capacity `cap`, NUL-terminated). Returns the
 * length written (excluding NUL), or -1 if it would overflow. */
static i64 scopy(char *dst, u64 cap, const char *src, u64 n) {
    if (n + 1 > cap) return -1;
    for (u64 i = 0; i < n; i++) dst[i] = src[i];
    dst[n] = 0;
    return (i64)n;
}

/* ---- self-location via /proc/self/maps ----------------------------- */

static char g_maps[65536];
static char g_root[4096];

/* Marker whose address falls inside this object's executable mapping. */
static void onelf_env_init(void);

/* Parse a hex number from `*p`, advancing the pointer. */
static u64 parse_hex(const char **p) {
    u64 v = 0;
    const char *s = *p;
    for (;;) {
        char c = *s;
        u64 d;
        if (c >= '0' && c <= '9') d = (u64)(c - '0');
        else if (c >= 'a' && c <= 'f') d = (u64)(c - 'a' + 10);
        else if (c >= 'A' && c <= 'F') d = (u64)(c - 'A' + 10);
        else break;
        v = (v << 4) | d;
        s++;
    }
    *p = s;
    return v;
}

/* Find the package root: the directory containing a `.onelf/`. Writes
 * it (NUL-terminated) into g_root and returns 1, or 0 if not found. */
static int find_root(void) {
    u64 self = (u64)(void *)&onelf_env_init;

    i64 n = read_file("/proc/self/maps", g_maps, sizeof(g_maps) - 1);
    if (n <= 0) return 0;
    g_maps[n] = 0;

    /* Scan lines: "start-end perms off dev inode   /path/to/lib.so" */
    const char *line = g_maps;
    const char *so = NULL;
    u64 so_len = 0;
    while (*line) {
        const char *p = line;
        u64 start = parse_hex(&p);
        u64 end = 0;
        if (*p == '-') { p++; end = parse_hex(&p); }

        const char *eol = p;
        while (*eol && *eol != '\n') eol++;

        if (self >= start && self < end) {
            /* Pathname is the last field; find the first '/' on the line. */
            const char *path = p;
            while (path < eol && *path != '/') path++;
            if (path < eol) {
                so = path;
                so_len = (u64)(eol - path);
            }
            break;
        }
        line = (*eol == '\n') ? eol + 1 : eol;
    }
    if (!so || so_len == 0) return 0;

    /* Walk up parent directories looking for a child `.onelf/env`. */
    char cand[4096];
    if (scopy(cand, sizeof(cand), so, so_len) < 0) return 0;

    for (int depth = 0; depth < 10; depth++) {
        /* Strip the trailing component (the last '/'). */
        u64 l = slen(cand);
        while (l > 1 && cand[l - 1] != '/') l--;
        if (l <= 1) break;       /* reached "/" */
        cand[l - 1] = 0;         /* drop the slash -> directory path */

        /* Probe "<cand>/.onelf/env". */
        char probe[4096];
        u64 cl = slen(cand);
        const char *suffix = "/.onelf/env";
        u64 sfl = slen(suffix);
        if (cl + sfl + 1 > sizeof(probe)) continue;
        for (u64 i = 0; i < cl; i++) probe[i] = cand[i];
        for (u64 i = 0; i <= sfl; i++) probe[cl + i] = suffix[i];

        i64 fd = sys_openat(probe);
        if (fd >= 0) {
            sys_close((int)fd);
            return scopy(g_root, sizeof(g_root), cand, cl) >= 0;
        }
        /* Probe "<cand>/.onelf/preload" too (env may be absent). */
        const char *suffix2 = "/.onelf/preload";
        u64 sfl2 = slen(suffix2);
        if (cl + sfl2 + 1 <= sizeof(probe)) {
            for (u64 i = 0; i <= sfl2; i++) probe[cl + i] = suffix2[i];
            i64 fd2 = sys_openat(probe);
            if (fd2 >= 0) {
                sys_close((int)fd2);
                return scopy(g_root, sizeof(g_root), cand, cl) >= 0;
            }
        }
    }
    return 0;
}

/* ---- apply .onelf/env and .onelf/preload --------------------------- */

static char g_buf[65536];

/* NUL-terminated string equality. */
static int seq(const char *a, const char *b) {
    while (*a && *a == *b) { a++; b++; }
    return *a == 0 && *b == 0;
}

/* Expand "${NAME}" in [src, src+n): ${ONELF_DIR} -> g_root (package
 * root), any other ${NAME} -> getenv(NAME) from the live environment.
 * Supports POSIX `${NAME:-word}`: if NAME is unset *or empty*, the
 * literal `word` is substituted instead. This is what makes the
 * default `PATH=${ONELF_DIR}/bin:${PATH:-/usr/bin:/bin}` keep the
 * inherited PATH yet avoid a dangling empty element after clearenv().
 * No nested braces inside `word`. An unterminated "${" is copied
 * literally. Writes NUL-terminated into dst (cap). Returns 0 on overflow. */
static int expand(char *dst, u64 cap, const char *src, u64 n) {
    u64 o = 0;
    for (u64 i = 0; i < n;) {
        if (i + 1 < n && src[i] == '$' && src[i + 1] == '{') {
            u64 j = i + 2;
            while (j < n && src[j] != '}') j++;
            if (j < n) {
                /* Split token src[i+2, j) on the first ":-". */
                u64 ts = i + 2, te = j;
                u64 name_end = te, def_start = 0;
                int has_def = 0;
                for (u64 p = ts; p + 1 < te; p++) {
                    if (src[p] == ':' && src[p + 1] == '-') {
                        name_end = p;
                        def_start = p + 2;
                        has_def = 1;
                        break;
                    }
                }
                char name[256];
                u64 nl = name_end - ts;
                if (nl < sizeof(name)) {
                    for (u64 k = 0; k < nl; k++) name[k] = src[ts + k];
                    name[nl] = 0;
                    const char *rep = seq(name, "ONELF_DIR")
                                          ? (const char *)g_root
                                          : getenv(name);
                    if (rep && rep[0]) {
                        for (u64 k = 0; rep[k]; k++) {
                            if (o + 1 >= cap) return 0;
                            dst[o++] = rep[k];
                        }
                    } else if (has_def) {
                        for (u64 k = def_start; k < te; k++) {
                            if (o + 1 >= cap) return 0;
                            dst[o++] = src[k];
                        }
                    }
                    i = j + 1;
                    continue;
                }
            }
        }
        if (o + 1 >= cap) return 0;
        dst[o++] = src[i++];
    }
    if (o >= cap) return 0;
    dst[o] = 0;
    return 1;
}

/* Trim ASCII whitespace by adjusting [*s, *e). */
static void trim(const char **s, const char **e) {
    while (*s < *e && (**s == ' ' || **s == '\t' || **s == '\r')) (*s)++;
    while (*e > *s) {
        char c = *(*e - 1);
        if (c == ' ' || c == '\t' || c == '\r') (*e)--;
        else break;
    }
}

static void apply_env(void) {
    char path[4096];
    u64 rl = slen(g_root);
    const char *sfx = "/.onelf/env";
    u64 sl = slen(sfx);
    if (rl + sl + 1 > sizeof(path)) return;
    for (u64 i = 0; i < rl; i++) path[i] = g_root[i];
    for (u64 i = 0; i <= sl; i++) path[rl + i] = sfx[i];

    i64 n = read_file(path, g_buf, sizeof(g_buf));
    if (n <= 0) return;

    const char *cur = g_buf;
    const char *end = g_buf + n;
    while (cur < end) {
        const char *ls = cur;
        const char *le = cur;
        while (le < end && *le != '\n') le++;
        cur = (le < end) ? le + 1 : end;

        const char *s = ls, *e = le;
        trim(&s, &e);
        if (s >= e || *s == '#') continue;

        /* Split at the first '='. */
        const char *eq = s;
        while (eq < e && *eq != '=') eq++;
        if (eq >= e) continue;

        const char *ks = s, *ke = eq;
        trim(&ks, &ke);
        if (ks >= ke) continue;
        const char *vs = eq + 1, *ve = e;
        trim(&vs, &ve);

        char key[1024];
        if (scopy(key, sizeof(key), ks, (u64)(ke - ks)) < 0) continue;
        char val[8192];
        if (!expand(val, sizeof(val), vs, (u64)(ve - vs))) continue;

        setenv(key, val, 1);
    }
}

static void apply_preload(void) {
    char path[4096];
    u64 rl = slen(g_root);
    const char *sfx = "/.onelf/preload";
    u64 sl = slen(sfx);
    if (rl + sl + 1 > sizeof(path)) return;
    for (u64 i = 0; i < rl; i++) path[i] = g_root[i];
    for (u64 i = 0; i <= sl; i++) path[rl + i] = sfx[i];

    i64 n = read_file(path, g_buf, sizeof(g_buf));
    if (n <= 0) return;

    const char *cur = g_buf;
    const char *end = g_buf + n;
    while (cur < end) {
        const char *ls = cur;
        const char *le = cur;
        while (le < end && *le != '\n') le++;
        cur = (le < end) ? le + 1 : end;

        const char *s = ls, *e = le;
        trim(&s, &e);
        if (s >= e || *s == '#') continue;

        char lib[8192];
        if (!expand(lib, sizeof(lib), s, (u64)(e - s))) continue;
        dlopen(lib, RTLD_NOW | RTLD_GLOBAL);
    }
}

static void onelf_env_init(void) {
    if (!find_root()) return;
    apply_env();
    apply_preload();
}

/* Register the constructor in .init_array; the dynamic loader runs it
 * before main() every time this object is loaded. */
__attribute__((used, section(".init_array")))
static void (*const onelf_env_ctor)(void) = onelf_env_init;
