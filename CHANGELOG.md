
## [0.3.4](https://github.com/QaidVoid/onelf/compare/0.3.3...0.3.4) - 2026-10-04

### ⛰️  Features

- *(bundle)* Find dlopen'd libraries by running the target - ([8d5763b](https://github.com/QaidVoid/onelf/commit/8d5763b753a2e111c77fae7f1f73fd6d1c80cc07))
- *(bundle)* Bring glibc's charset converters with libc - ([edebb21](https://github.com/QaidVoid/onelf/commit/edebb21c237a44630a3559ac124950ea73b37a9f))
- *(bundle)* Take the GPU stack from the host unless asked to bundle it - ([5acb29d](https://github.com/QaidVoid/onelf/commit/5acb29dd983c913001d5a12c98345f62797b8c8b))
- *(bundle)* Record a sysroot build's provenance - ([275b4a2](https://github.com/QaidVoid/onelf/commit/275b4a2c0b1c793e147285ff24a9637638546b90))
- *(bundle)* Take the closure from a pinned sysroot - ([d04162b](https://github.com/QaidVoid/onelf/commit/d04162bad476677625cdccfa5aaeb54c95171939))
- *(format)* Encode host-libs policy and cache request in flags - ([58e9347](https://github.com/QaidVoid/onelf/commit/58e934764020bd261d21844bbe81fe7fd51a8727))
- *(onelf)* Carry the GL build pin into packages - ([275697b](https://github.com/QaidVoid/onelf/commit/275697b5a88fda4fbefb168517ba8758772eebf9))
- *(pack)* Make the payload block size configurable - ([581728d](https://github.com/QaidVoid/onelf/commit/581728df2fa903feb0cddd361967ff4e7494b0d1))
- *(recipe)* Accept an https URL as the sysroot archive - ([8d25a74](https://github.com/QaidVoid/onelf/commit/8d25a74f0ae0ce877299c8254cedc5cdebd17c38))
- *(resolver)* Report the GL situation and index a fetched build - ([f177ff2](https://github.com/QaidVoid/onelf/commit/f177ff2b6a2a165fd30ba6c6c9a21652253d8aec))
- *(rt)* Serve a dependency set from its build instead of copying it - ([21797eb](https://github.com/QaidVoid/onelf/commit/21797ebc6d318cdba40ba6e4f4414b63e8708bf0))
- *(rt)* Fetch and use the dependency sets a package pins - ([73503b5](https://github.com/QaidVoid/onelf/commit/73503b5c9118c2b801e5c1b7aa132f11d4fd2709))
- *(rt)* Record a trace of opened files under ONELF_TRACE - ([e0b9381](https://github.com/QaidVoid/onelf/commit/e0b938147d28ec46d91e07afd3cc047dd2fc2dae))
- *(rt)* Fetch the pinned GL build when the host has none - ([25abda3](https://github.com/QaidVoid/onelf/commit/25abda398c10692c9a7d75e9bfa1dbe98abb11f7))
- *(rt)* Add the runtime-directory mode and make the cache opt-in - ([af70e21](https://github.com/QaidVoid/onelf/commit/af70e2163b8207f5f5c2a0fb23e1a6e3b3c19993))
- *(rt)* Reclaim dead helper mounts on the next launch - ([1817e36](https://github.com/QaidVoid/onelf/commit/1817e368b2398b6597c5e234dde12d89fd0ec8c2))
- *(rt)* Run under the host loader when its glibc is newer - ([1adf25c](https://github.com/QaidVoid/onelf/commit/1adf25cd5ffc276f3783c6219cdc3e900ae7ac61))
- *(rt)* Choose host libraries per soname at launch - ([29856c6](https://github.com/QaidVoid/onelf/commit/29856c6c827f5598d5da8199b1d4cdc48dee1196))
- *(rt)* Name the bundled Qt plugin directories - ([5183778](https://github.com/QaidVoid/onelf/commit/5183778c330bad6737fee24ee7d3c4dd6c47c957))
- *(run)* Resolve host libraries like the packed runtime - ([f2a9573](https://github.com/QaidVoid/onelf/commit/f2a95735792375fccd8aa3791344dbd07d52210c))
- *(sysroot)* A set build records what it ships - ([ba91be2](https://github.com/QaidVoid/onelf/commit/ba91be226742d00f2f15f4f9695f18e6bcc2973a))
- *(sysroot)* Shared dependency sets, packer and recipe side - ([e72b17a](https://github.com/QaidVoid/onelf/commit/e72b17ab6f130f5e40a27727b0c58af9edd7a510))
- *(sysroot)* Pin the sysroot archive by BLAKE3 hash - ([717afbf](https://github.com/QaidVoid/onelf/commit/717afbfe279997c11160794247f7efc426003d18))
- *(sysroot)* Keep globs, whole Python packages, host packages - ([6defc19](https://github.com/QaidVoid/onelf/commit/6defc19b25a62cca99fe8a806511f836a7b93959))
- *(sysroot)* Build a GL build's tree from named packages - ([52491bb](https://github.com/QaidVoid/onelf/commit/52491bb8c3c34baabcddd89bf638a0643177806e))
- *(sysroot)* Read pacman databases, closures, archives and prune - ([0906bd8](https://github.com/QaidVoid/onelf/commit/0906bd8ff478d2cc1d00de2dc1179fa04a28ff1b))
- *(sysroot)* Read the GL build pin from platform.toml - ([7808703](https://github.com/QaidVoid/onelf/commit/7808703fe24bd50576bc75088d0071092b49d239))
- Make the bundle answer for a compiled-in path - ([723801f](https://github.com/QaidVoid/onelf/commit/723801f4f0ab7995ea72c7e2292e226517a02ea8))

### 🐛 Bug Fixes

- *(bundle)* Bring the compat name service with every glibc - ([fdd33f5](https://github.com/QaidVoid/onelf/commit/fdd33f52073fb3f57c62f4608b18b525afdcc04b))
- *(bundle)* Keep a library subdirectory file's own directory on its run path - ([8af49aa](https://github.com/QaidVoid/onelf/commit/8af49aacf5665d24457a8c49d75e5b0757067e54))
- *(bundle)* Write one exact ORIGIN run path per file - ([747b6ea](https://github.com/QaidVoid/onelf/commit/747b6ea54c73c8cb7a88b68a9746e392b33714c1))
- *(bundle)* Remove only the dangling links a dropped object leaves - ([2351653](https://github.com/QaidVoid/onelf/commit/2351653b608e8bdcd6d5e4feae5ba13198d29862))
- *(bundle)* Search where an absolute DT_NEEDED entry points - ([1e50835](https://github.com/QaidVoid/onelf/commit/1e5083539e40a22ea39ec6e22484bb07fc7155b7))
- *(bundle)* Bring files and dns name services for a glibc before 2.34 - ([b6169d4](https://github.com/QaidVoid/onelf/commit/b6169d496eff3e95f73eec6ca6bedc8ebc6b78ef))
- *(bundle)* Find the string table without section headers - ([e3f0d73](https://github.com/QaidVoid/onelf/commit/e3f0d7364856d832af3c668e2c2bc82bf9ebd526))
- *(bundle)* Copy a transitive dependency despite a stray copy - ([b807a0e](https://github.com/QaidVoid/onelf/commit/b807a0e512fe725da7dd67c00a32a90763bcf189))
- *(bundle)* Keep an app's own GL beside its executable under --gl - ([c85cb0c](https://github.com/QaidVoid/onelf/commit/c85cb0cc301213b07f2effce8e91df0b4e9a7bab))
- *(bundle)* Drop only generated files for an unmet need - ([1b51cc0](https://github.com/QaidVoid/onelf/commit/1b51cc077196722ad051feb7c70f18fbedfd47ed))
- *(bundle)* Do not rewrite ELF files through symlinks - ([c0a1c82](https://github.com/QaidVoid/onelf/commit/c0a1c8235402cded8619a8defcc054d27af47304))
- *(bundle)* Scrub only loader paths in a bundled musl libc - ([d00f994](https://github.com/QaidVoid/onelf/commit/d00f994a32d128bda99c552d7aed551b1bed3085))
- *(bundle)* Scrub absolute rpaths that do not fit the origin slot - ([a4007e4](https://github.com/QaidVoid/onelf/commit/a4007e492431e77542ca00c2463a68b8c50355f6))
- *(bundle)* Remove what the previous sysroot build generated - ([807ff15](https://github.com/QaidVoid/onelf/commit/807ff15cf6e3b010d8a34fd8b2862d5fcac8a83a))
- *(bundle)* Drop objects the sysroot cannot satisfy, not the build - ([c8f2924](https://github.com/QaidVoid/onelf/commit/c8f29245ccd550f71c944df8861769630fce39f2))
- *(bundle)* Confine sysroot lookups to what the loader will find - ([659c941](https://github.com/QaidVoid/onelf/commit/659c94178d9c7099299f130aaf08b92afb28db12))
- *(bundle)* Fold a rootfs's compatibility links when flattening - ([c2af02a](https://github.com/QaidVoid/onelf/commit/c2af02af0a14f346ab81730221bdbdd0b665ec3c))
- *(cache)* Avoid overflow and a byte slice in cache listing - ([c91aa2a](https://github.com/QaidVoid/onelf/commit/c91aa2a37554edecd6f54c6706409e742ee6ecf9))
- *(cache)* Make cache clear honour the lock protocol - ([722e709](https://github.com/QaidVoid/onelf/commit/722e7096cfd6928b086eac0f76683e044ec0fc32))
- *(cli)* Refuse contradictory memfd flags and a key without a URL - ([1aa0e4e](https://github.com/QaidVoid/onelf/commit/1aa0e4ec535e98fe0311f2d5adc8647ab8af844c))
- *(cli)* Verify metadata before writing it to disk - ([9df1fa5](https://github.com/QaidVoid/onelf/commit/9df1fa5bc8a5e8ab9e48c1540f53a5553948f4f1))
- *(cli)* Report a package with no entrypoints instead of panicking - ([bd48912](https://github.com/QaidVoid/onelf/commit/bd48912a05d665816ad0fb1cddec289d61fb3602))
- *(extract)* Refuse -o - without a file and name missing files - ([5ba267a](https://github.com/QaidVoid/onelf/commit/5ba267a4712ef1fcbd45f708916f3166e9fd3cf1))
- *(format)* Key the link farm by host fingerprint - ([4ef749c](https://github.com/QaidVoid/onelf/commit/4ef749c6cf6c4c058459855e6f819d0dbdd57238))
- *(format)* Read only this ABI's entries from ld.so.cache - ([516bc41](https://github.com/QaidVoid/onelf/commit/516bc4177802d5a55f3f1dba0b3b7cb6bd0db035))
- *(format)* Require manifest lib dirs to stay inside the package - ([fc8f021](https://github.com/QaidVoid/onelf/commit/fc8f021b6c918b76c591b696780ce6ff94625139))
- *(format)* Seed the host walk from ICDs naming a bare soname - ([850ace7](https://github.com/QaidVoid/onelf/commit/850ace7dffd836dec2a83ca1656ef20bda9a451a))
- *(format)* Reject a manifest whose parent does not precede it - ([66fb969](https://github.com/QaidVoid/onelf/commit/66fb9696fff18f93080a0de1440ef6d488b0bb28))
- *(format)* Put the VA-API display wrappers on the platform line - ([d40d8a7](https://github.com/QaidVoid/onelf/commit/d40d8a7bfe51371a11f9faae15081b938e58d894))
- *(integrate)* Quote Exec paths in both integrators - ([259e09b](https://github.com/QaidVoid/onelf/commit/259e09be6896170e9d89d4331cc800d6e3f54eda))
- *(pack)* Keep what an object's own run path reaches - ([c9fa4cc](https://github.com/QaidVoid/onelf/commit/c9fa4cc0ec232434a028396fbd915e04771f23c3))
- *(pack)* Keep the resolver on for a package that pins a build - ([714c753](https://github.com/QaidVoid/onelf/commit/714c75327cf062dbf0f19aa3452b79059bbe3362))
- *(pack)* Accept a leading ./ in command and entrypoint paths - ([1ba5056](https://github.com/QaidVoid/onelf/commit/1ba50563fb4a4cf682d30adae3679eeb0a7138e6))
- *(pack)* Decide host-libs from the files actually packed - ([37ea08d](https://github.com/QaidVoid/onelf/commit/37ea08dffbf1d9666fcf6d908f03e2ba3afe34f5))
- *(pack)* Give each pack its own scratch files - ([04427a5](https://github.com/QaidVoid/onelf/commit/04427a58ce87f1c388c8f5ba44f4dfcee17b1245))
- *(pack)* Refuse a block size outside the supported range - ([5fa196f](https://github.com/QaidVoid/onelf/commit/5fa196f2ceed87a8637c8ecaf6c174041955ba94))
- *(pack)* Stage the output and skip a previous one in the tree - ([daf2c46](https://github.com/QaidVoid/onelf/commit/daf2c46a48df6bd9c2bb4a077548213f99138f36))
- *(rt)* Find the bundle's own GLib and GTK modules - ([797b353](https://github.com/QaidVoid/onelf/commit/797b35381a008fb7ccc9646421fc38543c3cbada))
- *(rt)* Hand a binary without an RPATH the bundled library path - ([be38d0c](https://github.com/QaidVoid/onelf/commit/be38d0c589aadec987dc1fe01c71f1058ab66e4d))
- *(rt)* Carry argv as bytes so a non-UTF-8 argument launches - ([60dafba](https://github.com/QaidVoid/onelf/commit/60dafba72c8192b5fea5a6bbd01fbf55315fcb11))
- *(rt)* Key content store blobs by mode as well as hash - ([8aebee2](https://github.com/QaidVoid/onelf/commit/8aebee24db72f9afce2be996b100729c27db3271))
- *(rt)* Validate the footer with the shared reader checks - ([81b69b9](https://github.com/QaidVoid/onelf/commit/81b69b9aab753c77086b794bb106f4aee1e77c38))
- *(rt)* Close close-on-exec descriptors before userland exec - ([6907f56](https://github.com/QaidVoid/onelf/commit/6907f56975e64a5bd3002d39225b984f301959e4))
- *(rt)* Keep serving the FUSE mount when the app is stopped - ([fa2ccf9](https://github.com/QaidVoid/onelf/commit/fa2ccf9d1b5cb32b2f8e332271c9ab580da219fc))
- *(rt)* Key the GL build store by hash so builds coexist - ([907324a](https://github.com/QaidVoid/onelf/commit/907324a40b3bdf2ec87384715cb655a1874eff3d))
- *(rt)* Keep a GL build off the search path, refetch a moved pin - ([ea6faac](https://github.com/QaidVoid/onelf/commit/ea6faacbb84b7c97e316ecdbe09c25ab13715ac9))
- *(rt)* Fetch the GL build when the host stack is partial - ([8f71321](https://github.com/QaidVoid/onelf/commit/8f71321c043a724cbb9569ef3327e799e70162bf))
- *(rt)* Script entrypoints get the resolver's lib path - ([5e4009b](https://github.com/QaidVoid/onelf/commit/5e4009bcc1735aa689ac84ce3b1e72b75be2251b))
- *(rt)* Put every library dir of a set on the path - ([b90812e](https://github.com/QaidVoid/onelf/commit/b90812e1c7248db9d174f6433706fb1c9076332b))
- *(rt)* Keep the interp line inside the package - ([3be561a](https://github.com/QaidVoid/onelf/commit/3be561a9e40334efed4869c85054a3397763ac89))
- *(rt)* Skip env lines std would abort on - ([5b214ce](https://github.com/QaidVoid/onelf/commit/5b214ce8525beb74b583ee5a6afc7b5cb7c24ede))
- *(rt)* Guard FUSE read against inode zero - ([2b6a147](https://github.com/QaidVoid/onelf/commit/2b6a147194bdc011f5123bc6a3e8094f40ebc583))
- *(rt)* Require a full BLAKE3 hash in a GL build pin - ([b55476b](https://github.com/QaidVoid/onelf/commit/b55476b6cbb8ba34d42a4a2b893bbea0c2f5efcb))
- *(rt)* Build the mountpoint name from path-safe characters - ([726c2e5](https://github.com/QaidVoid/onelf/commit/726c2e5d0cc5559a34ba47725d51b0ae958867db))
- *(rt)* Bound every block decode to its manifest length - ([0dac24a](https://github.com/QaidVoid/onelf/commit/0dac24aa5a7c22033fecadcb314b7fa341b49342))
- *(rt)* Size the userland exec stack like the kernel would - ([bb88286](https://github.com/QaidVoid/onelf/commit/bb882869c5c1c8dc40a3b9a8bbea44e63c80072c))
- *(rt)* Keep the fusermount3 descriptor from the app - ([452aebd](https://github.com/QaidVoid/onelf/commit/452aebdf7db254bb2f627758e50eea1b02666d97))
- *(sysroot)* Follow sonames when copying and dropping - ([3b2f26d](https://github.com/QaidVoid/onelf/commit/3b2f26dfcb5087cd6bf3084c4d96bf9ff0f8cd3d))
- *(sysroot)* A set promises only what its build can hold - ([717f65f](https://github.com/QaidVoid/onelf/commit/717f65f44d83ebb7990be9b052bf7cc4aef28d65))
- *(sysroot)* Keep a stripped setuid file readable - ([dd76d8f](https://github.com/QaidVoid/onelf/commit/dd76d8fe3f0efd449e43a15d8c2d9ce593c4a2a0))
- *(sysroot)* Carry the caches the distribution generated - ([c528958](https://github.com/QaidVoid/onelf/commit/c528958cf721568aae159558522eea069d639794))
- *(sysroot)* Apply the platform line when building a shared set - ([855095f](https://github.com/QaidVoid/onelf/commit/855095feabba1eac250bf1d273e6b28c8e0071f4))
- *(sysroot)* Refuse database paths that leave the sysroot - ([437fb4c](https://github.com/QaidVoid/onelf/commit/437fb4ce304bdea1fa588d8f399b2988559d0f15))
- *(sysroot)* Make the trace tier prune flat directories - ([a044fe7](https://github.com/QaidVoid/onelf/commit/a044fe73922f42f244e9b5c1f541835185f48ea7))
- *(sysroot)* Apply directory modes after unpacking - ([612f060](https://github.com/QaidVoid/onelf/commit/612f060bc799147d354c77d4d814f5ed0cdb74c7))
- *(sysroot)* Seed named packages and read ld.so.conf - ([07ec294](https://github.com/QaidVoid/onelf/commit/07ec2949fc953ecb5086f31e9c31478dafe491dc))
- *(verify)* Check per-block hashes the FUSE server enforces - ([1208c01](https://github.com/QaidVoid/onelf/commit/1208c018c704a4e465aa4cde730d1f36d960f90b))

### 🚜 Refactor

- *(bundle)* Keep one RUNPATH rewriter - ([f2c10b0](https://github.com/QaidVoid/onelf/commit/f2c10b0dfc850bf965244fc83a77261f40271bc5))
- *(format)* Drop the unused sharun compatibility flag - ([7a0929e](https://github.com/QaidVoid/onelf/commit/7a0929e56e70e30afcd2ee1e5a607c9bf6711429))

### ⚡ Performance

- *(rt)* Negotiate FUSE_MAX_PAGES so a read covers a whole block - ([c05dd67](https://github.com/QaidVoid/onelf/commit/c05dd6723a7f6ddd27d24d8aefaac988cf86c05f))
- Default to zstd 19, measured 6% smaller - ([43ac48d](https://github.com/QaidVoid/onelf/commit/43ac48dc64a410217050fab6d2c1af1706606559))

### 🎨 Styling

- Fmt - ([b575fb5](https://github.com/QaidVoid/onelf/commit/b575fb58ebcf8b083c239912e6f99391dca37d3a))

### 🧪 Testing

- *(sysroot)* Remove temp roots when a test ends - ([1f100fe](https://github.com/QaidVoid/onelf/commit/1f100fe357a25082e5b91f25afa5ab576ea90653))
- Wait for a killed process group before the next launch - ([1af2bfb](https://github.com/QaidVoid/onelf/commit/1af2bfb939217cfb32ac374f788079b3c3e67f50))
- Refuse a crafted entry path in extract, cache and rundir - ([cf1038b](https://github.com/QaidVoid/onelf/commit/cf1038be1c9837dc6abbc648ea0a8c1b9810d93d))
- Fail loudly under CI instead of skipping - ([d27cfef](https://github.com/QaidVoid/onelf/commit/d27cfef2a1838bd742cb27766b7bdda094a9c0ee))
- Cover vendor libraries, self-packing and a moved pin - ([7a7596d](https://github.com/QaidVoid/onelf/commit/7a7596da6dcd43933a6e515a9d615fb32ea41bdf))

## [0.3.3](https://github.com/QaidVoid/onelf/compare/0.3.2...0.3.3) - 2026-08-23

### ⛰️  Features

- *(bundle)* Report libraries resolved from the host - ([9b08dbd](https://github.com/QaidVoid/onelf/commit/9b08dbd31d200b54b870f23e15280db4eee1b1f6))
- *(format)* Verify payload blocks individually - ([a3cc7b7](https://github.com/QaidVoid/onelf/commit/a3cc7b799c76421f11199822a46304eea4c84a4d))
- *(format)* Flag packages updated from outside - ([68d64ef](https://github.com/QaidVoid/onelf/commit/68d64ef071b842bbff5bb5bca2402ab14be8e716))
- *(pack)* Withhold host lib dirs when nothing needs them - ([49ba57e](https://github.com/QaidVoid/onelf/commit/49ba57e0e3d4698afd2b59e78fe484f7136c7c52))
- *(pack)* Record an update URL without embedding the updater - ([1325a50](https://github.com/QaidVoid/onelf/commit/1325a50342a97a13fb8d5c4f8b5331aac1ef94b1))
- *(rt)* Make the host library dirs opt-out - ([bc2e0bc](https://github.com/QaidVoid/onelf/commit/bc2e0bcb363802f51973eb40d1d5b1d5a70ac78e))
- *(rt)* Use one hardened transport for self-update - ([27d9d9f](https://github.com/QaidVoid/onelf/commit/27d9d9fda69eaa197decf6e7fbed4caad14d778d))
- *(sign)* Add publisher signing tooling for self-update - ([d04805d](https://github.com/QaidVoid/onelf/commit/d04805d65dc7d87cd5ef8d8b235359e4a2d4fe8c))

### 🐛 Bug Fixes

- *(bundle)* State both outcomes for unbundled libraries - ([0602f95](https://github.com/QaidVoid/onelf/commit/0602f95683863c7ac57337a3c06b1c6d73abe86a))
- *(bundle)* Make the bundled search path inheritable - ([cad8332](https://github.com/QaidVoid/onelf/commit/cad83321c7856e7e86ad6711e6e8103c419fd0eb))
- *(bundle)* Pick the libc and arch from the entrypoint - ([6f8e9b5](https://github.com/QaidVoid/onelf/commit/6f8e9b55a8f89b2a8acba18486c5683729cc38a9))
- *(bundle)* Keep the RUNPATH rewrite inside .dynstr - ([c70176d](https://github.com/QaidVoid/onelf/commit/c70176d56c02a102abdabe6dee9da9eef4f5bb82))
- *(rt)* Return from the launcher when the app exits - ([666dd51](https://github.com/QaidVoid/onelf/commit/666dd5161b5704fa6826a76d0f1b6fbb54dc9839))
- *(rt)* Keep the fuse mount alive for a daemonized process - ([a215aff](https://github.com/QaidVoid/onelf/commit/a215aff9b3e8b50b035572b4e78bf7e5e394c0eb))
- *(rt)* Claim mountpoints and reclaim the cache under locks - ([96b67cd](https://github.com/QaidVoid/onelf/commit/96b67cd70bf22b180893563ca2ab46b374606189))
- *(rt)* Resolve host libs through the host ld.so.cache - ([2405865](https://github.com/QaidVoid/onelf/commit/2405865ae9459e3c0729a84ac3fdd3de7d00628b))
- *(rt)* Keep a read's own blocks from being evicted - ([759a6f8](https://github.com/QaidVoid/onelf/commit/759a6f875b744f5ba0075b780d9763a99a86b64e))
- *(test)* Make the bounded-memory test actually read the entry - ([f39bc4e](https://github.com/QaidVoid/onelf/commit/f39bc4e24f9d45782eb140968de9ab52185d5479))
- Refuse memfd when the entrypoint needs bundled libs - ([8f921cb](https://github.com/QaidVoid/onelf/commit/8f921cbab7ec07f05a7cb2db33927d63d3bd553e))
- Validate package regions before allocating from them - ([22a3b68](https://github.com/QaidVoid/onelf/commit/22a3b68bee109acb42c6a27110bb73d7b095879b))
- Honour entrypoint intent and stop forcing nested modes - ([89948d9](https://github.com/QaidVoid/onelf/commit/89948d9ea4bc437ba032ed7433bcef7ded918787))

### 🚜 Refactor

- *(format)* Share the detached signature URL rule - ([a0a8e79](https://github.com/QaidVoid/onelf/commit/a0a8e79735a9218a0bd9b1c2ff3a835d6858f5cf))

### 📚 Documentation

- Name the real source of host library resolution - ([69f5cdc](https://github.com/QaidVoid/onelf/commit/69f5cdc34f9ffaf55c3bc1d4df2265cdc5def29d))

### ⚡ Performance

- *(bundle)* Finalize each binary in one read and write - ([3bb9faa](https://github.com/QaidVoid/onelf/commit/3bb9faaa4597caa6c294c782b0f1fc29bd0fd8a0))
- *(pack)* Speed up host-libs detection, add GPU backends - ([c074619](https://github.com/QaidVoid/onelf/commit/c074619a84889834960f54776f590b41ff683d35))
- *(pack)* Stream file content instead of holding the tree - ([ed43132](https://github.com/QaidVoid/onelf/commit/ed43132810fd5b9f9de87dbfe5c8eef118049ba4))

### 🧪 Testing

- *(fuse)* Prove a large entry streams under a memory cap - ([cc6487e](https://github.com/QaidVoid/onelf/commit/cc6487efba9a7e80e994cdb15ef43445e6b5fd6d))
- *(pack)* Assert peak memory stays under the tree size - ([f906adb](https://github.com/QaidVoid/onelf/commit/f906adbd668983fa9fb3220e555a6e6135494f4d))
- Skip FUSE tests when mounting is not permitted - ([ff8be89](https://github.com/QaidVoid/onelf/commit/ff8be89627d015a2d4a3c64c9abc4a9269672b18))
- Cover unsigned self-update and concurrent extraction - ([d0fca56](https://github.com/QaidVoid/onelf/commit/d0fca56e09e42f92cdb789643d55f2683fedba82))
- Cover interpreter choice, search paths, and cache refusal - ([5fc169e](https://github.com/QaidVoid/onelf/commit/5fc169e3125ab44f13d1c0cd381bf30e20a9f6d5))
- Cover truncation, owner-only modes, and live-package gc - ([10a6496](https://github.com/QaidVoid/onelf/commit/10a6496346e514a5233ba102c9afd8e87b424320))

### ⚙️ Miscellaneous Tasks

- Add ci gate and clear lint and comment debt - ([17d4c42](https://github.com/QaidVoid/onelf/commit/17d4c424578eef621a8e205a54c99ca82b985613))

### Build

- Make the embedded runtime reproducible - ([63aa826](https://github.com/QaidVoid/onelf/commit/63aa826b68c78b9dbb707755d7fc4fab45658f9f))
- Resolve toolchain binaries without which - ([26e8d32](https://github.com/QaidVoid/onelf/commit/26e8d32c7ef59f3ba77c16ad9ac3b4be0e97ec33))

## [0.3.2](https://github.com/QaidVoid/onelf/compare/0.3.1...0.3.2) - 2026-08-15

### ⛰️  Features

- *(pack)* Let a package say it runs setuid binaries - ([37e7c92](https://github.com/QaidVoid/onelf/commit/37e7c92a3e5a249364399ecbef789a5c3c7ecf13))

## [0.3.1](https://github.com/QaidVoid/onelf/compare/0.3.0...0.3.1) - 2026-08-10

### 🐛 Bug Fixes

- *(rt)* Keep the bundled libraries out of the app's children ([#31](https://github.com/QaidVoid/onelf/pull/31)) - ([1e5385e](https://github.com/QaidVoid/onelf/commit/1e5385e0ba3567a944ac144814b42f39f69106a6))

## [0.3.0](https://github.com/QaidVoid/onelf/compare/0.2.8...0.3.0) - 2026-07-26

### ⛰️  Features

- *(i686)* Add 32-bit x86 target and native runtime support ([#29](https://github.com/QaidVoid/onelf/pull/29)) - ([4eed307](https://github.com/QaidVoid/onelf/commit/4eed307d40a0f7465c7b815103f36f11462f7305))
- *(pack)* Accept .onelf/icons and .onelf/desktop assets - ([23b3842](https://github.com/QaidVoid/onelf/commit/23b3842b33de7347ebcfa384a7782cfd13b94d2a))
- Port bootstrap and env payloads to no_std Rust ([#28](https://github.com/QaidVoid/onelf/pull/28)) - ([acb6035](https://github.com/QaidVoid/onelf/commit/acb6035966db68a11df7b95d6fe382a303874ec2))
- Harden runtime dirs, cache, and self-update ([#23](https://github.com/QaidVoid/onelf/pull/23)) - ([d3a53b9](https://github.com/QaidVoid/onelf/commit/d3a53b981b28120afbdfaa6dbc19b79339499105))

### 🐛 Bug Fixes

- *(bundle)* Don't strip Bun embedded-payload binaries - ([f0ac369](https://github.com/QaidVoid/onelf/commit/f0ac369e98c5d9c68dfb875226e2ed447d8330f1))
- *(bundle)* Skip ELF rewrites for Bun .bun-section binaries - ([07c0434](https://github.com/QaidVoid/onelf/commit/07c04343a84a6b03339c34ddf4ba8928533ab304))
- Bundler correctness for dry-run, runpaths, arch, aarch64 ([#25](https://github.com/QaidVoid/onelf/pull/25)) - ([53c6f10](https://github.com/QaidVoid/onelf/commit/53c6f1098f7310d19e54f315aec5339f4ad7ff12))
- Packer correctness for name, entrypoint, recipe, extract ([#24](https://github.com/QaidVoid/onelf/pull/24)) - ([18456d0](https://github.com/QaidVoid/onelf/commit/18456d0f2b9e416013d8d14472cd69d29f619bad))
- Make packing and bundling output deterministic ([#22](https://github.com/QaidVoid/onelf/pull/22)) - ([2e46a8d](https://github.com/QaidVoid/onelf/commit/2e46a8d0cff03d3dbcc82e3aba77c73b8c5b78ff))
- Verify payload hashes and validate extraction paths ([#21](https://github.com/QaidVoid/onelf/pull/21)) - ([d4a2930](https://github.com/QaidVoid/onelf/commit/d4a2930684699aaedce5d98d49fe14d3ed8d825a))

### 🚜 Refactor

- Dedup runtime resolvers and split bundle.rs modules ([#27](https://github.com/QaidVoid/onelf/pull/27)) - ([d7f81c1](https://github.com/QaidVoid/onelf/commit/d7f81c113db0d7a736dfe95138e551b8086edc13))
- Doc hygiene, internal dedup, remove onelf-preload ([#26](https://github.com/QaidVoid/onelf/pull/26)) - ([7b64f8e](https://github.com/QaidVoid/onelf/commit/7b64f8eb2c81ac5afeb2c207c60f9cd9dcd93d0f))

## [0.2.8](https://github.com/QaidVoid/onelf/compare/0.2.7...0.2.8) - 2026-06-14

### ⛰️  Features

- *(bundle)* Detect frameworks by versioned soname; add --no-* opt-outs - ([843dd80](https://github.com/QaidVoid/onelf/commit/843dd803cf4d0c9e7a8467da6bf27bed72e53cfb))

### 🐛 Bug Fixes

- *(bundle)* Keep EGL vendor JSON whose library_path is a bare soname - ([b65010b](https://github.com/QaidVoid/onelf/commit/b65010bcfc98ef21bfc8c0fcfe124f48e8b73dd3))

### 🎨 Styling

- Rustfmt assert! and data builder in pipeline test - ([c81b7da](https://github.com/QaidVoid/onelf/commit/c81b7dae4d6c8d098079e02c8c70cf5f58805f5e))

## [0.2.7](https://github.com/QaidVoid/onelf/compare/0.2.6...0.2.7) - 2026-06-13

### 🐛 Bug Fixes

- Require NUL boundary in scan_framework_strings to avoid false framework detection - ([f7bc735](https://github.com/QaidVoid/onelf/commit/f7bc735c34a9528df302f711e5b61e53e90808d0))

## [0.2.6](https://github.com/QaidVoid/onelf/compare/0.2.5...0.2.6) - 2026-05-19

### ⛰️  Features

- *(bundle)* Surface executables without baked-in $ORIGIN RUNPATH - ([b5baadf](https://github.com/QaidVoid/onelf/commit/b5baadfa3522f19f703d74d9ae8c4527138562de))
- *(env)* POSIX ${VAR:-word} default; PATH falls back to /usr/bin:/bin - ([fd8ec65](https://github.com/QaidVoid/onelf/commit/fd8ec65180ffa19704f497265979672f0669cb15))
- *(env)* Runtime ${VAR} passthrough + $ONELF_DIR/bin on PATH by default - ([c81904b](https://github.com/QaidVoid/onelf/commit/c81904b6b850f1c9246bbf0ad5df0b230181e5f7))
- *(pack)* [preload] recipe key + --preload, emit .onelf/preload - ([59dd5b3](https://github.com/QaidVoid/onelf/commit/59dd5b386b7cc0fb36d16cb0bf58b30d89c2549c))
- Re-exec-safe payload-side env via onelf-env DT_NEEDED constructor - ([e369c92](https://github.com/QaidVoid/onelf/commit/e369c922f7945b6615cc4f49e0404109021a29c7))
- Add store mode for uncompressed payloads - ([7da4dd1](https://github.com/QaidVoid/onelf/commit/7da4dd1d9a05eaac96f24d33773b00c87837a0f4))

### 🧪 Testing

- Unit + e2e coverage for store mode, onelf-env, preload - ([788d1d0](https://github.com/QaidVoid/onelf/commit/788d1d0eb7580dca7f0071ee93ad9ca205accdbe))

### Build

- *(payload)* Build real aarch64 onelf-env blob - ([67024a6](https://github.com/QaidVoid/onelf/commit/67024a60f6d3fbfd77834f325aba7dc0a6937a8c))

## [0.2.5](https://github.com/QaidVoid/onelf/compare/0.2.4...0.2.5) - 2026-04-25

### ⛰️  Features

- *(bundle)* Fall back to patchelf when no DT_RUNPATH slot is available - ([e108f18](https://github.com/QaidVoid/onelf/commit/e108f1885e3ddf9947a636de88c3e8d1a9ae8684))
- *(rt)* /tmp symlink fallback for self-extract in cache mode - ([d561bb3](https://github.com/QaidVoid/onelf/commit/d561bb310d149d176a88f22a01d775cb99bd6bc9))
- *(rt)* Bind-mount bundled linker for self-extract binaries - ([2f0c229](https://github.com/QaidVoid/onelf/commit/2f0c2297a932d5b90bec3757f8bb597e3bd7ac66))
- Add desktop integration (integrate/unintegrate) - ([d72b686](https://github.com/QaidVoid/onelf/commit/d72b686ab86d5ff33b853ba10313ee1e4faa501d))

### 🐛 Bug Fixes

- *(bundle)* Preserve self-extract trailer on Bun-compiled binaries - ([2cbcc7b](https://github.com/QaidVoid/onelf/commit/2cbcc7bcb90035a320d3d36a931017c02cc2f152))

### 🚜 Refactor

- *(rt)* Pass lib paths via --library-path on linker invocations - ([a4e4259](https://github.com/QaidVoid/onelf/commit/a4e4259d3e36110b972f6aa7b1555aa19cb4248d))

## [0.2.4](https://github.com/QaidVoid/onelf/compare/0.2.3...0.2.4) - 2026-04-17

### ⛰️  Features

- *(recipe)* Add [env] section for custom environment variables - ([5156b29](https://github.com/QaidVoid/onelf/commit/5156b299a5ad1f65a869b295fa3427ee3fb987dc))

### 🐛 Bug Fixes

- *(bundle)* Reorder bootstrap PT_LOAD to end of phdr table - ([8a26af8](https://github.com/QaidVoid/onelf/commit/8a26af8e3e751ddb1ed32d786a6565b48e7b1a4f))
- *(recipe)* Preserve unset env vars for runtime expansion - ([ba2045d](https://github.com/QaidVoid/onelf/commit/ba2045d66ae6bfd945ecb7abb46ad9e0993adc94))

## [0.2.3](https://github.com/QaidVoid/onelf/compare/0.2.2...0.2.3) - 2026-04-17

### ⛰️  Features

- *(bundle)* Replace PT_INTERP patching with AT_EXECFN bootstrap - ([e9482ae](https://github.com/QaidVoid/onelf/commit/e9482aeb91a3b5345855bcdd58c0d19e71621e68))

### 🐛 Bug Fixes

- *(fuse)* Detect and clean up stale FUSE mounts - ([bd01211](https://github.com/QaidVoid/onelf/commit/bd01211c6f1f5f9377bda463292fe9399f6bb47f))

### 🚜 Refactor

- *(rt)* Remove force_cwd and cache-mode PT_INTERP rewrite - ([9314016](https://github.com/QaidVoid/onelf/commit/9314016d60d0517165da3dc4f0213bc18503f92a))

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
