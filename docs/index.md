---
layout: home

hero:
  name: onelf
  text: Single-binary packaging for Linux
  tagline: Pack any application into one executable file. No fusermount3, no visible temp dirs, runs invisibly.
  actions:
    - theme: brand
      text: Get Started
      link: /guide/quick-start
    - theme: alt
      text: View on GitHub
      link: https://github.com/QaidVoid/onelf

features:
  - title: One file, many tricks
    details: Pack a binary and its libraries into a self-contained executable that runs everywhere a modern Linux kernel exists.
  - title: Invisible by default
    details: Uses a private user+mount namespace plus FUSE so no mount ever shows up in the host, and kernel tears it down when the process exits. No fusermount3 dependency.
  - title: Cross-libc friendly
    details: Run musl binaries on glibc hosts (or vice versa) with the bundled interpreter. No /tmp/.oi symlinks, no LD_PRELOAD trickery.
  - title: Delta self-update
    details: Built-in zsync-based self-update. Just set the update URL at pack time and users run --onelf-update.
  - title: Reproducible
    details: Same input with SOURCE_DATE_EPOCH set gives byte-identical output across machines.
  - title: Recipe-driven
    details: Declarative onelf.toml makes packaging reviewable and shareable. Bundle + pack with one command.
---
