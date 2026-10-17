# Cross-libc Packages

Linux has two major libc families: **glibc** (Debian, Fedora, Arch, most
distros) and **musl** (Alpine, Void-musl, static builds). Binaries built
against one cannot run on the other without bundled pieces, because:

1. The dynamic linker path differs. A musl ELF has
   `PT_INTERP = /lib/ld-musl-x86_64.so.1`. A glibc host doesn't have that
   file.
2. Libraries linked against one libc can't satisfy the other's symbols.
   Glibc's fortify `__printf_chk` doesn't exist in musl, for example.

The rules this page encodes:

- ✅ Let onelf handle both concerns. A musl binary runs on a glibc host
  and vice versa without any host-level setup.
- ❌ Wrong-family libraries in a bundle. Their symbols can't satisfy the
  other libc. `--strict-libc` refuses them.
- ✅ A musl app on a glibc host needs musl-built libraries on the
  `--search-path`; the host's `libdrm`, `libgcc_s` and `libpthread` are
  glibc-linked and unusable here.
- ⚠️ A wrong-libc warning after `bundle-libs` means the bundle may not
  work at runtime. Find a musl-built replacement.
- ❌ Expecting glibc-specific APIs (like `obstack_*`) to run on musl.

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

`bundle-libs` injects an AT_EXECFN bootstrap into each bundled
executable. The bootstrap reads `AT_EXECFN` at startup, computes the
bundled loader's path relative to the binary's own location, maps it
into memory, and jumps into it. The host's loader is used only when the
launch resolver picks the host's newer glibc for a glibc package (see
[Bundling Libraries](./bundling#libraries-that-come-from-the-host)),
never across libc families, so a musl binary runs on a glibc host and
vice versa without any host-level setup.

For PIE (`ET_DYN`) binaries the runtime can also use `userland-execve`
to map the bundled loader directly. Non-PIE (`ET_EXEC`) binaries go
through the kernel exec path with the bootstrap doing the loader
hand-off.

- ❌ Rewriting `PT_INTERP` to a relative path, which earlier versions of
  onelf did. It broke when the new path didn't fit in the original slot.
- ✅ The bootstrap stores the relative path as appended metadata, so
  there is no path-length constraint; deeply nested binaries work the
  same as shallow ones.

## Packaging musl apps on glibc hosts

The problem: the host's `libdrm`, `libgcc_s`, `libpthread`, etc. are all
glibc-linked, so bundle-libs can't use them for a musl app.

- ❌ The host's copies. Wrong family, and with `--strict-libc` they are
  refused outright.
- ✅ Musl-built versions on the `--search-path`: from Nix, or from the
  musl sysroot you built the app against.

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

✅ Export the env vars in your CI or `direnv`. `onelf build` does the rest.

## Verifying the bundle is clean

After `bundle-libs`, watch for warnings:

```
warning: libfoo.so.1 links against Glibc libc but target is Musl; this bundle may not work at runtime
```

With `--strict-libc` those are skipped and listed under "Not found":

```
Not found (1)
  libfoo.so.1 (needed by bin/app (libfoo.so.1 links against Glibc libc but target is Musl))
```

⚠️ Either way, the warning means the bundle may not work at runtime. It
is your signal to put a musl-built replacement on the `--search-path`.

## Limitations

- ❌ A binary that directly uses glibc-specific APIs (like `obstack_*`)
  won't magically work on musl. We can't cross-libc arbitrary system
  calls.
- ⚠️ Name service modules. The host's `nsswitch.conf` is read, and a
  service it names that the bundle lacks (`systemd`, `sss`, `resolve`) is
  skipped, so a user from LDAP or SSSD is not visible and hostnames go
  through `files` and `dns` as they would without those services. A
  bundled glibc of 2.34 or newer has `files` and `dns` built in; for an
  older one `bundle-libs` brings those two modules along. `compat`, which
  some distributions name instead of `files`, is a module on every glibc
  and is always brought along. Anything else stays out.
- ☑️ Before glibc 2.34, `dlopen` lives in `libdl.so.2`. A bundle of such
  a glibc applies `.onelf/env` as usual, but `.onelf/preload` only when
  something in the bundle links `libdl.so.2`; otherwise it says so on
  stderr and goes on.
