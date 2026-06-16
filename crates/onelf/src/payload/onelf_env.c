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
static char g_self[4096];

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
            scopy(g_self, sizeof(g_self), so, so_len);
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
                scopy(g_self, sizeof(g_self), so, so_len);
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

/* ---- exec-family interposition ------------------------------------- */
/*
 * Re-establish the bundle library path for every child the application
 * execs, env-independently, so onelf needs no baked rpath. For a bundled
 * ELF target we re-inject LD_LIBRARY_PATH (from .onelf/libpath) and
 * LD_PRELOAD (this object) and then perform the real exec unchanged, so the
 * kernel loads the target via its own PT_INTERP and /proc/self/exe stays
 * correct. For a host binary we strip LD_LIBRARY_PATH / LD_PRELOAD / ONELF_*
 * so the bundled libc never leaks. These symbols interpose libc's because
 * this object is the entrypoint's first DT_NEEDED, and the interposer
 * survives clearenv() because it self-locates the package root from
 * /proc/self/maps rather than the environment.
 */

extern void *dlsym(void *handle, const char *name);
extern int *__errno_location(void);
extern char **environ;
#define RTLD_NEXT ((void *)-1L)
#define ONELF_AT_FDCWD (-100)

#if defined(__x86_64__)
#define NR_EXECVE 59
#define NR_GETCWD 79
#define NR_READLINKAT 267
static inline i64 sys_execve(const char *p, char *const *a, char *const *e) {
    return sys3(NR_EXECVE, (i64)p, (i64)a, (i64)e);
}
static inline i64 sys_getcwd(char *b, u64 n) { return sys3(NR_GETCWD, (i64)b, (i64)n, 0); }
static inline i64 sys_readlinkat(const char *p, char *b, u64 n) {
    return sys4(NR_READLINKAT, ONELF_AT_FDCWD, (i64)p, (i64)b, (i64)n);
}
#elif defined(__aarch64__)
#define NR_EXECVE 221
#define NR_GETCWD 17
#define NR_READLINKAT 78
static inline i64 sys_execve(const char *p, char *const *a, char *const *e) {
    return svc4(NR_EXECVE, (i64)p, (i64)a, (i64)e, 0);
}
static inline i64 sys_getcwd(char *b, u64 n) { return svc4(NR_GETCWD, (i64)b, (i64)n, 0, 0); }
static inline i64 sys_readlinkat(const char *p, char *b, u64 n) {
    return svc4(NR_READLINKAT, ONELF_AT_FDCWD, (i64)p, (i64)b, (i64)n);
}
#endif

static int ensure_root(void) {
    if (g_root[0] == 0) find_root();
    return g_root[0] != 0;
}

static int sstarts(const char *s, const char *pfx) {
    while (*pfx) {
        if (*s != *pfx) return 0;
        s++;
        pfx++;
    }
    return 1;
}

/* Real exec via raw syscall (our exported execve interposes libc's, so a
 * normal call would recurse). Only returns on failure. */
static int real_execve(const char *p, char *const *a, char *const *e) {
    i64 r = sys_execve(p, a, e);
    *__errno_location() = (int)(-r);
    return -1;
}

static int head4(const char *path, u8 *b) {
    i64 fd = sys_openat(path);
    if (fd < 0) return 0;
    i64 r = sys_read((int)fd, b, 4);
    sys_close((int)fd);
    return r >= 4;
}

static int is_elf_file(const char *path) {
    u8 b[4];
    return head4(path, b) && b[0] == 0x7f && b[1] == 'E' && b[2] == 'L' && b[3] == 'F';
}

static int abspath(const char *path, char *out, u64 cap) {
    if (path[0] == '/') return scopy(out, cap, path, slen(path)) >= 0;
    char cwd[4096];
    if (sys_getcwd(cwd, sizeof(cwd)) < 0) return 0;
    u64 cl = slen(cwd), pl = slen(path);
    if (cl + 1 + pl + 1 > cap) return 0;
    for (u64 i = 0; i < cl; i++) out[i] = cwd[i];
    out[cl] = '/';
    for (u64 i = 0; i <= pl; i++) out[cl + 1 + i] = path[i];
    return 1;
}

/* Resolve one symlink level so /proc/self/exe (and similar) classify by
 * their real location, not the symlink path. */
