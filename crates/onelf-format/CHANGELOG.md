
## [0.6.0](https://github.com/QaidVoid/onelf/compare/onelf-format-v0.5.1...onelf-format-v0.6.0) - 2026-11-14

### ⛰️  Features

- *(bundle)* Drop shared, libc and driver dirs from inference - ([f6cc1df](https://github.com/QaidVoid/onelf/commit/f6cc1df73481cdf118067179376fe58f6f8e6f79))
- *(catalog)* Carry pipewire client-rt.conf for pre-1.4 clients - ([44e4862](https://github.com/QaidVoid/onelf/commit/44e4862819ff3c5e7436f02faee0c3825a6177dd))
- *(catalog)* Remove sections inference reproduces - ([ea6acde](https://github.com/QaidVoid/onelf/commit/ea6acdefb79a181c42dec39c856de3335f039df0))
- *(doctor)* Name admitted and refused host modules - ([e385728](https://github.com/QaidVoid/onelf/commit/e385728af513b64d917a055f8303ea077b2e2c37))
- *(format)* Add the plugin directory record codec - ([9ce9b5a](https://github.com/QaidVoid/onelf/commit/9ce9b5adaa688e90b48ff76e5011e51a463fa788))
- *(format)* Add the inference exclusions table to the catalog - ([1050694](https://github.com/QaidVoid/onelf/commit/1050694159813a4febc025d3aa5bc713334da522))
- *(format)* Share the glibc loader scrub and count its search dirs - ([d3ab06a](https://github.com/QaidVoid/onelf/commit/d3ab06a50a736cafd5ddddcf24f5ee14005b9356))
- *(format)* Parse the path overlay record strictly and bounded - ([3c2ad07](https://github.com/QaidVoid/onelf/commit/3c2ad07ae815c0a798d4cdab8ff6414d12dedf32))
- *(format)* Add canonical selection identity and storage - ([d8c2e2e](https://github.com/QaidVoid/onelf/commit/d8c2e2e7e6e6712847c0abeb33ba73aa1508e1dc))
- *(format)* Derive lint and runtime env knowledge - ([1dd4525](https://github.com/QaidVoid/onelf/commit/1dd4525693465a92fd59142ec142fd853abc9afd))
- *(format)* Add runtime capability note and gate - ([aff116c](https://github.com/QaidVoid/onelf/commit/aff116c2b76a8a9917b69e2ec356295e5537c05d))
- *(format)* Add bounded ELF fact inspection - ([2355f11](https://github.com/QaidVoid/onelf/commit/2355f11d495ecf326fea3ed6b99c5056db6e134b))
- *(format)* Add built-in rule catalog and metadata codecs - ([e0e0625](https://github.com/QaidVoid/onelf/commit/e0e06255115f14ede6f66231a099bb30fa94e3a7))
- *(format)* Add validated framework descriptor schemas - ([5b43504](https://github.com/QaidVoid/onelf/commit/5b4350481ad3f9fc674e3d216e3c1e479c10fed4))
- *(pack)* Append inferred pairs to the paths record - ([b923a0d](https://github.com/QaidVoid/onelf/commit/b923a0d9cb8ce50d12ff0039df64ef04b29e7684))
- *(resolve)* Admit checked host counterpart modules - ([feeeeb7](https://github.com/QaidVoid/onelf/commit/feeeeb7ce0bb9e86d006eb725132d8de3b3ac0da))
- *(resolve)* Derive host counterpart directories - ([d34c6f7](https://github.com/QaidVoid/onelf/commit/d34c6f7207b9ac9d0e59cc8485e6e1e78da38747))
- *(rt)* Present admitted host plugin modules as merged views - ([b8d78b0](https://github.com/QaidVoid/onelf/commit/b8d78b011d8f7f07f742f9e91d1c877d1946647e))
- *(rt)* Hold descriptor 77 on the package's anchor directory - ([280fdae](https://github.com/QaidVoid/onelf/commit/280fdaef4efc32c2442c303f939a620bf3361a16))
- *(rt)* Evaluate carried runtime rules and prepared pins - ([23f0af9](https://github.com/QaidVoid/onelf/commit/23f0af93b64fa02460921070d6c2d99e9b6b8e53))
- *(rt)* Export the resolved store through launch env - ([d65b2ee](https://github.com/QaidVoid/onelf/commit/d65b2eef5070ce08a8d013f6da0981253b5ea399))
- *(rules)* Collect only client-side PipeWire and SPA members - ([589925d](https://github.com/QaidVoid/onelf/commit/589925dd99119741a9c140b7003085ac52fdb56d))
- *(rules)* Let plugin groups list the members they collect - ([443ca55](https://github.com/QaidVoid/onelf/commit/443ca5582da0132769a369d6a6e36f34bf40aaee))
- *(sysroot)* Verify provider index for set pins - ([bc0c474](https://github.com/QaidVoid/onelf/commit/bc0c47477087f0398b539e529fd8c614a5798cf8))

### 🐛 Bug Fixes

- *(bundle)* Anchor by path prefix and keep inferred pairs - ([19eefbb](https://github.com/QaidVoid/onelf/commit/19eefbb9568346e7cc92a83483b88ac5e4884fd1))
- *(bundle)* Scrub PowerPC glibc loaders too - ([6fdf366](https://github.com/QaidVoid/onelf/commit/6fdf3662677796c2da0ca2ab801e629d23031be5))
- *(bundle)* Find data for tree-copied rule libraries on the host - ([9360155](https://github.com/QaidVoid/onelf/commit/93601555e520d9a0c422cbfd29c98b7b7c5cfffe))
- *(catalog)* Restore the qt, babl and gegl plugin sections - ([fbfd16b](https://github.com/QaidVoid/onelf/commit/fbfd16b305c18fffce7a8f45d1c7e3e02e7d837e))
- *(cli)* Publish verified sets only and persist root's cache - ([7d08f6e](https://github.com/QaidVoid/onelf/commit/7d08f6e246f9d656fb5074aed15123e71a75d055))
- *(format)* Bind provider indexes to the pin, not the name - ([2dfabd4](https://github.com/QaidVoid/onelf/commit/2dfabd473959c55219d21e2a8d0847436d251901))
- *(format)* Treat malformed onelf notes as incompatible - ([aaf49a3](https://github.com/QaidVoid/onelf/commit/aaf49a3debc31953237299ab117c13b7289aa1a6))
- *(format)* Accept lld version chains and read dynstr once - ([35eec27](https://github.com/QaidVoid/onelf/commit/35eec27274d6c12738e4f1c67894ad7d9e7d494d))
- *(resolve)* Refuse host modules another user could plant - ([980c517](https://github.com/QaidVoid/onelf/commit/980c517c45bd2c25e6845329b147846b4be8e692))
- *(resolve)* Run the host loader from a closed private copy - ([e13a1f5](https://github.com/QaidVoid/onelf/commit/e13a1f5f7fc680dd7d717b2c88ae1439256bcbe1))
- *(rt)* Keep nvidia-settings libraries out of the driver closure - ([67cc8ba](https://github.com/QaidVoid/onelf/commit/67cc8ba3923a2bb7d802c7bb3b8de2134473f01f))
- *(rt)* Scope resolver decisions and provider checks to selections - ([faebd83](https://github.com/QaidVoid/onelf/commit/faebd83a5753c356fd7c7dad2c04ddff7bff9fd3))
- *(rt)* Keep root's cache and base off env-chosen paths - ([5c9227b](https://github.com/QaidVoid/onelf/commit/5c9227b64962ec594c7f0a1fee4abd39093a5425))
- *(rules)* Map translations at runtime without copying locales - ([a947b40](https://github.com/QaidVoid/onelf/commit/a947b40d4d19a38d7922257dae12e674361038a3))
- *(rules)* Refuse loader, runtime-owned and shared env names - ([b656df1](https://github.com/QaidVoid/onelf/commit/b656df16fb2005725ce7ec274fe1b53bffe8a34f))
- *(runtime)* Warn about missing GL only when the package links it - ([2f9904c](https://github.com/QaidVoid/onelf/commit/2f9904c9a97e6042619aa8ce2623d23f8963beca))
- Treat libGLESv1_CM as a glvnd front end like libGLESv2 - ([984290c](https://github.com/QaidVoid/onelf/commit/984290c1b7a8eefee40f5dabf1f1dfeb81909ea4))

### 🚜 Refactor

- Drop stale dead-code allows and unused FUSE structs - ([a11dab9](https://github.com/QaidVoid/onelf/commit/a11dab984bcb31c9618f61aa4efbb816fc062c8b))

### 📚 Documentation

- *(rt)* Note that nvidia-settings itself loads its GTK libraries - ([cacfeb9](https://github.com/QaidVoid/onelf/commit/cacfeb903d9c5bbe1f3247ded0b130fbdb3f5f51))

### 🧪 Testing

- *(format)* Run rules-toml tests and use canonical rule files - ([c17659a](https://github.com/QaidVoid/onelf/commit/c17659a7796d45eddbc471fdd3fda109cd1e5208))


## [0.5.0](https://github.com/QaidVoid/onelf/compare/onelf-format-v0.4.2...onelf-format-v0.5.0) - 2026-10-29

### ⛰️  Features

- [**breaking**] Replace zsync with direct-release delta self-updates ([#41](https://github.com/QaidVoid/onelf/pull/41)) - ([cd99efb](https://github.com/QaidVoid/onelf/commit/cd99efbe9463d0d80b23937968177d0c980843e0))
- Integrate every entrypoint that has a menu entry - ([08a39bd](https://github.com/QaidVoid/onelf/commit/08a39bded09cd39816368b40ec5070d1dcff8208))

### 🐛 Bug Fixes

- *(rt)* Farm what the host's DRI driver modules need - ([1f12826](https://github.com/QaidVoid/onelf/commit/1f12826072f5c6feef8a91f19454860aabaf1169))
- *(rt)* Farm the host's GLX vendor libraries - ([1db8d66](https://github.com/QaidVoid/onelf/commit/1db8d663d5823db0a03bbf50ba94ae40f90717fc))

## [0.4.2](https://github.com/QaidVoid/onelf/compare/onelf-format-v0.4.1...onelf-format-v0.4.2) - 2026-10-21

### ⛰️  Features

- Add a stipplefs payload with --payload stipple - ([1512eca](https://github.com/QaidVoid/onelf/commit/1512eca9d7ee6c3558b7476ab81e0b37a7ad6333))
- Add --prefer-cache to run from the persistent cache first - ([20f0e6c](https://github.com/QaidVoid/onelf/commit/20f0e6caad37ca33dcc86d37c9b5f4fa5c69fa86))


## [0.4.0](https://github.com/QaidVoid/onelf/compare/onelf-format-v0.3.3...onelf-format-v0.4.0) - 2026-10-17

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
