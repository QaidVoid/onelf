/*
 * onelf relative-interpreter bootstrap for aarch64.
 * Same logic as bootstrap_x86_64.c, different syscall ABI.
 *
 * Build: aarch64-linux-gnu-gcc -nostdlib -static -fPIC -ffreestanding -O2 \
 *        -fno-stack-protector -fno-asynchronous-unwind-tables -c \
 *        -o bootstrap_aarch64.o bootstrap_aarch64.c
 *        aarch64-linux-gnu-ld -nostdlib --oformat binary -e _onelf_start \
 *        -T payload.ld trampoline_aarch64.o bootstrap_aarch64.o \
 *        -o bootstrap_aarch64.bin
 */

typedef unsigned long  u64;
typedef unsigned int   u32;
typedef unsigned short u16;
typedef unsigned char  u8;
typedef long           i64;

#define NULL ((void*)0)

#define AT_NULL    0
#define AT_PHDR    3
#define AT_PHNUM   5
#define AT_PAGESZ  6
#define AT_BASE    7
#define AT_ENTRY   9
#define AT_EXECFN  31

#define O_RDONLY   0
#define AT_FDCWD   -100
#define PROT_READ  1
#define PROT_WRITE 2
#define PROT_EXEC  4
#define MAP_PRIVATE    0x02
#define MAP_FIXED      0x10
#define MAP_ANONYMOUS  0x20

#define PT_LOAD    1
#define PT_DYNAMIC 2
#define PT_INTERP  3
#define PT_PHDR    6
#define PF_X 1
#define PF_W 2
#define PF_R 4

#define PAGE_SIZE 4096

typedef struct { u64 tag; u64 val; } auxval_t;

typedef struct {
    u8  e_ident[16];
    u16 e_type, e_machine;
    u32 e_version;
    u64 e_entry, e_phoff, e_shoff;
    u32 e_flags;
    u16 e_ehsize, e_phentsize, e_phnum, e_shentsize, e_shnum, e_shstrndx;
} Elf64_Ehdr;

typedef struct {
    u32 p_type, p_flags;
    u64 p_offset, p_vaddr, p_paddr, p_filesz, p_memsz, p_align;
} Elf64_Phdr;

typedef struct {
    i64 entry_delta;
    u16 rel_path_len;
    char rel_path[];
} __attribute__((packed)) metadata_t;

/* aarch64 syscall ABI: number in x8, args in x0-x5, return in x0 */
static inline i64 sys_openat(int dfd, const char *path, int flags) {
    register i64 x0 __asm__("x0") = dfd;
    register const char *x1 __asm__("x1") = path;
    register i64 x2 __asm__("x2") = flags;
    register i64 x8 __asm__("x8") = 56; /* __NR_openat */
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x1), "r"(x2), "r"(x8) : "memory");
    return x0;
}

static inline i64 sys_read(int fd, void *buf, u64 count) {
    register i64 x0 __asm__("x0") = fd;
    register void *x1 __asm__("x1") = buf;
    register u64 x2 __asm__("x2") = count;
    register i64 x8 __asm__("x8") = 63; /* __NR_read */
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x1), "r"(x2), "r"(x8) : "memory");
    return x0;
}

static inline i64 sys_pread(int fd, void *buf, u64 count, u64 offset) {
    register i64 x0 __asm__("x0") = fd;
    register void *x1 __asm__("x1") = buf;
    register u64 x2 __asm__("x2") = count;
    register u64 x3 __asm__("x3") = offset;
    register i64 x8 __asm__("x8") = 67; /* __NR_pread64 */
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x1), "r"(x2), "r"(x3), "r"(x8) : "memory");
    return x0;
}

static inline i64 sys_mmap(u64 addr, u64 len, int prot, int flags,
                           int fd, u64 off) {
    register u64 x0 __asm__("x0") = addr;
    register u64 x1 __asm__("x1") = len;
    register i64 x2 __asm__("x2") = prot;
    register i64 x3 __asm__("x3") = flags;
    register i64 x4 __asm__("x4") = fd;
    register u64 x5 __asm__("x5") = off;
    register i64 x8 __asm__("x8") = 222; /* __NR_mmap */
    __asm__ volatile("svc #0" : "+r"(x0)
        : "r"(x1), "r"(x2), "r"(x3), "r"(x4), "r"(x5), "r"(x8) : "memory");
    return (i64)x0;
}