static void resolve_target(const char *path, char *out, u64 cap) {
    char link[4096];
    i64 n = sys_readlinkat(path, link, sizeof(link) - 1);
    if (n > 0) {
        link[n] = 0;
        if (link[0] == '/') {
            scopy(out, cap, link, (u64)n);
            return;
        }
        /* relative link target: resolve against the directory of `path`. */
        u64 pl = slen(path);
        while (pl > 0 && path[pl - 1] != '/') pl--;
        if (pl + (u64)n + 1 <= cap) {
            for (u64 i = 0; i < pl; i++) out[i] = path[i];
            for (u64 i = 0; i <= (u64)n; i++) out[pl + i] = link[i];
            return;
        }
    }
    scopy(out, cap, path, slen(path));
}

static int is_bundled(const char *path) {
    char resolved[4096];
    resolve_target(path, resolved, sizeof(resolved));
    char ab[4096];
    if (!abspath(resolved, ab, sizeof(ab))) return 0;
    if (!sstarts(ab, g_root)) return 0;
    u64 rl = slen(g_root);
    return ab[rl] == '/' || ab[rl] == 0;
}

/* Join g_root + "/" + suffix into out. */
static int root_join(const char *suffix, char *out, u64 cap) {
    u64 rl = slen(g_root), sl = slen(suffix);
    if (rl + 1 + sl + 1 > cap) return 0;
    u64 o = 0;
    for (u64 i = 0; i < rl; i++) out[o++] = g_root[i];
    out[o++] = '/';
    for (u64 i = 0; i <= sl; i++) out[o + i] = suffix[i];
    return 1;
}

/* Colon-joined absolute library path from .onelf/libpath (one rel dir per
 * line). Empty string if the file is absent. */
static void build_libpath(char *out, u64 cap) {
    out[0] = 0;
    char p[4096];
    if (!root_join(".onelf/libpath", p, sizeof(p))) return;
    char buf[8192];
    i64 n = read_file(p, buf, sizeof(buf));
    if (n <= 0) return;
    u64 o = 0;
    u64 rl = slen(g_root);
    const char *cur = buf, *end = buf + n;
    while (cur < end) {
        const char *ls = cur, *le = cur;
        while (le < end && *le != '\n') le++;
        cur = (le < end) ? le + 1 : end;
        const char *s = ls, *e = le;
        trim(&s, &e);
        if (s >= e) continue;
        u64 entry = rl + 1 + (u64)(e - s);
        if (o + (o ? 1 : 0) + entry + 1 > cap) break;
        if (o) out[o++] = ':';
        for (u64 i = 0; i < rl; i++) out[o++] = g_root[i];
        out[o++] = '/';
        for (const char *q = s; q < e; q++) out[o++] = *q;
    }
    out[o] = 0;
}

/* Build the child environment. Bundled: drop inherited LD_LIBRARY_PATH /
 * LD_PRELOAD and append the bundle's. Host: also drop ONELF_*. */
static int build_env(char *const *envp, int bundled, char **nenv, int cap,
                     char *ld_entry, const char *lib, char *pre_entry) {
    int have_lib = lib && lib[0];
    int ne = 0;
    for (char *const *e = envp; e && *e; e++) {
        const char *kv = *e;
        int is_lib = sstarts(kv, "LD_LIBRARY_PATH=");
        int is_pre = sstarts(kv, "LD_PRELOAD=");
        int drop;
        if (bundled) {
            /* Always re-add LD_PRELOAD; replace LD_LIBRARY_PATH only when we
             * have a derived path, else keep the inherited one as a fallback. */
            drop = is_pre || (is_lib && have_lib);
        } else {
            drop = is_pre || is_lib || sstarts(kv, "ONELF_");
        }
        if (drop) continue;
        if (ne < cap - 3) nenv[ne++] = (char *)kv;
    }
    if (bundled) {
        if (lib && lib[0]) {
            const char *k = "LD_LIBRARY_PATH=";
            u64 o = 0;
            while (k[o]) { ld_entry[o] = k[o]; o++; }
            u64 j = 0;
            while (lib[j]) ld_entry[o++] = lib[j++];
            ld_entry[o] = 0;
            nenv[ne++] = ld_entry;
        }
        const char *k2 = "LD_PRELOAD=";
        u64 o = 0;
        while (k2[o]) { pre_entry[o] = k2[o]; o++; }
        u64 j = 0;
        while (g_self[j]) pre_entry[o++] = g_self[j++];
        pre_entry[o] = 0;
        nenv[ne++] = pre_entry;
    }
    nenv[ne] = 0;
    return ne;
}

