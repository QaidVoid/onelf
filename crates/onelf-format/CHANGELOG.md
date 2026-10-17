
## [0.3.4](https://github.com/QaidVoid/onelf/compare/onelf-format-v0.3.3...onelf-format-v0.3.4) - 2026-10-17

### ⛰️  Features

- *(format)* Encode host-libs policy and cache request in flags - ([58e9347](https://github.com/QaidVoid/onelf/commit/58e934764020bd261d21844bbe81fe7fd51a8727))
- *(integrate)* Add desktop actions and install every icon size - ([6327365](https://github.com/QaidVoid/onelf/commit/632736501317225e158f64bcaad4741e78efed8d))
- *(onelf)* Carry the GL build pin into packages - ([275697b](https://github.com/QaidVoid/onelf/commit/275697b5a88fda4fbefb168517ba8758772eebf9))
- *(resolver)* Report the GL situation and index a fetched build - ([f177ff2](https://github.com/QaidVoid/onelf/commit/f177ff2b6a2a165fd30ba6c6c9a21652253d8aec))
- *(rt)* Let a package extract, mount and diagnose itself - ([1672e9a](https://github.com/QaidVoid/onelf/commit/1672e9acfabed7d9eefdff4bd741d462ee9ebb7a))
- *(rt)* Reach host and set libraries without LD_LIBRARY_PATH - ([7d275e7](https://github.com/QaidVoid/onelf/commit/7d275e734d224bc0266051d717a7e60e38de037d))
- *(rt)* Fetch and use the dependency sets a package pins - ([73503b5](https://github.com/QaidVoid/onelf/commit/73503b5c9118c2b801e5c1b7aa132f11d4fd2709))
- *(rt)* Choose host libraries per soname at launch - ([29856c6](https://github.com/QaidVoid/onelf/commit/29856c6c827f5598d5da8199b1d4cdc48dee1196))
- *(run)* Resolve host libraries like the packed runtime - ([f2a9573](https://github.com/QaidVoid/onelf/commit/f2a95735792375fccd8aa3791344dbd07d52210c))
- *(update)* Sign updates and authenticate packages (format 2) - ([a5803a3](https://github.com/QaidVoid/onelf/commit/a5803a3905c9f5dedbeee558359a05cf4277d8d7))
- Point plugin, data and locale lookups at the bundle - ([e4f122d](https://github.com/QaidVoid/onelf/commit/e4f122d13c21190aaea70d0576180f11624cf2f3))

### 🐛 Bug Fixes

- *(build)* Validate the toolchain and end a skip() recursion - ([f4cf395](https://github.com/QaidVoid/onelf/commit/f4cf39561c9db28547c1943b9fd20c630ced6e03))
- *(format)* Require manifest lib dirs to stay inside the package - ([fc8f021](https://github.com/QaidVoid/onelf/commit/fc8f021b6c918b76c591b696780ce6ff94625139))
- *(format)* Seed the host walk from ICDs naming a bare soname - ([850ace7](https://github.com/QaidVoid/onelf/commit/850ace7dffd836dec2a83ca1656ef20bda9a451a))
- *(format)* Key the link farm by host fingerprint - ([4ef749c](https://github.com/QaidVoid/onelf/commit/4ef749c6cf6c4c058459855e6f819d0dbdd57238))
- *(format)* Read only this ABI's entries from ld.so.cache - ([516bc41](https://github.com/QaidVoid/onelf/commit/516bc4177802d5a55f3f1dba0b3b7cb6bd0db035))
- *(format)* Reject a manifest whose parent does not precede it - ([66fb969](https://github.com/QaidVoid/onelf/commit/66fb9696fff18f93080a0de1440ef6d488b0bb28))
- *(format)* Put the VA-API display wrappers on the platform line - ([d40d8a7](https://github.com/QaidVoid/onelf/commit/d40d8a7bfe51371a11f9faae15081b938e58d894))
- *(integrate)* Quote Exec paths in both integrators - ([259e09b](https://github.com/QaidVoid/onelf/commit/259e09be6896170e9d89d4331cc800d6e3f54eda))
- *(rt)* Probe user namespaces and launch chosen entrypoints - ([8f98189](https://github.com/QaidVoid/onelf/commit/8f981890cffbd9839892dabb45d8d4e3733625f4))
- *(rt)* Isolate rundir trees, mounts and inherited state - ([28f1787](https://github.com/QaidVoid/onelf/commit/28f1787161b6a9a68a9b691af39048b9435dba1d))
- *(rt)* Fetch the GL build when the host stack is partial - ([8f71321](https://github.com/QaidVoid/onelf/commit/8f71321c043a724cbb9569ef3327e799e70162bf))
- *(rt)* Validate the footer with the shared reader checks - ([81b69b9](https://github.com/QaidVoid/onelf/commit/81b69b9aab753c77086b794bb106f4aee1e77c38))
- *(rt)* Keep a GL build off the search path, refetch a moved pin - ([ea6faac](https://github.com/QaidVoid/onelf/commit/ea6faacbb84b7c97e316ecdbe09c25ab13715ac9))
- Guard update keys, manifests, verify and desktop files - ([69bd353](https://github.com/QaidVoid/onelf/commit/69bd353688b893712c4b17c9483bd599aa09ad35))
- Re-judge the resolver, desktop patching and FUSE access - ([be59c04](https://github.com/QaidVoid/onelf/commit/be59c04f25c1539abb4f549ad5be64553a92fb08))

### 🚜 Refactor

- *(format)* Drop the unused sharun compatibility flag - ([7a0929e](https://github.com/QaidVoid/onelf/commit/7a0929e56e70e30afcd2ee1e5a607c9bf6711429))

### ⚡ Performance

- Default to 64 KiB payload blocks - ([6b08951](https://github.com/QaidVoid/onelf/commit/6b089510be493d2c540f24156b349993f4220083))

## [0.3.3](https://github.com/QaidVoid/onelf/compare/onelf-format-v0.3.2...onelf-format-v0.3.3) - 2026-08-23

### ⛰️  Features

- *(format)* Flag packages updated from outside - ([68d64ef](https://github.com/QaidVoid/onelf/commit/68d64ef071b842bbff5bb5bca2402ab14be8e716))
- *(format)* Verify payload blocks individually - ([a3cc7b7](https://github.com/QaidVoid/onelf/commit/a3cc7b799c76421f11199822a46304eea4c84a4d))
- *(rt)* Make the host library dirs opt-out - ([bc2e0bc](https://github.com/QaidVoid/onelf/commit/bc2e0bcb363802f51973eb40d1d5b1d5a70ac78e))

### 🐛 Bug Fixes

- *(rt)* Resolve host libs through the host ld.so.cache - ([2405865](https://github.com/QaidVoid/onelf/commit/2405865ae9459e3c0729a84ac3fdd3de7d00628b))
- *(rt)* Claim mountpoints and reclaim the cache under locks - ([96b67cd](https://github.com/QaidVoid/onelf/commit/96b67cd70bf22b180893563ca2ab46b374606189))
- Refuse memfd when the entrypoint needs bundled libs - ([8f921cb](https://github.com/QaidVoid/onelf/commit/8f921cbab7ec07f05a7cb2db33927d63d3bd553e))
- Validate package regions before allocating from them - ([22a3b68](https://github.com/QaidVoid/onelf/commit/22a3b68bee109acb42c6a27110bb73d7b095879b))
- Honour entrypoint intent and stop forcing nested modes - ([89948d9](https://github.com/QaidVoid/onelf/commit/89948d9ea4bc437ba032ed7433bcef7ded918787))

### 🚜 Refactor

- *(format)* Share the detached signature URL rule - ([a0a8e79](https://github.com/QaidVoid/onelf/commit/a0a8e79735a9218a0bd9b1c2ff3a835d6858f5cf))

### ⚙️ Miscellaneous Tasks

- Add ci gate and clear lint and comment debt - ([17d4c42](https://github.com/QaidVoid/onelf/commit/17d4c424578eef621a8e205a54c99ca82b985613))



## [0.3.0](https://github.com/QaidVoid/onelf/compare/onelf-format-v0.2.8...onelf-format-v0.3.0) - 2026-07-26

### 🐛 Bug Fixes

- Verify payload hashes and validate extraction paths ([#21](https://github.com/QaidVoid/onelf/pull/21)) - ([d4a2930](https://github.com/QaidVoid/onelf/commit/d4a2930684699aaedce5d98d49fe14d3ed8d825a))

### 🚜 Refactor

- Doc hygiene, internal dedup, remove onelf-preload ([#26](https://github.com/QaidVoid/onelf/pull/26)) - ([7b64f8e](https://github.com/QaidVoid/onelf/commit/7b64f8eb2c81ac5afeb2c207c60f9cd9dcd93d0f))



## [0.2.6](https://github.com/QaidVoid/onelf/compare/onelf-format-v0.2.5...onelf-format-v0.2.6) - 2026-05-19

### ⛰️  Features

- Add store mode for uncompressed payloads - ([7da4dd1](https://github.com/QaidVoid/onelf/commit/7da4dd1d9a05eaac96f24d33773b00c87837a0f4))
## [0.1.0] - 2026-03-08

### ⛰️  Features

- Implement directory scanning and compression - ([3e99558](https://github.com/QaidVoid/onelf/commit/3e995585ca1880fe7163b049236793bf3362f42f))
- Add entry and entrypoint types - ([05dee9c](https://github.com/QaidVoid/onelf/commit/05dee9c2ed1d027791c7f332bb7a67e05e967c1d))
- Implement manifest and footer structures - ([5b688a3](https://github.com/QaidVoid/onelf/commit/5b688a3ac3747ac5ed9fac033ff14d520264e220))
- Scaffold project - ([dc106fd](https://github.com/QaidVoid/onelf/commit/dc106fdec8e450ee8a20ae85eef9afdd3e6a02f9))
