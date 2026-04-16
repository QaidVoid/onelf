# Cross-libc Packages

Linux has two major libc families: **glibc** (Debian, Fedora, Arch, most
distros) and **musl** (Alpine, Void-musl, static builds). Binaries built
against one cannot run on the other without bundled pieces, because:

1. The dynamic linker path differs. A musl ELF has
   `PT_INTERP = /lib/ld-musl-x86_64.so.1`. A glibc host doesn't have that
   file.
2. Libraries linked against one libc can't satisfy the other's symbols.
   Glibc's fortify `__printf_chk` doesn't exist in musl, for example.

onelf handles both concerns automatically.

## What happens at pack time

When bundle-libs sees a musl binary (PT_INTERP matches `ld-musl-*`), it:

1. Bundles the musl libc under both names it might be called: the actual
   filename (`libc.musl-x86_64.so.1`) and a symlink (`ld-musl-x86_64.so.1`)
   matching the `PT_INTERP`.
2. Skips glibc libc / loader entries that would show up transitively (e.g.
   from `libgcc_s.so.1`). Those are wrong-family and would break things.
3. With `--strict-libc`, refuses to bundle other libraries that link
   against the wrong libc at all.

## What happens at runtime

`bundle-libs` rewrites each ELF's `PT_INTERP` to a path relative to the
AppDir root (like `lib/ld-musl-x86_64.so.1` or `lib/ld-linux-x86-64.so.2`).
At exec time the runtime chdirs into the AppDir and the kernel resolves
the relative PT_INTERP against the bundled loader. The host's own loader
is never consulted, so a musl binary runs on a glibc host and vice versa
without any host-level setup.

For PIE (`ET_DYN`) binaries the runtime can also use `userland-execve`
to map the bundled loader directly and skip the kernel loader entirely.
Non-PIE (`ET_EXEC`) binaries always go through the kernel + patched
PT_INTERP path.

When the replacement PT_INTERP overflows the original slot (e.g. a
deeply nested binary like `5.0/python/bin/python3.11` needing
`../../../lib/ld-linux-x86-64.so.2`), bundle-libs appends the new string
to the end of the file and rewrites the PT_INTERP program header to
point at it. Kernel reads PT_INTERP by file offset, so the string does
not need PT_LOAD coverage.

## Packaging musl apps on glibc hosts

The host's `libdrm`, `libgcc_s`, `libpthread`, etc. are all glibc-linked,
so bundle-libs can't use them directly. You need musl-built versions.

### With Nix

```bash
export MUSL_LIBDRM=$(nix build nixpkgs#pkgsCross.musl64.libdrm.out --no-link --print-out-paths)/lib
export MUSL_GCC=$(nix eval --raw nixpkgs#pkgsCross.musl64.stdenv.cc.cc.lib)/x86_64-unknown-linux-musl/lib

onelf bundle-libs ./app --strict-libc \
  --search-path "$MUSL_LIBDRM" \
  --search-path "$MUSL_GCC"
```

### With a musl toolchain

If you built the app against a musl sysroot, point at that:

```bash
onelf bundle-libs ./app --strict-libc \
  --search-path $MUSL_SYSROOT/usr/lib \
  --search-path $MUSL_SYSROOT/lib
```

### Recipe

```toml
[bundle]
search-paths = ["${MUSL_LIBDRM}", "${MUSL_GCC}"]
strict-libc = true
```

Export the env vars in your CI or `direnv` and `onelf build` does the rest.

## Verifying the bundle is clean

After `bundle-libs`, watch for warnings:

```
warning: libfoo.so.1 links against Glibc libc but target is Musl;
         this bundle may not work at runtime
```

With `--strict-libc` those are skipped and listed under "Not found":

```
Not found (1)
  libfoo.so.1 (needed by bin/app (libfoo.so.1 links against Glibc libc but target is Musl))
```

Either way, the warning is your signal that you need a musl-built
replacement on the `--search-path`.

## Limitations

- We can't cross-libc arbitrary system calls. A binary that directly uses
  glibc-specific APIs (like `obstack_*`) won't magically work on musl.
- nss / nsswitch libraries can be fiddly. If your app does DNS or user
  lookups via dlopen'd nsswitch modules, you may need to bundle them too.
