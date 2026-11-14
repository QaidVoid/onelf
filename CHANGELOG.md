
## [0.6.1](https://github.com/QaidVoid/onelf/compare/0.6.0...0.6.1) - 2026-11-14

### ⛰️  Features

- *(rt)* Show set downloads in a dialog, notification or terminal - ([fc7a965](https://github.com/QaidVoid/onelf/commit/fc7a96507f4a8610c159ccac5ba75a7eef6d7922))
- *(rt)* Collect idle sets and GL builds at launch - ([0a278c8](https://github.com/QaidVoid/onelf/commit/0a278c800c0509f98b21830bb18596fbd1323097))

### 🐛 Bug Fixes

- *(rt)* Let set downloads finish slowly and resume after drops - ([6096cfd](https://github.com/QaidVoid/onelf/commit/6096cfdc274c28a7afc8f75c4ef11d9db6bd7719))

### 🎨 Styling

- Fmt - ([4523f1c](https://github.com/QaidVoid/onelf/commit/4523f1c83e8b7e4b380a187d42a337a3436cfd1c))

### ⚙️ Miscellaneous Tasks

- Update Cargo.toml dependencies - ([0000000](https://github.com/QaidVoid/onelf/commit/0000000))

## [0.6.0](https://github.com/QaidVoid/onelf/compare/0.5.1...0.6.0) - 2026-11-14

### ⛰️  Features

- *(build)* Add a dry run and sorted completeness findings - ([20fad17](https://github.com/QaidVoid/onelf/commit/20fad17f3c703c0124b08a715de6184d77a39a85))
- *(build)* Load project-local rules and carry their mappings - ([ecd0371](https://github.com/QaidVoid/onelf/commit/ecd037153f386631f99e6b7df30d388ab112b19f))
- *(build)* Print the staging summary on builds - ([a7036ff](https://github.com/QaidVoid/onelf/commit/a7036ffc513e4515aa64b7905491521e9bdd48bb))
- *(bundle)* Trim inferred dirs to what a trace opened - ([05d5b1a](https://github.com/QaidVoid/onelf/commit/05d5b1a79efeefba83c4f33f1b7c07b87c327bb6))
- *(bundle)* Write the plugin directory record - ([f588945](https://github.com/QaidVoid/onelf/commit/f588945410ca0f8a386d4b8ec31d100c6dd1da0f))
- *(bundle)* Rewrite compiled-in paths to anchor paths - ([50835ef](https://github.com/QaidVoid/onelf/commit/50835ef24f55b4f48720f6d090c50e7983068717))
- *(bundle)* Choose one redirection per inferred directory - ([dd9c665](https://github.com/QaidVoid/onelf/commit/dd9c665533f36a2cdbded1ab275d112d3968b4c2))
- *(bundle)* Omit unloadable inferred members in non-strict scans - ([b6c8b23](https://github.com/QaidVoid/onelf/commit/b6c8b2360de6d54095a03380fa48b22988b21669))
- *(bundle)* Carry inferred plugin directories - ([48584bf](https://github.com/QaidVoid/onelf/commit/48584bf14cc3aa542af441a548ceaec80735dd2b))
- *(bundle)* Collect inferred data directories - ([7d62fc9](https://github.com/QaidVoid/onelf/commit/7d62fc9f0aecad0d917bdaa460dd9e283e3d4610))
- *(bundle)* Drop shared, libc and driver dirs from inference - ([f6cc1df](https://github.com/QaidVoid/onelf/commit/f6cc1df73481cdf118067179376fe58f6f8e6f79))
- *(bundle)* Report the directories bundled objects compile in - ([be207dd](https://github.com/QaidVoid/onelf/commit/be207ddbe95f8c6e7160a8234f574c21c53848d5))
- *(bundle)* Audit each entrypoint's own selection - ([a919b4f](https://github.com/QaidVoid/onelf/commit/a919b4fb224315ed5c6755ccdba0d191c8ce73b2))
- *(bundle)* Render deterministic completeness findings - ([5e9ec7f](https://github.com/QaidVoid/onelf/commit/5e9ec7f641f5b9baeeeb022509b41ebd49c7bd6b))
- *(bundle)* Prepare sysroot build set helpers - ([bea0796](https://github.com/QaidVoid/onelf/commit/bea0796594df2c8bfd75a3f68f6abb4617a64e93))
- *(bundle)* Prepare per-pin set resource subtrees - ([ebb693c](https://github.com/QaidVoid/onelf/commit/ebb693ce1fddf033a5c4be09137b57f0da328de1))
- *(bundle)* Audit per-entrypoint provider collisions - ([76d4119](https://github.com/QaidVoid/onelf/commit/76d41191898aa6520f3902b049e6b0670a1beeec))
- *(bundle)* Register staged build hooks with catalog coverage - ([95f10f1](https://github.com/QaidVoid/onelf/commit/95f10f1db6af0eff71b08babb2641953702506d0))
- *(bundle)* Classify omissions and pruning conflicts - ([84e015b](https://github.com/QaidVoid/onelf/commit/84e015b3c63deaf04bae023cc015187b424ddf91))
- *(bundle)* Collect helper executables via descriptors - ([ece060e](https://github.com/QaidVoid/onelf/commit/ece060e64c22579974e03370e225d20c7a3f9c3f))
- *(bundle)* Carry data via ordered descriptor alternatives - ([2182440](https://github.com/QaidVoid/onelf/commit/2182440505923d37335e8de19c28a0425ce030b6))
- *(bundle)* Collect plugin groups into preserved layouts - ([2036a05](https://github.com/QaidVoid/onelf/commit/2036a050530789c7011bbf201b0ef544a9a607b4))
- *(bundle)* Discover source candidates on admitted roots - ([c79c3e3](https://github.com/QaidVoid/onelf/commit/c79c3e3c1e9ad7354ad1d3f0c410779e9b4961ee))
- *(bundle)* Activate framework rules to a fixed point - ([1daa87b](https://github.com/QaidVoid/onelf/commit/1daa87bc92ffc67a838f1a642f0e0f114fe1564b))
- *(bundle)* Plan destinations before any mutation - ([07b585a](https://github.com/QaidVoid/onelf/commit/07b585a94433a9879bed7c839261b3a63465239f))
- *(catalog)* Carry pipewire client-rt.conf for pre-1.4 clients - ([44e4862](https://github.com/QaidVoid/onelf/commit/44e4862819ff3c5e7436f02faee0c3825a6177dd))
- *(catalog)* Remove sections inference reproduces - ([ea6acde](https://github.com/QaidVoid/onelf/commit/ea6acdefb79a181c42dec39c856de3335f039df0))
- *(cli)* Explain sets and views in info and doctor - ([077408c](https://github.com/QaidVoid/onelf/commit/077408c473264b6fd99f4f8542095fcf143afe51))
- *(doctor)* Name admitted and refused host modules - ([e385728](https://github.com/QaidVoid/onelf/commit/e385728af513b64d917a055f8303ea077b2e2c37))
- *(format)* Share the glibc loader scrub and count its search dirs - ([d3ab06a](https://github.com/QaidVoid/onelf/commit/d3ab06a50a736cafd5ddddcf24f5ee14005b9356))
- *(format)* Parse the path overlay record strictly and bounded - ([3c2ad07](https://github.com/QaidVoid/onelf/commit/3c2ad07ae815c0a798d4cdab8ff6414d12dedf32))
- *(format)* Derive lint and runtime env knowledge - ([1dd4525](https://github.com/QaidVoid/onelf/commit/1dd4525693465a92fd59142ec142fd853abc9afd))
- *(format)* Add runtime capability note and gate - ([aff116c](https://github.com/QaidVoid/onelf/commit/aff116c2b76a8a9917b69e2ec356295e5537c05d))
- *(format)* Add the plugin directory record codec - ([9ce9b5a](https://github.com/QaidVoid/onelf/commit/9ce9b5adaa688e90b48ff76e5011e51a463fa788))
- *(format)* Add the inference exclusions table to the catalog - ([1050694](https://github.com/QaidVoid/onelf/commit/1050694159813a4febc025d3aa5bc713334da522))
- *(format)* Add canonical selection identity and storage - ([d8c2e2e](https://github.com/QaidVoid/onelf/commit/d8c2e2e7e6e6712847c0abeb33ba73aa1508e1dc))
- *(format)* Add bounded ELF fact inspection - ([2355f11](https://github.com/QaidVoid/onelf/commit/2355f11d495ecf326fea3ed6b99c5056db6e134b))
- *(format)* Add built-in rule catalog and metadata codecs - ([e0e0625](https://github.com/QaidVoid/onelf/commit/e0e06255115f14ede6f66231a099bb30fa94e3a7))
- *(format)* Add validated framework descriptor schemas - ([5b43504](https://github.com/QaidVoid/onelf/commit/5b4350481ad3f9fc674e3d216e3c1e479c10fed4))
- *(pack)* Append inferred pairs to the paths record - ([b923a0d](https://github.com/QaidVoid/onelf/commit/b923a0d9cb8ce50d12ff0039df64ef04b29e7684))
- *(pack)* Lower entrypoint-sets into built packages - ([5da82ac](https://github.com/QaidVoid/onelf/commit/5da82ac1da8c75d551a11a7e29348bfc11b554c2))
- *(pack)* Add gated runtime metadata lowering - ([a2ff2fa](https://github.com/QaidVoid/onelf/commit/a2ff2fa625582f27c455f0aaf6b2ab9a762d25fc))
- *(recipe)* Add primary and named set selections - ([68700a4](https://github.com/QaidVoid/onelf/commit/68700a43ef3fc85a5a6b8a28bf68fc8748bc1eca))
- *(recipe)* Add verified binary input import - ([5a2b904](https://github.com/QaidVoid/onelf/commit/5a2b9046d73cfe4b43cd7c9fd9ea8860d8ffd4c2))
- *(resolve)* Derive host counterpart directories - ([d34c6f7](https://github.com/QaidVoid/onelf/commit/d34c6f7207b9ac9d0e59cc8485e6e1e78da38747))
- *(resolve)* Admit checked host counterpart modules - ([feeeeb7](https://github.com/QaidVoid/onelf/commit/feeeeb7ce0bb9e86d006eb725132d8de3b3ac0da))
- *(rt)* Present admitted host plugin modules as merged views - ([b8d78b0](https://github.com/QaidVoid/onelf/commit/b8d78b011d8f7f07f742f9e91d1c877d1946647e))
- *(rt)* Hold descriptor 77 on the package's anchor directory - ([280fdae](https://github.com/QaidVoid/onelf/commit/280fdaef4efc32c2442c303f939a620bf3361a16))
- *(rt)* Validate selected providers and advertise store import - ([223229d](https://github.com/QaidVoid/onelf/commit/223229dd4183db6c765cf777e53b511ad48dbf84))
- *(rt)* Integrate immutable views into all launch modes - ([d5e53bd](https://github.com/QaidVoid/onelf/commit/d5e53bdc777902beca318b760b631904fa513e9e))
- *(rt)* Collect views under lease-aware sweeps - ([85b52fc](https://github.com/QaidVoid/onelf/commit/85b52fc524866261b6104422d5ed8e9a4fa23565))
- *(rt)* Evaluate carried runtime rules and prepared pins - ([23f0af9](https://github.com/QaidVoid/onelf/commit/23f0af93b64fa02460921070d6c2d99e9b6b8e53))
- *(rt)* Export the resolved store through launch env - ([d65b2ee](https://github.com/QaidVoid/onelf/commit/d65b2eef5070ce08a8d013f6da0981253b5ea399))
- *(rt)* Expose launch support through a library target - ([26b8872](https://github.com/QaidVoid/onelf/commit/26b88721ece309d1dca96adb0bbdc04be6627f87))
- *(rt)* Scope set acquisition to entrypoint selections - ([7630b2b](https://github.com/QaidVoid/onelf/commit/7630b2b33600105f0f4af0d1b06206fbe4e3c044))
- *(rt)* Stage views with atomic verified publication - ([5b42cdb](https://github.com/QaidVoid/onelf/commit/5b42cdbfbe3fa94c4b718cb577d67bf75f2dc2df))
- *(rt)* Add pre-fork subreaper lifetime guardian - ([9e08c5c](https://github.com/QaidVoid/onelf/commit/9e08c5c9b3451b51b2474e0c9bbd3523b1bfd829))
- *(rt)* Add normalized view identity and records - ([7d0c312](https://github.com/QaidVoid/onelf/commit/7d0c312a6ebd5f2923afc4253da8978d83d6aade))
- *(rt)* Stage cached builds under locked verified publication - ([939d462](https://github.com/QaidVoid/onelf/commit/939d46295ec82ce69385757b72a79f2d9f5e36d0))
- *(rt)* Publish bounded per-view environment input - ([5c83c15](https://github.com/QaidVoid/onelf/commit/5c83c156656ddede34783a5f3ab8d103116f156c))
- *(rt)* Pin stored candidates and split store ownership - ([2995d46](https://github.com/QaidVoid/onelf/commit/2995d466dd7e74ce0cfcc0d64b9a13d513deeead))
- *(rt)* Evaluate path mappings and register hooks - ([5de8072](https://github.com/QaidVoid/onelf/commit/5de8072fc5359c8e7090a4008babba984d1bea6b))
- *(rt)* Verify cached artifacts through owned handles - ([eca4290](https://github.com/QaidVoid/onelf/commit/eca429031ea94a4e61ea557f0d1854a4da85921b))
- *(rt)* Validate payload metadata before launch dispatch - ([eba3c4d](https://github.com/QaidVoid/onelf/commit/eba3c4dfeaaf91c432742f5a672acaf1d6881366))
- *(rules)* Collect only client-side PipeWire and SPA members - ([589925d](https://github.com/QaidVoid/onelf/commit/589925dd99119741a9c140b7003085ac52fdb56d))
- *(rules)* Let plugin groups list the members they collect - ([443ca55](https://github.com/QaidVoid/onelf/commit/443ca5582da0132769a369d6a6e36f34bf40aaee))
- *(run)* Launch development runs from prepared views - ([49d416f](https://github.com/QaidVoid/onelf/commit/49d416fca97172b6a3bd5085cd60887c57615939))
- *(run)* Launch bundles through shared runtime support - ([0a1e0ad](https://github.com/QaidVoid/onelf/commit/0a1e0add60b88a3a9a4e13e36ed08f07ca765c3f))
- *(sysroot)* Make generated cleanup hash-aware - ([bb8c37b](https://github.com/QaidVoid/onelf/commit/bb8c37babdc1a4617fb8ce7f0f436041088b5172))
- *(sysroot)* Verify provider index for set pins - ([bc0c474](https://github.com/QaidVoid/onelf/commit/bc0c47477087f0398b539e529fd8c614a5798cf8))

### 🐛 Bug Fixes

- *(assembly)* Refuse self-imports and guard edits after failures - ([7ed58be](https://github.com/QaidVoid/onelf/commit/7ed58be3f0bf2898691b5708517da54b21682582))
- *(bundle)* Find merged dlopen sonames, leave GPU libs to host - ([cd6d86a](https://github.com/QaidVoid/onelf/commit/cd6d86a92ad684ac49c9ca0e4de94ca621fa476d))
- *(bundle)* Check inferred members against tree, libc and trace - ([38b03da](https://github.com/QaidVoid/onelf/commit/38b03da6a3bae9e85fea864fd42b18e0217839b6))
- *(bundle)* Anchor by path prefix and keep inferred pairs - ([19eefbb](https://github.com/QaidVoid/onelf/commit/19eefbb9568346e7cc92a83483b88ac5e4884fd1))
- *(bundle)* Scrub PowerPC glibc loaders too - ([6fdf366](https://github.com/QaidVoid/onelf/commit/6fdf3662677796c2da0ca2ab801e629d23031be5))
- *(bundle)* Carry glibc's compatibility libraries beside libc - ([2a56fc2](https://github.com/QaidVoid/onelf/commit/2a56fc26921b669a834bdb9d27f44a03eee16032))
- *(bundle)* Treat recipe exclusions as policy in strict builds - ([56d335c](https://github.com/QaidVoid/onelf/commit/56d335c1f6a0b25c9e9606132c8c91ec31d756cf))
- *(bundle)* Keep compiled schemas off symlinked AppDir paths - ([87499e1](https://github.com/QaidVoid/onelf/commit/87499e101e9568584503fa07aa3e9b497929575e))
- *(bundle)* Trust a hinted set store only when the uid owns it - ([abaff4e](https://github.com/QaidVoid/onelf/commit/abaff4e7f029c9c154ffe11782f53f670dba9ff4))
- *(bundle)* Find data for tree-copied rule libraries on the host - ([9360155](https://github.com/QaidVoid/onelf/commit/93601555e520d9a0c422cbfd29c98b7b7c5cfffe))
- *(bundle)* Strict host scans, pre-write planning and set audits - ([562fb45](https://github.com/QaidVoid/onelf/commit/562fb45ead055da7b44c5bd52a6cb5c0eeb695ce))
- *(bundle)* Keep import sources out of the walk - ([ad270a9](https://github.com/QaidVoid/onelf/commit/ad270a905b976a6e73677316934549c2dbe2c438))
- *(bundle)* Bound repair discovery to eligible sources - ([686fc3b](https://github.com/QaidVoid/onelf/commit/686fc3ba6b4c863c538574046b42620384c0e62f))
- *(catalog)* Restore the qt, babl and gegl plugin sections - ([fbfd16b](https://github.com/QaidVoid/onelf/commit/fbfd16b305c18fffce7a8f45d1c7e3e02e7d837e))
- *(cli)* Publish verified sets only and persist root's cache - ([7d08f6e](https://github.com/QaidVoid/onelf/commit/7d08f6e246f9d656fb5074aed15123e71a75d055))
- *(cli)* Gate low-level pack and drop duplicate set parsers - ([15b4247](https://github.com/QaidVoid/onelf/commit/15b42473bd99058767c7fc09fd9f98c344d7dd94))
- *(doctor)* Name each mapping and host library, not env values - ([a183605](https://github.com/QaidVoid/onelf/commit/a1836055ce48d0ff9ab0a3f6282eb0f14ae5bba8))
- *(env)* Keep the caller's LD_LIBRARY_PATH from the app - ([4d5822e](https://github.com/QaidVoid/onelf/commit/4d5822e31fededd1ca028ec57bafdb8aebeb0101))
- *(format)* Bind provider indexes to the pin, not the name - ([2dfabd4](https://github.com/QaidVoid/onelf/commit/2dfabd473959c55219d21e2a8d0847436d251901))
- *(format)* Treat malformed onelf notes as incompatible - ([aaf49a3](https://github.com/QaidVoid/onelf/commit/aaf49a3debc31953237299ab117c13b7289aa1a6))
- *(format)* Accept lld version chains and read dynstr once - ([35eec27](https://github.com/QaidVoid/onelf/commit/35eec27274d6c12738e4f1c67894ad7d9e7d494d))
- *(fuse)* Say FUSE is unavailable when extraction takes over - ([2cf43d8](https://github.com/QaidVoid/onelf/commit/2cf43d874a5d87b027aee942985a7f23aa94a9eb))
- *(pack)* Stream input blocks to bound worker memory - ([84d2266](https://github.com/QaidVoid/onelf/commit/84d2266f71afcc392111873eace358a10e8d7d1e))
- *(payloads)* Bound env reads and align the runtime-env codec - ([8f5de0b](https://github.com/QaidVoid/onelf/commit/8f5de0b1111cee5e8d98f937f7bcca2c2e776647))
- *(resolve)* Refuse host modules another user could plant - ([980c517](https://github.com/QaidVoid/onelf/commit/980c517c45bd2c25e6845329b147846b4be8e692))
- *(resolve)* Run the host loader from a closed private copy - ([e13a1f5](https://github.com/QaidVoid/onelf/commit/e13a1f5f7fc680dd7d717b2c88ae1439256bcbe1))
- *(rt)* Scope resolver decisions and provider checks to selections - ([faebd83](https://github.com/QaidVoid/onelf/commit/faebd83a5753c356fd7c7dad2c04ddff7bff9fd3))
- *(rt)* Keep views, guardians and signals faithful per launch - ([f8a66ca](https://github.com/QaidVoid/onelf/commit/f8a66cacfa907893664747f7d7e4ad5028e17fa1))
- *(rt)* Import pinned builds from the hinted store safely - ([fa3db19](https://github.com/QaidVoid/onelf/commit/fa3db192252a43f16a66b36556bbf4acb29c89d2))
- *(rt)* Keep nvidia-settings libraries out of the driver closure - ([67cc8ba](https://github.com/QaidVoid/onelf/commit/67cc8ba3923a2bb7d802c7bb3b8de2134473f01f))
- *(rt)* Keep root's cache and base off env-chosen paths - ([5c9227b](https://github.com/QaidVoid/onelf/commit/5c9227b64962ec594c7f0a1fee4abd39093a5425))
- *(rt)* Let the doctor see a helper FUSE mount place host files - ([4d6d8a9](https://github.com/QaidVoid/onelf/commit/4d6d8a9ea2f1c75759a9534a9c3255b3c533b6fb))
- *(rt)* Judge a missing hinted store by its nearest ancestor - ([255869a](https://github.com/QaidVoid/onelf/commit/255869a52150f053b2a4643d081f41c1e9db15a5))
- *(rt)* Classify carried rule variables like built-in ones - ([44814ac](https://github.com/QaidVoid/onelf/commit/44814ac536198cdddf3ef0e308428e5a6d1a371f))
- *(rules)* Map translations at runtime without copying locales - ([a947b40](https://github.com/QaidVoid/onelf/commit/a947b40d4d19a38d7922257dae12e674361038a3))
- *(rules)* Refuse loader, runtime-owned and shared env names - ([b656df1](https://github.com/QaidVoid/onelf/commit/b656df16fb2005725ce7ec274fe1b53bffe8a34f))
- *(run)* Hold the view lease and use the packed library gate - ([1dddfe2](https://github.com/QaidVoid/onelf/commit/1dddfe2feb0db6501411aad45ea787bc9152bfd3))
- *(runtime)* Warn about missing GL only when the package links it - ([2f9904c](https://github.com/QaidVoid/onelf/commit/2f9904c9a97e6042619aa8ce2623d23f8963beca))
- Treat libGLESv1_CM as a glvnd front end like libGLESv2 - ([984290c](https://github.com/QaidVoid/onelf/commit/984290c1b7a8eefee40f5dabf1f1dfeb81909ea4))
- Make the capability CI jobs pass on Ubuntu 26.04 runners ([#62](https://github.com/QaidVoid/onelf/pull/62)) - ([ba10af3](https://github.com/QaidVoid/onelf/commit/ba10af3ea41060f4b42c5f720e227c8ad72dedcd))
- Make source stamps and FUSE mount reclaim race-free - ([aa56aad](https://github.com/QaidVoid/onelf/commit/aa56aad5dc1a4c33df62cd46d48991d39dc6ec49))
- Preserve plugin subdirs and carry library data ([#59](https://github.com/QaidVoid/onelf/pull/59)) ([#60](https://github.com/QaidVoid/onelf/pull/60)) - ([16e1936](https://github.com/QaidVoid/onelf/commit/16e19369f37124235b60fa6b236c5a463ad7f92d))

### 🚜 Refactor

- *(bundle)* Share plugin enqueueing and data carries - ([3509477](https://github.com/QaidVoid/onelf/commit/35094776b4c90e7f4938f4b71f9f062f11c0ff14))
- *(bundle)* Remove dead code and align translation tests - ([be27032](https://github.com/QaidVoid/onelf/commit/be27032fe13bc503cabf6de2879d5582b08fad1a))
- *(env)* Share one library search path between launches - ([a62597e](https://github.com/QaidVoid/onelf/commit/a62597e00fa04146f714eda3e59cde937afc8268))
- *(onelf)* Extract assembly request and report planning - ([91558c5](https://github.com/QaidVoid/onelf/commit/91558c5ff64cfc8785c7a949e2f406a050345eef))
- Drop stale dead-code allows and unused FUSE structs - ([a11dab9](https://github.com/QaidVoid/onelf/commit/a11dab984bcb31c9618f61aa4efbb816fc062c8b))

### 📚 Documentation

- *(rt)* Note that nvidia-settings itself loads its GTK libraries - ([cacfeb9](https://github.com/QaidVoid/onelf/commit/cacfeb903d9c5bbe1f3247ded0b130fbdb3f5f51))

### 🎨 Styling

- *(rt)* Use ascii in fuse module docs - ([1610a5b](https://github.com/QaidVoid/onelf/commit/1610a5bdb9aff42c7eb44570f7af40e2e28eaa3c))

### 🧪 Testing

- *(distro)* Add framework and audio matrix CI fixtures - ([fd10ce4](https://github.com/QaidVoid/onelf/commit/fd10ce4d48a3990eef2f2facc35e2b20c736460c))
- *(fixtures)* Run offline as the user on foreign bare hosts - ([cb438fb](https://github.com/QaidVoid/onelf/commit/cb438fb48f807124929716c86e8ed8bdc38c8e9d))
- *(format)* Run rules-toml tests and use canonical rule files - ([c17659a](https://github.com/QaidVoid/onelf/commit/c17659a7796d45eddbc471fdd3fda109cd1e5208))
- *(onelf)* Run emission tests in-crate and drop the source mirror - ([937e96d](https://github.com/QaidVoid/onelf/commit/937e96d1eeef5ba5f949ba3ba19a6df9ad161c58))
- *(pipeline)* Launch the synthetic framework end to end - ([36ae83b](https://github.com/QaidVoid/onelf/commit/36ae83b14495c8c065e830cf5e8d5479195f1d03))
- *(pipeline)* Cover local rules, disables and prepared pins - ([31f039f](https://github.com/QaidVoid/onelf/commit/31f039ffe19a64cd99e9ff7e76a503d720815a29))
- *(pipeline)* Expect no library path in a dev run with sets - ([53bfa43](https://github.com/QaidVoid/onelf/commit/53bfa434a41685c5574f9bf0e4878e13440ad6fb))
- *(pipeline)* Find the Ubuntu gst-plugin-scanner location - ([f45ed95](https://github.com/QaidVoid/onelf/commit/f45ed95f168f42f1a866da6dd2867a1711029067))
- *(pipeline)* Cover dev leases, import aliases and stale sets - ([b99edc7](https://github.com/QaidVoid/onelf/commit/b99edc714b278ea7aeb30d3670f585f6153e3f67))
- *(pipeline)* Find the prepared view directory, not its lock - ([c454e30](https://github.com/QaidVoid/onelf/commit/c454e30a63030a0f940b41335b115bc204757bfe))
- *(pipeline)* Add media stack functional checks - ([525d7ec](https://github.com/QaidVoid/onelf/commit/525d7ec35bd92c3cab68bb2049505ad096e0771d))
- *(pipeline)* Add scale fixtures with read counters - ([9cb69c1](https://github.com/QaidVoid/onelf/commit/9cb69c1d038a59c5b290db3b82bd33d74e2c0276))
- *(pipeline)* Lock dev and packed launch parity - ([0f92af6](https://github.com/QaidVoid/onelf/commit/0f92af6c4ab2aea9217fdfbee6e47604bef8e7b3))
- *(pipeline)* Add concurrent selection-view fixtures - ([49bf983](https://github.com/QaidVoid/onelf/commit/49bf983da6a487a67d72b298ce0b0eeebd4ba5cb))
- *(pipeline)* Add privileged root-view regressions - ([9c0477d](https://github.com/QaidVoid/onelf/commit/9c0477def58f586e5fa98138f4493a90326d59c0))
- *(pipeline)* Verify cross-directory init and build - ([8e7fc2c](https://github.com/QaidVoid/onelf/commit/8e7fc2cc9abae2aac60ad42fe8fd36b269bd9891))
- *(rt)* Find the built runtime in Cargo's newer build layout - ([81e0eec](https://github.com/QaidVoid/onelf/commit/81e0eec6c0ccb85ed676b026d5e6bf5067eae975))
- Close the pipeline races a slow runner exposes - ([2798f5e](https://github.com/QaidVoid/onelf/commit/2798f5e0e17297995838e63dca44770bee4d91f9))
- Accept a merged ALSA plugin view that serves the package's - ([943f2a0](https://github.com/QaidVoid/onelf/commit/943f2a00a27635df4e25b614aafcd0cb903788ad))
- Expect the restricted-namespace notice where namespaces fail - ([024697e](https://github.com/QaidVoid/onelf/commit/024697e555afbe129e59e8257a15b91285968a75))
- Prove framework resources come from the package - ([0b1e1c5](https://github.com/QaidVoid/onelf/commit/0b1e1c5128da5a10bcbe8000678f162c8ac2e134))
- Sample peak memory only after the child has exec'd - ([116ffd6](https://github.com/QaidVoid/onelf/commit/116ffd6d28b8fd74ace837ab009af7e81c3d01dd))
- Add pinned distro container fixtures - ([dd01e1c](https://github.com/QaidVoid/onelf/commit/dd01e1cf83b62e61140b7965e515676081a9483f))

### ⚙️ Miscellaneous Tasks

- Add capability preflight and required tests - ([4726c30](https://github.com/QaidVoid/onelf/commit/4726c302ffee5154fe761667ad325758bbbaf501))

## [0.5.1](https://github.com/QaidVoid/onelf/compare/0.5.0...0.5.1) - 2026-10-31

### ⛰️  Features

- *(recipe)* Pin dependency sets without a sysroot - ([1a2c8ad](https://github.com/QaidVoid/onelf/commit/1a2c8ad50561a64f17217ef2f13e1bac29f9dede))

### 🧪 Testing

- Remove each test's scratch dir however it ends - ([6eef501](https://github.com/QaidVoid/onelf/commit/6eef5010d5753f4bb43730a3f881cc884ceec302))

## [0.5.0](https://github.com/QaidVoid/onelf/compare/0.4.2...0.5.0) - 2026-10-29

### ⛰️  Features

- *(rt)* [**breaking**] Delta-update compressed stipple payloads - ([57df6b1](https://github.com/QaidVoid/onelf/commit/57df6b132f36be891a481c591bb0a52792db9269))
- [**breaking**] Replace zsync with direct-release delta self-updates ([#41](https://github.com/QaidVoid/onelf/pull/41)) - ([cd99efb](https://github.com/QaidVoid/onelf/commit/cd99efbe9463d0d80b23937968177d0c980843e0))
- Integrate every entrypoint that has a menu entry - ([08a39bd](https://github.com/QaidVoid/onelf/commit/08a39bded09cd39816368b40ec5070d1dcff8208))

### 🐛 Bug Fixes

- *(rt)* Farm what the host's DRI driver modules need - ([1f12826](https://github.com/QaidVoid/onelf/commit/1f12826072f5c6feef8a91f19454860aabaf1169))
- *(rt)* Farm the host's GLX vendor libraries - ([1db8d66](https://github.com/QaidVoid/onelf/commit/1db8d663d5823db0a03bbf50ba94ae40f90717fc))

### ⚡ Performance

- *(pack)* Stop training a stipple dictionary - ([9d94dc8](https://github.com/QaidVoid/onelf/commit/9d94dc8b2c395803ea80d079065f6abb8ebd37f5))

## [0.4.2](https://github.com/QaidVoid/onelf/compare/0.4.1...0.4.2) - 2026-10-21

### ⛰️  Features

- Add a stipplefs payload with --payload stipple - ([1512eca](https://github.com/QaidVoid/onelf/commit/1512eca9d7ee6c3558b7476ab81e0b37a7ad6333))
- Add --prefer-cache to run from the persistent cache first - ([20f0e6c](https://github.com/QaidVoid/onelf/commit/20f0e6caad37ca33dcc86d37c9b5f4fa5c69fa86))

### 🐛 Bug Fixes

- *(rt)* Pass the entrypoint's tree path as argv[0] - ([be446cc](https://github.com/QaidVoid/onelf/commit/be446cc7ac2abd1d29112edf9177c4183314fa9a))

### 🧪 Testing

- *(rt)* Use a shell, not sleep, in the mapped-tree test - ([2f40f80](https://github.com/QaidVoid/onelf/commit/2f40f808c92ad34084be2a304810f598107b7ebe))

### ⚙️ Miscellaneous Tasks

- Update Cargo.toml dependencies - ([0000000](https://github.com/QaidVoid/onelf/commit/0000000))

## [0.4.1](https://github.com/QaidVoid/onelf/compare/0.4.0...0.4.1) - 2026-10-17

### 🧪 Testing

- Retry reclaiming a killed shared mount until its group is gone - ([b55fbd5](https://github.com/QaidVoid/onelf/commit/b55fbd5970b5a7da31ad7052e34b7d6e515e1fda))

## [0.4.0](https://github.com/QaidVoid/onelf/compare/0.3.3...0.4.0) - 2026-10-17

### ⛰️  Features

- *(bundle)* Find dlopen'd libraries by running the target - ([8d5763b](https://github.com/QaidVoid/onelf/commit/8d5763b753a2e111c77fae7f1f73fd6d1c80cc07))
- *(bundle)* Bring glibc's charset converters with libc - ([edebb21](https://github.com/QaidVoid/onelf/commit/edebb21c237a44630a3559ac124950ea73b37a9f))
- *(bundle)* Take the GPU stack from the host unless asked to bundle it - ([5acb29d](https://github.com/QaidVoid/onelf/commit/5acb29dd983c913001d5a12c98345f62797b8c8b))
- *(bundle)* Record a sysroot build's provenance - ([275b4a2](https://github.com/QaidVoid/onelf/commit/275b4a2c0b1c793e147285ff24a9637638546b90))
- *(bundle)* Take the closure from a pinned sysroot - ([d04162b](https://github.com/QaidVoid/onelf/commit/d04162bad476677625cdccfa5aaeb54c95171939))
- *(format)* Encode host-libs policy and cache request in flags - ([58e9347](https://github.com/QaidVoid/onelf/commit/58e934764020bd261d21844bbe81fe7fd51a8727))
- *(integrate)* Add desktop actions and install every icon size - ([6327365](https://github.com/QaidVoid/onelf/commit/632736501317225e158f64bcaad4741e78efed8d))
- *(onelf)* Carry the GL build pin into packages - ([275697b](https://github.com/QaidVoid/onelf/commit/275697b5a88fda4fbefb168517ba8758772eebf9))
- *(pack)* Make the payload block size configurable - ([581728d](https://github.com/QaidVoid/onelf/commit/581728df2fa903feb0cddd361967ff4e7494b0d1))
- *(recipe)* Accept an https URL as the sysroot archive - ([8d25a74](https://github.com/QaidVoid/onelf/commit/8d25a74f0ae0ce877299c8254cedc5cdebd17c38))
- *(resolver)* Report the GL situation and index a fetched build - ([f177ff2](https://github.com/QaidVoid/onelf/commit/f177ff2b6a2a165fd30ba6c6c9a21652253d8aec))
- *(rt)* Share one FUSE mount between concurrent launches - ([900a7a9](https://github.com/QaidVoid/onelf/commit/900a7a9a60c8aab072529d647718805611f77787))
- *(rt)* Let a package extract, mount and diagnose itself - ([1672e9a](https://github.com/QaidVoid/onelf/commit/1672e9acfabed7d9eefdff4bd741d462ee9ebb7a))
- *(rt)* Reach host and set libraries without LD_LIBRARY_PATH - ([7d275e7](https://github.com/QaidVoid/onelf/commit/7d275e734d224bc0266051d717a7e60e38de037d))
- *(rt)* Stage state privately and find package fonts - ([c6f169b](https://github.com/QaidVoid/onelf/commit/c6f169b04ef8d7c4c482773e03d295dbfb828a4b))
- *(rt)* Serve a dependency set from its build instead of copying it - ([21797eb](https://github.com/QaidVoid/onelf/commit/21797ebc6d318cdba40ba6e4f4414b63e8708bf0))
- *(rt)* Fetch and use the dependency sets a package pins - ([73503b5](https://github.com/QaidVoid/onelf/commit/73503b5c9118c2b801e5c1b7aa132f11d4fd2709))
- *(rt)* Record a trace of opened files under ONELF_TRACE - ([e0b9381](https://github.com/QaidVoid/onelf/commit/e0b938147d28ec46d91e07afd3cc047dd2fc2dae))
- *(rt)* Fetch the pinned GL build when the host has none - ([25abda3](https://github.com/QaidVoid/onelf/commit/25abda398c10692c9a7d75e9bfa1dbe98abb11f7))
- *(rt)* Add the runtime-directory mode and make the cache opt-in - ([af70e21](https://github.com/QaidVoid/onelf/commit/af70e2163b8207f5f5c2a0fb23e1a6e3b3c19993))
- *(rt)* Reclaim dead helper mounts on the next launch - ([1817e36](https://github.com/QaidVoid/onelf/commit/1817e368b2398b6597c5e234dde12d89fd0ec8c2))
- *(rt)* Run under the host loader when its glibc is newer - ([1adf25c](https://github.com/QaidVoid/onelf/commit/1adf25cd5ffc276f3783c6219cdc3e900ae7ac61))
- *(rt)* Choose host libraries per soname at launch - ([29856c6](https://github.com/QaidVoid/onelf/commit/29856c6c827f5598d5da8199b1d4cdc48dee1196))
- *(rt)* Fall back to setuid fusermount and honour FUSERMOUNT_PROG - ([c2943e4](https://github.com/QaidVoid/onelf/commit/c2943e4f1bcfea127d84b4cbedfdd7c982f83726))
- *(rt)* Name the bundled Qt plugin directories - ([5183778](https://github.com/QaidVoid/onelf/commit/5183778c330bad6737fee24ee7d3c4dd6c47c957))
- *(run)* Resolve host libraries like the packed runtime - ([f2a9573](https://github.com/QaidVoid/onelf/commit/f2a95735792375fccd8aa3791344dbd07d52210c))
- *(sysroot)* A set build records what it ships - ([ba91be2](https://github.com/QaidVoid/onelf/commit/ba91be226742d00f2f15f4f9695f18e6bcc2973a))
- *(sysroot)* Shared dependency sets, packer and recipe side - ([e72b17a](https://github.com/QaidVoid/onelf/commit/e72b17ab6f130f5e40a27727b0c58af9edd7a510))
- *(sysroot)* Pin the sysroot archive by BLAKE3 hash - ([717afbf](https://github.com/QaidVoid/onelf/commit/717afbfe279997c11160794247f7efc426003d18))
- *(sysroot)* Keep globs, whole Python packages, host packages - ([6defc19](https://github.com/QaidVoid/onelf/commit/6defc19b25a62cca99fe8a806511f836a7b93959))
- *(sysroot)* Build a GL build's tree from named packages - ([52491bb](https://github.com/QaidVoid/onelf/commit/52491bb8c3c34baabcddd89bf638a0643177806e))
- *(sysroot)* Read pacman databases, closures, archives and prune - ([0906bd8](https://github.com/QaidVoid/onelf/commit/0906bd8ff478d2cc1d00de2dc1179fa04a28ff1b))
- *(sysroot)* Read the GL build pin from platform.toml - ([7808703](https://github.com/QaidVoid/onelf/commit/7808703fe24bd50576bc75088d0071092b49d239))
- *(update)* Sign updates and authenticate packages (format 2) - ([a5803a3](https://github.com/QaidVoid/onelf/commit/a5803a3905c9f5dedbeee558359a05cf4277d8d7))
- Prefetch the blocks a startup profile lists - ([822a654](https://github.com/QaidVoid/onelf/commit/822a65498e95f01c26a4808444e8c891731fc29a))
- Point plugin, data and locale lookups at the bundle - ([e4f122d](https://github.com/QaidVoid/onelf/commit/e4f122d13c21190aaea70d0576180f11624cf2f3))
- Give host programs the caller's environment back - ([32cf581](https://github.com/QaidVoid/onelf/commit/32cf5812bfaa87946d2f3873dc09ffe77d4dd631))
- Make the bundle answer for a compiled-in path - ([723801f](https://github.com/QaidVoid/onelf/commit/723801f4f0ab7995ea72c7e2292e226517a02ea8))

### 🐛 Bug Fixes

- *(build)* Validate the toolchain and end a skip() recursion - ([f4cf395](https://github.com/QaidVoid/onelf/commit/f4cf39561c9db28547c1943b9fd20c630ced6e03))
- *(bundle)* Sandbox traced runs and keep rewrites in the tree - ([ef8a61a](https://github.com/QaidVoid/onelf/commit/ef8a61a27550a71e56b805e07876b9a32fcb8974))
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
- *(rt)* Probe user namespaces and launch chosen entrypoints - ([8f98189](https://github.com/QaidVoid/onelf/commit/8f981890cffbd9839892dabb45d8d4e3733625f4))
- *(rt)* Isolate rundir trees, mounts and inherited state - ([28f1787](https://github.com/QaidVoid/onelf/commit/28f1787161b6a9a68a9b691af39048b9435dba1d))
- *(rt)* Trace the app's opens and end orphaned set servers - ([4592eef](https://github.com/QaidVoid/onelf/commit/4592eef7a2896b0aef7c9dd71a5127e2bedf8e4f))
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
- *(sysroot)* Keep a link to a directory the trace keeps files in - ([128651a](https://github.com/QaidVoid/onelf/commit/128651a390c2f03acb1441012ee3839e91de0598))
- *(sysroot)* Confine reads to the sysroot and stop on gaps - ([395ed2b](https://github.com/QaidVoid/onelf/commit/395ed2b0cb0f84fd075a7ea95303998abb4e4250))
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
- Build a recipe named by a bare file name - ([5390f98](https://github.com/QaidVoid/onelf/commit/5390f98d21442c857105f203711bb09acc381b2b))
- Tighten memfd env, update checks, recipes and messages - ([f76f7cf](https://github.com/QaidVoid/onelf/commit/f76f7cf7681e30ca6ec5137ad80c5cfe26d7e96e))
- Guard update keys, manifests, verify and desktop files - ([69bd353](https://github.com/QaidVoid/onelf/commit/69bd353688b893712c4b17c9483bd599aa09ad35))
- Re-judge the resolver, desktop patching and FUSE access - ([be59c04](https://github.com/QaidVoid/onelf/commit/be59c04f25c1539abb4f549ad5be64553a92fb08))
- Harden pack reads and the FUSE server - ([47dc21f](https://github.com/QaidVoid/onelf/commit/47dc21f5748b9803a93b552e5820063ebf470415))
- Follow release redirects and let host mounts reach apps - ([dc80991](https://github.com/QaidVoid/onelf/commit/dc809917db9f23da844a4d46351e4d7241478578))

### 🚜 Refactor

- *(bundle)* Keep one RUNPATH rewriter - ([f2c10b0](https://github.com/QaidVoid/onelf/commit/f2c10b0dfc850bf965244fc83a77261f40271bc5))
- *(format)* Drop the unused sharun compatibility flag - ([7a0929e](https://github.com/QaidVoid/onelf/commit/7a0929e56e70e30afcd2ee1e5a607c9bf6711429))

### 📚 Documentation

- Check every page against the code and fix quick start - ([07951b2](https://github.com/QaidVoid/onelf/commit/07951b2625169bd9f79df3bd6418f8f1eb5f4d6e))

### ⚡ Performance

- *(rt)* Negotiate FUSE_MAX_PAGES so a read covers a whole block - ([c05dd67](https://github.com/QaidVoid/onelf/commit/c05dd6723a7f6ddd27d24d8aefaac988cf86c05f))
- Default to 64 KiB payload blocks - ([6b08951](https://github.com/QaidVoid/onelf/commit/6b089510be493d2c540f24156b349993f4220083))
- Default to zstd 19, measured 6% smaller - ([43ac48d](https://github.com/QaidVoid/onelf/commit/43ac48dc64a410217050fab6d2c1af1706606559))

### 🎨 Styling

- Fmt - ([b575fb5](https://github.com/QaidVoid/onelf/commit/b575fb58ebcf8b083c239912e6f99391dca37d3a))

### 🧪 Testing

- *(sysroot)* Remove temp roots when a test ends - ([1f100fe](https://github.com/QaidVoid/onelf/commit/1f100fe357a25082e5b91f25afa5ab576ea90653))
- Keep fixture links on toolchains that default to as-needed - ([4ed5de7](https://github.com/QaidVoid/onelf/commit/4ed5de7f18bafb39b827aca0a4a98e4ab2b9460e))
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
