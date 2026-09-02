---
layout: home

hero:
  name: onelf
  text: One file. Any Linux.
  tagline: Pack an application and every library it needs into a single executable. It runs from a private mount, takes GPU drivers from the host on purpose, and leaves nothing behind.
  actions:
    - theme: brand
      text: Quick Start
      link: /guide/quick-start
    - theme: alt
      text: How onelf Thinks
      link: /guide/concepts
    - theme: alt
      text: GitHub
      link: https://github.com/QaidVoid/onelf

features:
  - icon: run
    title: Runs invisibly
    details: A private user and mount namespace plus FUSE. No mount shows up on the host, no helper is required, and the kernel tears everything down when the last process exits.
  - icon: host
    title: Decides one library at a time
    details: GPU drivers must come from the host, and they bring their own libstdc++ and glibc. At launch, each library present on both sides is chosen by its symbol versions, never by directory order.
  - icon: sysroot
    title: Knows what it ships
    details: Fill a bundle from a pinned sysroot's package database instead of scanning your machine. Plugins, schemas and data arrive because a package owns them, and every bundle records its provenance.
  - icon: libc
    title: Its own libc, anywhere
    details: A bundle carries glibc and the loader, so it runs on musl hosts and on distributions older or newer than the one it was built on.
  - icon: update
    title: Delta updates, signed
    details: zsync-based self-update with an Ed25519 signature checked before a single byte is replaced. Or leave the updater out and let a package manager do it.
  - icon: recipe
    title: A recipe, not a ritual
    details: One onelf.toml captures the whole build. Same input, same bytes, on any machine, with SOURCE_DATE_EPOCH doing the rest.
---
