
## [0.4.0](https://github.com/QaidVoid/onelf/compare/onelf-sysroot-v0.3.3...onelf-sysroot-v0.4.0) - 2026-10-17

### ⛰️  Features

- *(sysroot)* Keep globs, whole Python packages, host packages - ([6defc19](https://github.com/QaidVoid/onelf/commit/6defc19b25a62cca99fe8a806511f836a7b93959))
- *(sysroot)* Read the GL build pin from platform.toml - ([7808703](https://github.com/QaidVoid/onelf/commit/7808703fe24bd50576bc75088d0071092b49d239))
- *(sysroot)* Read pacman databases, closures, archives and prune - ([0906bd8](https://github.com/QaidVoid/onelf/commit/0906bd8ff478d2cc1d00de2dc1179fa04a28ff1b))

### 🐛 Bug Fixes

- *(sysroot)* Confine reads to the sysroot and stop on gaps - ([395ed2b](https://github.com/QaidVoid/onelf/commit/395ed2b0cb0f84fd075a7ea95303998abb4e4250))
- *(sysroot)* Keep a stripped setuid file readable - ([dd76d8f](https://github.com/QaidVoid/onelf/commit/dd76d8fe3f0efd449e43a15d8c2d9ce593c4a2a0))
- *(sysroot)* Apply directory modes after unpacking - ([612f060](https://github.com/QaidVoid/onelf/commit/612f060bc799147d354c77d4d814f5ed0cdb74c7))
- *(sysroot)* Make the trace tier prune flat directories - ([a044fe7](https://github.com/QaidVoid/onelf/commit/a044fe73922f42f244e9b5c1f541835185f48ea7))
- *(sysroot)* Seed named packages and read ld.so.conf - ([07ec294](https://github.com/QaidVoid/onelf/commit/07ec2949fc953ecb5086f31e9c31478dafe491dc))
- Harden pack reads and the FUSE server - ([47dc21f](https://github.com/QaidVoid/onelf/commit/47dc21f5748b9803a93b552e5820063ebf470415))

### 🧪 Testing

- *(sysroot)* Remove temp roots when a test ends - ([1f100fe](https://github.com/QaidVoid/onelf/commit/1f100fe357a25082e5b91f25afa5ab576ea90653))
