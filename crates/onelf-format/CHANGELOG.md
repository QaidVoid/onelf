
## [0.3.4](https://github.com/QaidVoid/onelf/compare/onelf-format-v0.3.3...onelf-format-v0.3.4) - 2026-09-02

### ⛰️  Features

- *(format)* Encode host-libs policy and cache request in flags - ([58e9347](https://github.com/QaidVoid/onelf/commit/58e934764020bd261d21844bbe81fe7fd51a8727))
- *(onelf)* Carry the GL build pin into packages - ([275697b](https://github.com/QaidVoid/onelf/commit/275697b5a88fda4fbefb168517ba8758772eebf9))
- *(resolver)* Report the GL situation and index a fetched build - ([f177ff2](https://github.com/QaidVoid/onelf/commit/f177ff2b6a2a165fd30ba6c6c9a21652253d8aec))
- *(rt)* Choose host libraries per soname at launch - ([29856c6](https://github.com/QaidVoid/onelf/commit/29856c6c827f5598d5da8199b1d4cdc48dee1196))
- *(run)* Resolve host libraries like the packed runtime - ([f2a9573](https://github.com/QaidVoid/onelf/commit/f2a95735792375fccd8aa3791344dbd07d52210c))

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
