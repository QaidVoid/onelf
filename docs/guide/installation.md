# Installation

## From source

onelf is written in Rust. The packer CLI builds on any host, and it embeds a
pre-built musl-static runtime for the packed binaries.

### Prerequisites

- Rust (nightly currently, stable soon)
- `musl-gcc` or a cross-toolchain producing `x86_64-unknown-linux-musl` and
  `aarch64-unknown-linux-musl` binaries
- On NixOS: `nix develop` in the repo sets up everything

### Build

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

## From pre-built releases

Download from the [releases page](https://github.com/QaidVoid/onelf/releases),
make the binary executable, and put it on your `$PATH`.

```bash
curl -LO https://github.com/QaidVoid/onelf/releases/latest/download/onelf-x86_64-linux
chmod +x onelf-x86_64-linux
sudo mv onelf-x86_64-linux /usr/local/bin/onelf
```

## Verifying your install

```bash
onelf --version
onelf --help
```

You should see the list of subcommands: `init`, `build`, `pack`, `run`,
`bundle-libs`, `info`, `list`, `extract`, `verify`, `icon`, `desktop`, `cache`.
