# Building and Testing onelf

What a machine needs to build onelf from source and run its test suite,
and what the suite does when something is missing.

## Building

```bash
cargo build
```

needs three things beyond a Rust toolchain:

- **A musl cross compiler** (distro package `musl-tools`, providing
  `musl-gcc`). The build compiles the embedded runtime twice as a
  static musl binary. Without it the build fails with instructions; the
  search order is `ONELF_MUSL_CC`, `CC_x86_64_unknown_linux_musl`,
  `<arch>-linux-musl-gcc` and `musl-gcc` on `PATH`, then common Nix and
  Bootlin locations.
- **llvm-tools** (`rustup component add llvm-tools`), which supplies
  the `llvm-objcopy` that flattens the bootstrap payload. `ONELF_OBJCOPY`
  names an alternative.
- **A host C compiler** (`cc`), which links the native architecture's
  payloads.

Escape hatches for toolchain-less builds: `ONELF_RT_PATH` and
`ONELF_RT_UPDATE_PATH` embed prebuilt runtime binaries instead of
compiling them, and `ONELF_PAYLOAD_DIR` embeds prebuilt payloads. All
are listed in [Environment Variables](../reference/env-vars).

## Testing

```bash
cargo test --workspace
```

The suite is in `crates/onelf/tests/pipeline.rs`. Tests that need a
tool the host lacks print `skip: <reason>` and pass; nothing else skips
silently. Three capabilities gate skips:

- **A C compiler** (`cc` or `gcc`) compiles most test fixtures. Dozens
  of tests skip without one.
- **patchelf** (`ONELF_PATCHELF` or on `PATH`) gates the rpath and
  constructor-injection tests. A handful skip; one test instead asserts
  the no-patchelf fallback path works, so it runs either way.
- **A mountable FUSE** gates roughly seventeen tests. Installing the
  `fuse3` package is necessary but not sufficient: the test probe runs
  a package in FUSE mode, which needs either unprivileged user
  namespaces or a working `fusermount3` plus `/dev/fuse`. Three tests
  additionally require the `fusermount3` binary on `PATH`.

Nothing else is required: the suite never invokes `appstreamcli` or
`bash`, and plain `/bin/sh` scripts stand in for applications.

## `CI` makes skips failures

When the `CI` environment variable is set to a non-empty value, every
`skip` panics instead of passing. This is how CI stays honest: the
workflow installs `musl-tools patchelf fuse3`, loads the `fuse` module,
and lowers AppArmor's unprivileged-user namespace restriction, so a
skipped test there means a real regression. Locally, leaving `CI`
unset keeps the suite lenient.