/* Resolve `file` against PATH (from envp, then getenv) into out. */
static int path_search(const char *file, char *const *envp, char *out, u64 cap) {
    for (const char *s = file; *s; s++)
        if (*s == '/') return scopy(out, cap, file, slen(file)) >= 0;
    const char *path = 0;
    for (char *const *e = envp; e && *e; e++)
        if (sstarts(*e, "PATH=")) { path = *e + 5; break; }
    if (!path) path = getenv("PATH");
    if (!path) path = "/usr/bin:/bin";
    u64 fl = slen(file);
    const char *p = path;
    while (*p) {
        const char *q = p;
        while (*q && *q != ':') q++;
        u64 dl = (u64)(q - p);
        u64 o = 0;
        if (dl == 0) {
            if (1 + 1 + fl + 1 > cap) goto next;
            out[o++] = '.';
        } else {
            if (dl + 1 + fl + 1 > cap) goto next;
            for (u64 i = 0; i < dl; i++) out[o++] = p[i];
        }
        out[o++] = '/';
        for (u64 i = 0; i <= fl; i++) out[o + i] = file[i];
        {
            i64 fd = sys_openat(out);
            if (fd >= 0) { sys_close((int)fd); return 1; }
        }
    next:
        p = (*q == ':') ? q + 1 : q;
    }
    return 0;
}

/* Core routing shared by the exec*() wrappers. Re-inject (bundled) or strip
 * (host) the bundle env, then exec the target unchanged. Only returns on
 * failure. */
static int route_exec(const char *path, char *const *argv, char *const *envp) {
    if (!ensure_root() || !path || !argv || !is_elf_file(path))
        return real_execve(path, argv, envp);

    int bundled = is_bundled(path);

    char ld_entry[16400];
    char pre_entry[4200];
    char lib[16384];
    build_libpath(lib, sizeof(lib));

    char *nenv[2048];
    build_env(envp, bundled, nenv, 2048, ld_entry, lib, pre_entry);

    return real_execve(path, argv, nenv);
}

int execve(const char *path, char *const argv[], char *const envp[]) {
    return route_exec(path, argv, envp);
}

int execv(const char *path, char *const argv[]) {
    return route_exec(path, argv, environ);
}

int execvpe(const char *file, char *const argv[], char *const envp[]) {
    if (!file) return real_execve(file, argv, envp);
    char resolved[4096];
    if (!path_search(file, envp, resolved, sizeof(resolved))) {
        *__errno_location() = 2; /* ENOENT */
        return -1;
    }
    return route_exec(resolved, argv, envp);
}

int execvp(const char *file, char *const argv[]) {
    return execvpe(file, argv, environ);
}

typedef int (*pspawn_fn)(int *, const char *, const void *, const void *,
                         char *const *, char *const *);

static int route_spawn(int is_p, int *pid, const char *path, const void *fa,
                       const void *attr, char *const argv[], char *const envp[],
                       pspawn_fn real) {
    if (!ensure_root() || !path || !argv) return real(pid, path, fa, attr, argv, envp);

    char target[4096];
    if (is_p) {
        if (!path_search(path, envp, target, sizeof(target)))
            return real(pid, path, fa, attr, argv, envp);
    } else {
        if (scopy(target, sizeof(target), path, slen(path)) < 0)
            return real(pid, path, fa, attr, argv, envp);
    }

    if (!is_elf_file(target)) return real(pid, path, fa, attr, argv, envp);

    int bundled = is_bundled(target);

    char ld_entry[16400];
    char pre_entry[4200];
    char lib[16384];
    build_libpath(lib, sizeof(lib));
    char *nenv[2048];
    build_env(envp, bundled, nenv, 2048, ld_entry, lib, pre_entry);

    /* Spawn the original path (kernel loads via its PT_INTERP); only the
     * environment is adjusted. */
    return real(pid, path, fa, attr, argv, nenv);
}

static pspawn_fn g_real_spawn, g_real_spawnp;

int posix_spawn(int *pid, const char *path, const void *fa, const void *attr,
                char *const argv[], char *const envp[]) {
    if (!g_real_spawn) g_real_spawn = (pspawn_fn)dlsym(RTLD_NEXT, "posix_spawn");
    if (!g_real_spawn) { *__errno_location() = 38; return -1; } /* ENOSYS */
    return route_spawn(0, pid, path, fa, attr, argv, envp, g_real_spawn);
}

int posix_spawnp(int *pid, const char *file, const void *fa, const void *attr,
                 char *const argv[], char *const envp[]) {
    if (!g_real_spawnp) g_real_spawnp = (pspawn_fn)dlsym(RTLD_NEXT, "posix_spawnp");
    if (!g_real_spawnp) { *__errno_location() = 38; return -1; }
    return route_spawn(1, pid, file, fa, attr, argv, envp, g_real_spawnp);
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
