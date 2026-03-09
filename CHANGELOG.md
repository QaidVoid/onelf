
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
