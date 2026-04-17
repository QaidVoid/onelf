
## [0.2.1](https://github.com/QaidVoid/onelf/compare/onelf-rt-v0.2.0...onelf-rt-v0.2.1) - 2026-04-17

### ⛰️  Features

- *(env)* Auto-set XKB_CONFIG_ROOT when share/X11/xkb is bundled - ([7f1bfee](https://github.com/QaidVoid/onelf/commit/7f1bfee2deaaa0b654b54c9219e0a425cb41c58f))
- *(rt)* Add ONELF_FUSE_NO_NAMESPACE env var - ([efa86a4](https://github.com/QaidVoid/onelf/commit/efa86a45ca2ef611f182c465650fe0f8254d74f7))
- *(rt)* Rewrite PT_INTERP to absolute path at cache extraction - ([6c52e10](https://github.com/QaidVoid/onelf/commit/6c52e10e96cdbc589e9b2bba29c8f2beffa0c1df))

## [0.2.0](https://github.com/QaidVoid/onelf/compare/onelf-rt-v0.1.2...onelf-rt-v0.2.0) - 2026-04-16

### ⛰️  Features

- *(bundle)* Patch PT_INTERP at bundle-libs time for correct /proc/self/exe - ([7ed8ac4](https://github.com/QaidVoid/onelf/commit/7ed8ac4b5a91d6b011d4fcf2961dcc3dd341ecb2))
- *(env)* Discover host GPU driver paths for CUDA, OptiX, Vulkan - ([44b099a](https://github.com/QaidVoid/onelf/commit/44b099ada584c323b4c883e926d0cb78490e1fa2))
- *(rt)* Sweep stale onelf-* mountpoint dirs on startup - ([4a6b803](https://github.com/QaidVoid/onelf/commit/4a6b8031967c1882ad34694bbdcf70cda05a5e7b))
- *(rt)* Gate self-update behind 'update' feature; pick at pack time - ([7939d1c](https://github.com/QaidVoid/onelf/commit/7939d1cd4652b7998349927b2b44f801e5405450))
- *(rt)* Self-update via zsync with --onelf-update/--onelf-check-update - ([aeeea1f](https://github.com/QaidVoid/onelf/commit/aeeea1f3b8d76a12bec92466e85b600fd3468e84))
- *(rt)* Add ephemeral tmpfs fallback before persistent cache - ([b0680b4](https://github.com/QaidVoid/onelf/commit/b0680b4fa4c87aaa1b424d616885e1d90d3d7db3))
- *(rt)* Mount FUSE via user+mount namespace; drop fusermount3 dependency - ([ff2968b](https://github.com/QaidVoid/onelf/commit/ff2968b3b72df292dde6344b77c483b95bbacd31))
- Add userland-execve for bundled interpreter - ([6977832](https://github.com/QaidVoid/onelf/commit/6977832cb62bc51bbcc09f964ba014bd616d7e67))

### 🐛 Bug Fixes

- *(bundle)* Make PT_INTERP always relative to AppDir root - ([a9f02c4](https://github.com/QaidVoid/onelf/commit/a9f02c49f82ba312de6dcb7bbcba13bbd650b3f5))
- *(rt)* Skip LD_LIBRARY_PATH when entrypoint is a script - ([b277013](https://github.com/QaidVoid/onelf/commit/b2770135a2f391f40cb7e2f5ce445dacfdef4f02))
- *(rt)* Skip userland-execve for non-PIE binaries (avoid panic) - ([e5013de](https://github.com/QaidVoid/onelf/commit/e5013de884bbfb9352edbc9d60aa3f5cf0dbf759))
- Run packages correctly on NixOS stub-ld systems - ([6faee69](https://github.com/QaidVoid/onelf/commit/6faee698dd3017168ba311c1e13983c114b80484))

### 🚜 Refactor

- Remove PT_INTERP patching and /tmp/.oi symlinks - ([07685c8](https://github.com/QaidVoid/onelf/commit/07685c853bac6d7a9fc6fb2f42f512dea91597e8))

## [0.1.2](https://github.com/QaidVoid/onelf/compare/onelf-rt-v0.1.1...onelf-rt-v0.1.2) - 2026-03-09

### 🐛 Bug Fixes

- Always use bundled interpreter to match bundled libc - ([8c91234](https://github.com/QaidVoid/onelf/commit/8c91234d83260dda0ab44eca8ed3397f7a6f0c56))

## [0.1.1](https://github.com/QaidVoid/onelf/compare/onelf-rt-v0.1.0...onelf-rt-v0.1.1) - 2026-03-08

### 🐛 Bug Fixes

- Resolve aarch64 rt build - ([08b7f00](https://github.com/QaidVoid/onelf/commit/08b7f004a7629b393d227747d8579f5c6919ee6b))

## [0.1.0] - 2026-03-08

### ⛰️  Features

- Add --gtk flag to bundle GSettings schemas and set XDG_DATA_DIRS - ([4eb8fd4](https://github.com/QaidVoid/onelf/commit/4eb8fd492fb9e6dff8248514f5eec577a9d6efa0))
- Add cross-libc interpreter support and GPU driver bundling - ([5c449ef](https://github.com/QaidVoid/onelf/commit/5c449ef4fe88e3276d1a1b057a83135979c142dd))
- Add portable directory and env file support to runtime - ([3b1a486](https://github.com/QaidVoid/onelf/commit/3b1a4864215e5c5109a5472cdbf81671ffa8ee60))
- Add icon and desktop file extraction from packed binaries - ([a5a7e76](https://github.com/QaidVoid/onelf/commit/a5a7e76aa9bd3a1e178b4c72a6c7b7e4037177ab))
- Make FUSE the default execution mode - ([483d634](https://github.com/QaidVoid/onelf/commit/483d634ad82546a696ee87d4270fc946fd878a1e))
- Implement FUSE mount and execution - ([4a27181](https://github.com/QaidVoid/onelf/commit/4a2718144bde019888d483ba4094e5bdfc0c52ab))
- Add FUSE filesystem implementation - ([c5b639f](https://github.com/QaidVoid/onelf/commit/c5b639f4651d20e83a7bd81c395809bbfe2a3a18))
- Add memfd execution mode - ([759632c](https://github.com/QaidVoid/onelf/commit/759632c74e1ea51183466c546ef920b538bca46d))
- Implement cache execution mode - ([9c92316](https://github.com/QaidVoid/onelf/commit/9c92316f443e0c315ec2c0c93424129fcc7f24f9))
- Add package loading and cache extraction - ([11dc6f3](https://github.com/QaidVoid/onelf/commit/11dc6f3c47b34773af868a2b3e6d9b453fcfca65))
- Scaffold project - ([dc106fd](https://github.com/QaidVoid/onelf/commit/dc106fdec8e450ee8a20ae85eef9afdd3e6a02f9))
