
## [0.2.2](https://github.com/QaidVoid/onelf/compare/0.2.1...0.2.2) - 2026-04-17

### 🐛 Bug Fixes

- *(bundle)* Add aarch64 and armhf multiarch paths to lib search - ([4f8178e](https://github.com/QaidVoid/onelf/commit/4f8178e1498c1cc3d74f1c854ade6b2061644dfa))
- *(env)* Merge host EGL vendor dirs alongside bundled ones - ([2ef5bea](https://github.com/QaidVoid/onelf/commit/2ef5beac72b3033d61b73476cd1fe1c7d8008eb7))
- *(env)* Merge host + bundled Vulkan ICD paths instead of replacing - ([79776d4](https://github.com/QaidVoid/onelf/commit/79776d47abbde8b590d8eb48f1361a05592e1d34))

## [0.2.1](https://github.com/QaidVoid/onelf/compare/0.2.0...0.2.1) - 2026-04-17

### ⛰️  Features

- *(env)* Auto-set XKB_CONFIG_ROOT when share/X11/xkb is bundled - ([7f1bfee](https://github.com/QaidVoid/onelf/commit/7f1bfee2deaaa0b654b54c9219e0a425cb41c58f))
- *(recipe)* Expand ${VAR} env vars across all recipe fields - ([5438870](https://github.com/QaidVoid/onelf/commit/54388706dbaec4be0c6dd9434dd202cbfba6801a))
- *(rt)* Rewrite PT_INTERP to absolute path at cache extraction - ([6c52e10](https://github.com/QaidVoid/onelf/commit/6c52e10e96cdbc589e9b2bba29c8f2beffa0c1df))
- *(rt)* Add ONELF_FUSE_NO_NAMESPACE env var - ([efa86a4](https://github.com/QaidVoid/onelf/commit/efa86a45ca2ef611f182c465650fe0f8254d74f7))

### 🐛 Bug Fixes

- *(bundle)* Strip absolute DT_NEEDED paths and extend RUNPATH depth - ([46a6ead](https://github.com/QaidVoid/onelf/commit/46a6ead505ccf266cbe0a8cf21f94620634bd226))

## [0.2.0](https://github.com/QaidVoid/onelf/compare/0.1.2...0.2.0) - 2026-04-16

### ⛰️  Features

- *(bundle)* Scrub baked-in /nix/store zoneinfo and locale paths - ([835a6cf](https://github.com/QaidVoid/onelf/commit/835a6cf20bc6e42339d9984cfb5be2e2aae1c733))
- *(bundle)* Scan binary strings for dlopen'd framework sonames - ([4dd9b97](https://github.com/QaidVoid/onelf/commit/4dd9b97a2c130c18f90f306eab4424076f67d47e))
- *(bundle)* Patch PT_INTERP at bundle-libs time for correct /proc/self/exe - ([7ed8ac4](https://github.com/QaidVoid/onelf/commit/7ed8ac4b5a91d6b011d4fcf2961dcc3dd341ecb2))
- *(bundle)* User-extensible dlopen allow-list via --dlopen flag and recipe key - ([e61d780](https://github.com/QaidVoid/onelf/commit/e61d780e5288ebab71ea6677c52ca327fe9b7421))
- *(bundle)* Auto-enable gl/dri/vulkan/wayland/gtk from DT_NEEDED - ([9a858e0](https://github.com/QaidVoid/onelf/commit/9a858e067a3dd753849ed741e3e478b281a41196))
- *(bundle)* Add --from-binary to scaffold AppDir from a single binary - ([95d3cf0](https://github.com/QaidVoid/onelf/commit/95d3cf0437be54decab03a0951b408ce7ea7bbb0))
- *(bundle)* Add --scan-dlopen to detect common dlopen'd libs from binary strings - ([f4bbe96](https://github.com/QaidVoid/onelf/commit/f4bbe96e448ce1ecd79002a885bef9182a0521f2))
- *(bundle)* Add --strict-libc to skip libs with mismatched libc family - ([802062f](https://github.com/QaidVoid/onelf/commit/802062f153f96cd917f6bed9bdcc2e1f3423f2f7))
- *(bundle)* Detect libc family mismatch and skip wrong-family libc deps - ([3778a08](https://github.com/QaidVoid/onelf/commit/3778a0838f9a6e441a16d111b2393b7d85c6b680))
- *(bundle)* Auto-create ld-musl symlink for musl PT_INTERP - ([9a6e1cc](https://github.com/QaidVoid/onelf/commit/9a6e1cc7d917c4f61add6028f08eacc3e0185152))
- *(cli)* Add 'onelf run' to exec an AppDir in place for dev iteration - ([bcffa4d](https://github.com/QaidVoid/onelf/commit/bcffa4dce2678709f5413de7476887c63e43ca20))
- *(cli)* Add 'onelf verify' to check packed binary integrity - ([fb8b27a](https://github.com/QaidVoid/onelf/commit/fb8b27a664f31f60b70cbaec008d1f48da931f1a))
- *(cli)* Add 'onelf init' to scaffold a starter onelf.toml - ([dda441e](https://github.com/QaidVoid/onelf/commit/dda441e6c0ada074540f8ab6494d7a3844269b04))
- *(cli)* Add onelf.toml recipe and 'onelf build' subcommand - ([6c2ddd9](https://github.com/QaidVoid/onelf/commit/6c2ddd9aac71a4d6bc673275c3ec84573feb03ca))
- *(cli)* Default --lib-dir to auto for pack - ([fe0e598](https://github.com/QaidVoid/onelf/commit/fe0e598610c8dd37bbfb52046711b3f875a84abd))
- *(env)* Discover host GPU driver paths for CUDA, OptiX, Vulkan - ([44b099a](https://github.com/QaidVoid/onelf/commit/44b099ada584c323b4c883e926d0cb78490e1fa2))
- *(pack)* Embed package metadata (version, description, license) - ([35e2270](https://github.com/QaidVoid/onelf/commit/35e2270a9f24c7480ae53e198bd1f254233415a6))
- *(pack)* Auto-enable memfd for static-linked entrypoints - ([29908ca](https://github.com/QaidVoid/onelf/commit/29908ca9205d0ce48bb6a41e37ebcd1b5f7b6140))
- *(pack)* Honor SOURCE_DATE_EPOCH for reproducible packaging - ([346b131](https://github.com/QaidVoid/onelf/commit/346b13159bd73000534a852c186bcac2dfc2ea88))
- *(rt)* Gate self-update behind 'update' feature; pick at pack time - ([7939d1c](https://github.com/QaidVoid/onelf/commit/7939d1cd4652b7998349927b2b44f801e5405450))
- *(rt)* Sweep stale onelf-* mountpoint dirs on startup - ([4a6b803](https://github.com/QaidVoid/onelf/commit/4a6b8031967c1882ad34694bbdcf70cda05a5e7b))
- *(rt)* Self-update via zsync with --onelf-update/--onelf-check-update - ([aeeea1f](https://github.com/QaidVoid/onelf/commit/aeeea1f3b8d76a12bec92466e85b600fd3468e84))
- *(rt)* Add ephemeral tmpfs fallback before persistent cache - ([b0680b4](https://github.com/QaidVoid/onelf/commit/b0680b4fa4c87aaa1b424d616885e1d90d3d7db3))
- *(rt)* Mount FUSE via user+mount namespace; drop fusermount3 dependency - ([ff2968b](https://github.com/QaidVoid/onelf/commit/ff2968b3b72df292dde6344b77c483b95bbacd31))
- Add userland-execve for bundled interpreter - ([6977832](https://github.com/QaidVoid/onelf/commit/6977832cb62bc51bbcc09f964ba014bd616d7e67))

### 🐛 Bug Fixes

- *(bundle)* Set RUNPATH to $ORIGIN/../lib on bundled ELFs - ([4038a6a](https://github.com/QaidVoid/onelf/commit/4038a6aad9d13ae624efdaa98992325129913535))
- *(bundle)* Make PT_INTERP always relative to AppDir root - ([a9f02c4](https://github.com/QaidVoid/onelf/commit/a9f02c49f82ba312de6dcb7bbcba13bbd650b3f5))
- *(bundle)* Skip redundant libc aliases from transitive deps - ([bc5c660](https://github.com/QaidVoid/onelf/commit/bc5c66015f3452e455e4a554abd0f0ca23b8f351))
- *(bundle)* Prioritize --search-path over system/nix store scans - ([01aa327](https://github.com/QaidVoid/onelf/commit/01aa32732fed291f74814ee18da8ecf9824ee574))
- *(pack)* Do not auto-enable memfd for non-ELF entrypoints (shell scripts) - ([45d36d2](https://github.com/QaidVoid/onelf/commit/45d36d278f0f760907e716e52229fd55d8c1e28f))
- *(rt)* Skip LD_LIBRARY_PATH when entrypoint is a script - ([b277013](https://github.com/QaidVoid/onelf/commit/b2770135a2f391f40cb7e2f5ce445dacfdef4f02))
- *(rt)* Skip userland-execve for non-PIE binaries (avoid panic) - ([e5013de](https://github.com/QaidVoid/onelf/commit/e5013de884bbfb9352edbc9d60aa3f5cf0dbf759))
- Run packages correctly on NixOS stub-ld systems - ([6faee69](https://github.com/QaidVoid/onelf/commit/6faee698dd3017168ba311c1e13983c114b80484))
- Match bundled interpreter against symlinks too - ([ab1a814](https://github.com/QaidVoid/onelf/commit/ab1a8146e67422358bd5771f6d7e094877a1c16e))

### 🚜 Refactor

- Remove PT_INTERP patching and /tmp/.oi symlinks - ([07685c8](https://github.com/QaidVoid/onelf/commit/07685c853bac6d7a9fc6fb2f42f512dea91597e8))

## [0.1.2](https://github.com/QaidVoid/onelf/compare/0.1.1...0.1.2) - 2026-03-09

### 🐛 Bug Fixes

- Always use bundled interpreter to match bundled libc - ([8c91234](https://github.com/QaidVoid/onelf/commit/8c91234d83260dda0ab44eca8ed3397f7a6f0c56))

## [0.1.1](https://github.com/QaidVoid/onelf/compare/0.1.0...0.1.1) - 2026-03-08

### 🐛 Bug Fixes

- Resolve aarch64 rt build - ([08b7f00](https://github.com/QaidVoid/onelf/commit/08b7f004a7629b393d227747d8579f5c6919ee6b))

## [0.1.0] - 2026-03-08

### ⛰️  Features

- Add nix flake devshell and fix musl cross-compilation - ([491d89f](https://github.com/QaidVoid/onelf/commit/491d89f79b4f0849f74bb9776712cd7a72fb03a0))
- Add --gtk flag to bundle GSettings schemas and set XDG_DATA_DIRS - ([4eb8fd4](https://github.com/QaidVoid/onelf/commit/4eb8fd492fb9e6dff8248514f5eec577a9d6efa0))
- Add cross-libc interpreter support and GPU driver bundling - ([5c449ef](https://github.com/QaidVoid/onelf/commit/5c449ef4fe88e3276d1a1b057a83135979c142dd))
- Add icon and desktop file extraction from packed binaries - ([a5a7e76](https://github.com/QaidVoid/onelf/commit/a5a7e76aa9bd3a1e178b4c72a6c7b7e4037177ab))
- Add build script to compile onelf-rt for musl - ([8a4f2b4](https://github.com/QaidVoid/onelf/commit/8a4f2b46687f72727022cd477c671798819232df))
- Add bundle-libs command - ([ac5afd8](https://github.com/QaidVoid/onelf/commit/ac5afd89bdd583dc10e7d964478cee550e86ee66))
- Add info, list, extract commands - ([5c51b41](https://github.com/QaidVoid/onelf/commit/5c51b41b6ea654803fd85747a91a0e8ee7bc34ff))
- Add pack command basics - ([fc28cfa](https://github.com/QaidVoid/onelf/commit/fc28cfa2339c9f7b543633eb4112b32f133bd275))
- Implement directory scanning and compression - ([3e99558](https://github.com/QaidVoid/onelf/commit/3e995585ca1880fe7163b049236793bf3362f42f))
- Add zstd compression wrapper - ([8ddb3bb](https://github.com/QaidVoid/onelf/commit/8ddb3bb03838941835d4a3055fe88b3a8f187cfa))
- Scaffold project - ([dc106fd](https://github.com/QaidVoid/onelf/commit/dc106fdec8e450ee8a20ae85eef9afdd3e6a02f9))
- Add entry and entrypoint types - ([05dee9c](https://github.com/QaidVoid/onelf/commit/05dee9c2ed1d027791c7f332bb7a67e05e967c1d))
- Implement manifest and footer structures - ([5b688a3](https://github.com/QaidVoid/onelf/commit/5b688a3ac3747ac5ed9fac033ff14d520264e220))
- Add portable directory and env file support to runtime - ([3b1a486](https://github.com/QaidVoid/onelf/commit/3b1a4864215e5c5109a5472cdbf81671ffa8ee60))
- Make FUSE the default execution mode - ([483d634](https://github.com/QaidVoid/onelf/commit/483d634ad82546a696ee87d4270fc946fd878a1e))
- Implement FUSE mount and execution - ([4a27181](https://github.com/QaidVoid/onelf/commit/4a2718144bde019888d483ba4094e5bdfc0c52ab))
- Add FUSE filesystem implementation - ([c5b639f](https://github.com/QaidVoid/onelf/commit/c5b639f4651d20e83a7bd81c395809bbfe2a3a18))
- Add memfd execution mode - ([759632c](https://github.com/QaidVoid/onelf/commit/759632c74e1ea51183466c546ef920b538bca46d))
- Implement cache execution mode - ([9c92316](https://github.com/QaidVoid/onelf/commit/9c92316f443e0c315ec2c0c93424129fcc7f24f9))
- Add package loading and cache extraction - ([11dc6f3](https://github.com/QaidVoid/onelf/commit/11dc6f3c47b34773af868a2b3e6d9b453fcfca65))

### 🐛 Bug Fixes

- Don't skip hidden files - ([0169b5d](https://github.com/QaidVoid/onelf/commit/0169b5d34efdffbdb8f354464626bf82fc3743b4))
