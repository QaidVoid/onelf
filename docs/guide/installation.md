# Installation

## From pre-built releases

Each release on the [releases page](https://github.com/QaidVoid/onelf/releases)
carries a static `onelf` binary for `x86_64`, `aarch64` and `i686`, each
with a `.b3sum` file holding its BLAKE3 hash. Download the one for your
machine, make it executable, and put it on your `$PATH`.

```bash
curl -LO https://github.com/QaidVoid/onelf/releases/latest/download/onelf-x86_64-linux
chmod +x onelf-x86_64-linux
sudo mv onelf-x86_64-linux /usr/local/bin/onelf
```

The binary is a static musl build, so it runs on any Linux distribution,
glibc or musl.

## From source

onelf is written in Rust and builds with a current stable toolchain. The
`onelf` binary embeds the runtime that every packed file starts with,
and the build compiles that runtime as a static musl binary. Beyond Rust
it needs:

- a musl C compiler (`musl-gcc`, from the `musl-tools` package on Debian
  and Ubuntu),
- `llvm-tools` (`rustup component add llvm-tools`),
- a host C compiler (`cc`).

[Building and Testing onelf](./building-onelf) explains each of these and
the escape hatches for a machine without them.

```bash
git clone https://github.com/QaidVoid/onelf
cd onelf
cargo build --release --bin onelf
```

The binary ends up at `target/release/onelf`. Copy it somewhere on `$PATH`.

### Nix flake

The repo ships a flake with a ready dev shell:

```bash
nix develop
cargo build --release --bin onelf
```

## Verifying your install

```bash
onelf --version
onelf --help
```

`onelf --help` lists the subcommands: `pack`, `init`, `build`, `run`,
`verify`, `sysroot`, `info`, `list`, `extract`, `icon`, `desktop`,
`integrate`, `unintegrate`, `key`, `sign`, `cache` and `bundle-libs`.
Next, follow the [Quick Start](./quick-start).