static inline i64 sys_close(int fd) {
    register i64 x0 __asm__("x0") = fd;
    register i64 x8 __asm__("x8") = 57; /* __NR_close */
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x8) : "memory");
    return x0;
}

static inline void sys_write(int fd, const void *buf, u64 count) {
    register i64 x0 __asm__("x0") = fd;
    register const void *x1 __asm__("x1") = buf;
    register u64 x2 __asm__("x2") = count;
    register i64 x8 __asm__("x8") = 64; /* __NR_write */
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x1), "r"(x2), "r"(x8) : "memory");
}

static inline i64 sys_readlinkat(int dfd, const char *path, char *buf, u64 bufsiz) {
    register i64 x0 __asm__("x0") = dfd;
    register const char *x1 __asm__("x1") = path;
    register char *x2 __asm__("x2") = buf;
    register u64 x3 __asm__("x3") = bufsiz;
    register i64 x8 __asm__("x8") = 78; /* __NR_readlinkat */
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x1), "r"(x2), "r"(x3), "r"(x8) : "memory");
    return x0;
}

static void die(const char *msg) {
    u64 n = 0; while (msg[n]) n++;
    sys_write(2, msg, n);
    register i64 x0 __asm__("x0") = 127;
    register i64 x8 __asm__("x8") = 94; /* __NR_exit_group */
    __asm__ volatile("svc #0" :: "r"(x0), "r"(x8));
    __builtin_unreachable();
}

static void mcpy(void *d, const void *s, u64 n) {
    u8 *dd = d; const u8 *ss = s; while (n--) *dd++ = *ss++;
}

static void mset(void *d, u8 v, u64 n) {
    u8 *dd = d; while (n--) *dd++ = v;
}

static int pflags(u32 f) {
    return ((f&PF_R)?PROT_READ:0)|((f&PF_W)?PROT_WRITE:0)|((f&PF_X)?PROT_EXEC:0);
}

static u64 map_interp(int fd, const Elf64_Ehdr *ehdr, u64 pmask) {
    u64 phsz = ehdr->e_phnum * sizeof(Elf64_Phdr);
    Elf64_Phdr ph[32];
    if (ehdr->e_phnum > 32) die("onelf: interp too many phdrs\n");
    if (sys_pread(fd, ph, phsz, ehdr->e_phoff) != (i64)phsz)
        die("onelf: read interp phdrs\n");

    u64 vmax = 0;
    for (u16 i = 0; i < ehdr->e_phnum; i++)
        if (ph[i].p_type == PT_LOAD) {
            u64 v = ph[i].p_vaddr + ph[i].p_memsz;
            if (v > vmax) vmax = v;
        }

    u64 base = 0;
    int fixed = 0;
    for (u16 i = 0; i < ehdr->e_phnum; i++) {
        if (ph[i].p_type != PT_LOAD) continue;
        u64 off = ph[i].p_offset;
        u64 mis = off & pmask;
        u64 va  = base + ph[i].p_vaddr - mis;
        int pr  = pflags(ph[i].p_flags);
        off -= mis;

        if (!fixed) {
            base -= va;
            va = (u64)sys_mmap(0, vmax - va, pr, MAP_PRIVATE, fd, off);
            if ((i64)va < 0) die("onelf: mmap interp\n");
            base += va;
            fixed = 1;
        } else if (ph[i].p_filesz) {
            va = (u64)sys_mmap(va, ph[i].p_filesz + mis, pr,
                               MAP_PRIVATE | MAP_FIXED, fd, off);
            if ((i64)va < 0) die("onelf: mmap interp seg\n");
        }

        if (ph[i].p_memsz <= ph[i].p_filesz) continue;
        u64 fe = va + mis + ph[i].p_filesz;
        u64 pe = (fe + pmask) & ~pmask;
        u64 me = va + mis + ph[i].p_memsz;
        if (pe < me) {
            u64 r = (u64)sys_mmap(pe, me - pe, pr,
                                  MAP_PRIVATE | MAP_FIXED | MAP_ANONYMOUS, -1, 0);
            if ((i64)r < 0) die("onelf: mmap bss\n");
            me = pe;
        }
        if (pr & PROT_WRITE) mset((void *)fe, 0, me - fe);
    }
    return base;
}

u64 _onelf_bootstrap(u64 *stack, const metadata_t *meta) {
    u32 argc = *(u32 *)stack;
    const char **envp = (const char **)stack + argc + 2;
    while (*envp) envp++;
    envp++;

    /* Index auxv entries (tags 0-31). */
    u64 *auxv[32];
    u32 seen = 0;
    for (auxval_t *a = (auxval_t *)envp; ; a++) {
        u64 t = a->tag;
        if (t <= 31) { seen |= 1u << t; auxv[t] = &a->val; }
        if (t == AT_NULL) break;
    }

    const char *execfn = (seen & (1u << AT_EXECFN))
        ? (const char *)*auxv[AT_EXECFN] : NULL;
    if (!execfn) die("onelf: no AT_EXECFN\n");

    /* If AT_EXECFN is a /proc path (e.g., /proc/self/exe from a
     * re-exec), resolve it via readlink so we get the real binary
     * path. */
    char resolved[4096];
    if (execfn[0] == '/' && execfn[1] == 'p' && execfn[2] == 'r'
        && execfn[3] == 'o' && execfn[4] == 'c' && execfn[5] == '/') {
        i64 n = sys_readlinkat(AT_FDCWD, execfn, resolved, sizeof(resolved) - 1);
        if (n > 0) {
            resolved[n] = '\0';
            execfn = resolved;
        }
    }

    /* Dirname of execfn. */
    u64 dlen = 0;
    for (u64 i = 0; execfn[i]; i++)
        if (execfn[i] == '/') dlen = i + 1;

    u64 plen = dlen + meta->rel_path_len;

    /* Allocate: copied phdrs (n+1) + interp path string. */
    u32 nph = (seen & (1u << AT_PHNUM)) ? (u32)*auxv[AT_PHNUM] : 0;
    u64 alloc = (nph + 1) * sizeof(Elf64_Phdr) + plen + 1;
    u64 buf = (u64)sys_mmap(0, alloc, PROT_READ | PROT_WRITE,
                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if ((i64)buf < 0) die("onelf: alloc\n");

    /* Copy phdrs, patch PT_PHDR. */
    Elf64_Phdr *nw = (Elf64_Phdr *)buf;
    u64 baddr = 0;
    if (nph && (seen & (1u << AT_PHDR))) {
        Elf64_Phdr *old = (Elf64_Phdr *)*auxv[AT_PHDR];
        mcpy(nw, old, nph * sizeof(Elf64_Phdr));
        for (u32 i = 0; i < nph; i++) {
            if (nw[i].p_type == PT_PHDR) {
                baddr = (u64)old - nw[i].p_vaddr;
                nw[i].p_vaddr = buf - baddr;
                nw[i].p_paddr = nw[i].p_vaddr;
                nw[i].p_filesz = (nph + 1) * sizeof(Elf64_Phdr);
                nw[i].p_memsz = nw[i].p_filesz;
            }
        }
    }

    /* Form interp path after the phdrs. */
    char *ipath = (char *)(nw + nph + 1);
    mcpy(ipath, execfn, dlen);
    mcpy(ipath + dlen, meta->rel_path, meta->rel_path_len);
    ipath[plen] = '\0';

    /* Append PT_INTERP. */
    Elf64_Phdr *iph = &nw[nph];
    mset(iph, 0, sizeof(Elf64_Phdr));
    iph->p_type = PT_INTERP;
    iph->p_vaddr = (u64)ipath - baddr;
    iph->p_filesz = plen + 1;
    iph->p_memsz = plen + 1;
    iph->p_flags = PF_R;

    /* Patch auxv. */
    if (seen & (1u << AT_PHDR))  *auxv[AT_PHDR] = buf;
    if (seen & (1u << AT_PHNUM)) *auxv[AT_PHNUM] = nph + 1;
    if (seen & (1u << AT_ENTRY)) *auxv[AT_ENTRY] += meta->entry_delta;

    /* Load interpreter. */
    i64 fd = sys_openat(AT_FDCWD, ipath, O_RDONLY);
    if (fd < 0) die("onelf: open interp\n");
    Elf64_Ehdr ehdr;
    if (sys_read((int)fd, &ehdr, sizeof(ehdr)) != sizeof(ehdr))
        die("onelf: read interp\n");
    if (*(u32 *)ehdr.e_ident != 0x464c457f)
        die("onelf: not ELF\n");

    u64 pmask = (seen & (1u << AT_PAGESZ)) ? *auxv[AT_PAGESZ] - 1 : PAGE_SIZE - 1;
    u64 ibase = map_interp((int)fd, &ehdr, pmask);

    if (seen & (1u << AT_BASE)) *auxv[AT_BASE] = ibase;
    sys_close((int)fd);

    return ibase + ehdr.e_entry;
}
