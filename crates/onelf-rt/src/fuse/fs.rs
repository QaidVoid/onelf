//! FUSE filesystem operations.
//!
//! Implements the low-level FUSE ops (lookup, getattr, readdir, read, etc.)
//! against the in-memory manifest. File reads decompress payload blocks on
//! demand with a per-inode block cache.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io;
use std::io::Write;
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Instant;

use onelf_format::Manifest;
use onelf_format::entry::EntryKind;
use rustix::event::{PollFd, PollFlags, poll};

use crate::loader;

use super::protocol::*;

fn entry_to_inode(entry_idx: usize) -> u64 {
    entry_idx as u64 + 1
}

fn inode_to_entry(inode: u64) -> usize {
    (inode - 1) as usize
}

fn build_children(manifest: &Manifest) -> Vec<Vec<u64>> {
    let n = manifest.entries.len();
    let mut children = vec![Vec::new(); n + 1];
    for (idx, entry) in manifest.entries.iter().enumerate() {
        if entry.parent != u32::MAX {
            let parent_inode = entry_to_inode(entry.parent as usize);
            children[parent_inode as usize].push(entry_to_inode(idx));
        }
    }
    children
}

fn make_attr(inode: u64, entry: &onelf_format::entry::Entry, manifest: &Manifest) -> FuseAttr {
    let mode = match entry.kind {
        EntryKind::Dir => 0o040000 | (entry.mode & 0o7777),
        EntryKind::File => 0o100000 | (entry.mode & 0o7777),
        EntryKind::Symlink => 0o120000 | 0o777,
    };
    let nlink = match entry.kind {
        EntryKind::Dir => 2,
        _ => 1,
    };
    let size = match entry.kind {
        EntryKind::File => entry.blocks.iter().map(|b| b.original_size).sum::<u64>(),
        EntryKind::Symlink => manifest.get_string(entry.symlink_target).len() as u64,
        EntryKind::Dir => 0,
    };
    FuseAttr {
        ino: inode,
        size,
        blocks: size.div_ceil(512),
        atime: entry.mtime_secs,
        mtime: entry.mtime_secs,
        ctime: entry.mtime_secs,
        atimensec: entry.mtime_nsec,
        mtimensec: entry.mtime_nsec,
        ctimensec: entry.mtime_nsec,
        mode,
        nlink,
        uid: rustix::process::getuid().as_raw(),
        gid: rustix::process::getgid().as_raw(),
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

const CACHE_IDLE_SECS: u64 = 2;

/// How long a detached server waits before asking, the first time, whether
/// anything is still using the mount. Long enough for a process that
/// daemonizes to have forked, short enough that a package nothing held on to
/// is reclaimed promptly.
const DETACHED_GRACE_SECS: u64 = 1;

/// How long a detached server waits between later checks. By then something
/// is using the mount, and the answer only changes when that process exits,
/// so this trades how promptly the mount is reclaimed against how often the
/// check runs for nothing.
const DETACHED_PROBE_SECS: u64 = 5;

/// Default ceiling on decompressed blocks held in memory.
///
/// At the packer's default 64 KiB block size this is 512 blocks, far more
/// than the reads in flight need, while keeping the server itself from
/// becoming the memory problem it exists to avoid. The packer caps
/// `--block-size` at this budget so a single block always fits.
const DEFAULT_CACHE_BUDGET: usize = 32 * 1024 * 1024;

/// Read the cache ceiling from `ONELF_FUSE_CACHE_BYTES`, falling back to
/// [`DEFAULT_CACHE_BUDGET`] when unset or unparseable.
fn cache_budget() -> usize {
    std::env::var("ONELF_FUSE_CACHE_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(DEFAULT_CACHE_BUDGET)
}

type BlockKey = (u64, usize); // (inode, block_index)

/// Decompressed blocks held for reuse, bounded by total bytes.
///
/// A sustained sequential read never goes idle, so clearing on an idle timer
/// alone let the whole file accumulate here. The byte budget is the real
/// bound; the idle clear is kept because it returns memory to the OS when an
/// app goes quiet, which a budget alone does not do.
struct BlockCache {
    data: HashMap<BlockKey, Vec<u8>>,
    /// Insertion order, oldest first, used to pick an eviction victim.
    order: VecDeque<BlockKey>,
    bytes: usize,
    budget: usize,
    last_access: Option<Instant>,
}

impl BlockCache {
    fn new(budget: usize) -> Self {
        Self {
            data: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            budget,
            last_access: None,
        }
    }

    fn get_block(&self, inode: u64, block_index: usize) -> Option<&[u8]> {
        self.data.get(&(inode, block_index)).map(|v| v.as_slice())
    }

    /// Insert a block, evicting older ones to stay within budget.
    ///
    /// `pinned` names blocks of `inode` that the read in progress will read
    /// back, and which therefore must survive regardless of the budget.
    /// Without that, a read spanning more blocks than the budget holds, or
    /// another block landing on a full cache, evicts a block that is about to
    /// be borrowed.
    fn insert_block(&mut self, inode: u64, block_index: usize, content: Vec<u8>, pinned: &[usize]) {
        self.last_access = Some(Instant::now());
        let key = (inode, block_index);
        if let Some(old) = self.data.remove(&key) {
            self.bytes -= old.len();
            self.order.retain(|k| *k != key);
        }
        // Evict oldest-first until the newcomer fits, skipping anything the
        // caller pinned. A block larger than the whole budget still goes in,
        // because refusing it would leave its read unservable.
        let mut skipped: Vec<BlockKey> = Vec::new();
        while self.bytes + content.len() > self.budget {
            let Some(victim) = self.order.pop_front() else {
                break;
            };
            if victim.0 == inode && pinned.contains(&victim.1) {
                skipped.push(victim);
                continue;
            }
            if let Some(dropped) = self.data.remove(&victim) {
                self.bytes -= dropped.len();
            }
        }
        // Pinned blocks keep their place at the front, so they are the first
        // to go once the read that needed them is done.
        for k in skipped.into_iter().rev() {
            self.order.push_front(k);
        }
        self.bytes += content.len();
        self.order.push_back(key);
        self.data.insert(key, content);
    }

    /// Clear everything once nothing was inserted for [`CACHE_IDLE_SECS`],
    /// and say whether it did.
    fn maybe_clear(&mut self) -> bool {
        if let Some(last) = self.last_access
            && last.elapsed().as_secs() >= CACHE_IDLE_SECS
        {
            self.data.clear();
            self.data.shrink_to_fit();
            self.order.clear();
            self.bytes = 0;
            self.last_access = None;
            return true;
        }
        false
    }

    fn is_active(&self) -> bool {
        self.last_access.is_some()
    }
}

/// Records the sysroot-space path of every file the app looks up or
/// opens, into the file named by `ONELF_TRACE`. The trace tier reads
/// that list back to prune what a run never touched, so this is how a
/// trace is captured: run the packed app under `ONELF_TRACE=trace.txt`
/// through the paths that matter, then name the file in the recipe.
struct TraceRecorder {
    file: File,
    seen: HashSet<usize>,
    /// Set once the launcher looks up [`TRACE_ARM`], just before it
    /// hands over to the app. Until then the reads are the runtime's.
    armed: bool,
}

/// The name the launcher looks up in the mount's root to start a trace.
/// No package entry has it: the lookup arms the recorder and fails.
pub(crate) const TRACE_ARM: &str = ".onelf-trace-arm";

impl TraceRecorder {
    /// A recorder when `ONELF_TRACE` names a writable file, else none.
    /// The file is appended, so several runs accumulate into one trace.
    fn from_env() -> Option<Self> {
        let path = std::env::var_os("ONELF_TRACE").filter(|v| !v.is_empty())?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()?;
        Some(Self {
            file,
            seen: HashSet::new(),
            armed: false,
        })
    }

    /// Write entry `idx`'s sysroot path once. Directories are skipped:
    /// only files and symlinks appear in a package database's file list.
    fn record(&mut self, idx: usize, manifest: &Manifest) {
        if !self.armed || !self.seen.insert(idx) {
            return;
        }
        let entry = &manifest.entries[idx];
        if entry.kind != EntryKind::File && entry.kind != EntryKind::Symlink {
            return;
        }
        if let Some(sysroot) = appdir_to_sysroot(&manifest.entry_path(idx)) {
            let _ = writeln!(self.file, "/{sysroot}");
        }
    }
}

/// Map an AppDir-relative path back to the sysroot path the trace tier
/// matches against, and drop the runtime's own `.onelf/` bookkeeping.
///
/// The sysroot copier strips `usr/` from every path under it, so any
/// top-level directory may have come from there: `lib32/` and `libexec/`
/// as much as `bin/` and `share/`. Only the directories a root
/// filesystem keeps beside `usr/`, never inside it, were copied as they
/// were and are already in sysroot space.
fn appdir_to_sysroot(path: &str) -> Option<String> {
    if path.starts_with(".onelf") {
        return None;
    }
    const BESIDE_USR: &[&str] = &[
        "etc", "opt", "var", "srv", "boot", "home", "media", "mnt", "root", "run", "tmp",
    ];
    let top = path.split('/').next().unwrap_or(path);
    if BESIDE_USR.contains(&top) {
        Some(path.to_string())
    } else {
        Some(format!("usr/{path}"))
    }
}

/// Records each block a read touches for the first time into the file
/// named by `ONELF_PROFILE`, as `path<TAB>offset` in the order the reads
/// came. Unlike a trace it starts with the mount, since what the runtime
/// reads before the app starts is part of every launch too. The packer
/// turns such a profile into [`PREFETCH_FILE`] for [`Prefetch`].
struct ProfileRecorder {
    file: File,
    seen: HashSet<BlockKey>,
}

impl ProfileRecorder {
    /// A recorder when `ONELF_PROFILE` names a writable file, else none.
    /// The file is appended, so several runs accumulate into one profile.
    fn from_env() -> Option<Self> {
        let path = std::env::var_os("ONELF_PROFILE").filter(|v| !v.is_empty())?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()?;
        Some(Self {
            file,
            seen: HashSet::new(),
        })
    }

    fn record(&mut self, manifest: &Manifest, entry_idx: usize, block: usize, offset: usize) {
        if self.seen.insert((entry_to_inode(entry_idx), block)) {
            let _ = writeln!(self.file, "{}\t{offset}", manifest.entry_path(entry_idx));
        }
    }
}

/// The blocks a startup reads, as the packer recorded them from a profile:
/// one `path<TAB>block` line each, in the order they were first read.
pub(crate) const PREFETCH_FILE: &str = ".onelf/prefetch";

/// Decompressed bytes the prefetch may hold that no read has taken yet.
///
/// This bounds how far it runs ahead of the app, and so what it costs in
/// memory when the profile no longer matches what the app does. On a
/// Blender startup this much hid nearly all of the decompression, and
/// twice as much gained a few percent at most.
const PREFETCH_WINDOW: usize = 32 * 1024 * 1024;

/// Most worker threads the prefetch runs. Each decompresses one block at a
/// time, so this is how many cores it takes from the app at most. One core
/// is always left for the app and this server.
const PREFETCH_THREADS: usize = 4;

/// Package-relative paths resolved against the tree one component at a
/// time. A directory's names are indexed the first time a path passes
/// through it, since a profile names hundreds of files in the same few
/// directories.
struct PathIndex<'m> {
    manifest: &'m Manifest,
    children: &'m [Vec<u64>],
    dirs: HashMap<u64, HashMap<&'m str, u64>>,
}

impl<'m> PathIndex<'m> {
    fn new(manifest: &'m Manifest, children: &'m [Vec<u64>]) -> Self {
        Self {
            manifest,
            children,
            dirs: HashMap::new(),
        }
    }

    fn entry(&mut self, path: &str) -> Option<usize> {
        let (manifest, children) = (self.manifest, self.children);
        let mut inode = entry_to_inode(0);
        for name in path.split('/') {
            let names = self.dirs.entry(inode).or_insert_with(|| {
                children[inode as usize]
                    .iter()
                    .map(|&c| {
                        (
                            manifest.get_string(manifest.entries[inode_to_entry(c)].name),
                            c,
                        )
                    })
                    .collect()
            });
            inode = *names.get(name)?;
        }
        Some(inode_to_entry(inode))
    }
}

/// The blocks [`PREFETCH_FILE`] lists, in its order. Lines naming nothing
/// in this package are skipped: the profile is a hint, never a reason to
/// refuse a launch.
fn profiled_blocks(
    manifest: &Manifest,
    children: &[Vec<u64>],
    file: &mut File,
    footer: &onelf_format::Footer,
    dict: Option<&[u8]>,
) -> Vec<(BlockKey, onelf_format::Block)> {
    let mut index = PathIndex::new(manifest, children);
    let Some(idx) = index.entry(PREFETCH_FILE) else {
        return Vec::new();
    };
    let Ok(data) = loader::read_verified_entry(file, footer, &manifest.entries[idx], dict) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&data);
    let mut seen = HashSet::new();
    let mut blocks = Vec::new();
    for line in text.lines() {
        let Some((path, block)) = line.split_once('\t') else {
            continue;
        };
        let (Some(idx), Ok(block)) = (index.entry(path), block.parse::<usize>()) else {
            continue;
        };
        let Some(b) = manifest.entries[idx].blocks.get(block) else {
            continue;
        };
        let key = (entry_to_inode(idx), block);
        if seen.insert(key) {
            blocks.push((key, b.clone()));
        }
    }
    blocks
}

/// Decompresses the blocks a startup profile lists, in its order, on
/// worker threads, so the app's reads find them ready instead of waiting
/// on zstd one request at a time.
///
/// Reads never wait on it. A block a read needs before a worker has it is
/// decompressed by the read as always, and the worker's copy is dropped.
/// The workers stop when this is dropped, which has to happen before the
/// process forks: a fork copies only the calling thread, and a lock a
/// worker held would stay held in the copy.
struct Prefetch {
    shared: Arc<Prefetched>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

struct Prefetched {
    blocks: Vec<(BlockKey, onelf_format::Block)>,
    wanted: HashSet<BlockKey>,
    /// Index into `blocks` of the next one a worker takes.
    next: AtomicUsize,
    store: Mutex<Store>,
    /// Signalled when a read takes a block, which makes room in the window.
    room: Condvar,
    window: usize,
}

#[derive(Default)]
struct Store {
    ready: HashMap<BlockKey, Vec<u8>>,
    bytes: usize,
    /// Blocks a read decompressed itself, which a worker must not add.
    taken: HashSet<BlockKey>,
    stop: bool,
}

impl Prefetched {
    fn new(blocks: Vec<(BlockKey, onelf_format::Block)>, window: usize) -> Self {
        Self {
            wanted: blocks.iter().map(|(key, _)| *key).collect(),
            blocks,
            next: AtomicUsize::new(0),
            store: Mutex::new(Store::default()),
            room: Condvar::new(),
            window,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn work(&self, file: &File, footer: &onelf_format::Footer, dict: Option<&[u8]>) {
        loop {
            let Some((key, block)) = self.blocks.get(self.next.fetch_add(1, Ordering::Relaxed))
            else {
                return;
            };
            {
                let mut store = self.lock();
                while !store.stop && store.bytes >= self.window {
                    store = self.room.wait(store).unwrap_or_else(|e| e.into_inner());
                }
                if store.stop {
                    return;
                }
                if store.taken.contains(key) {
                    continue;
                }
            }
            let Ok(data) = loader::read_payload_entry(file, footer, block, dict) else {
                continue;
            };
            let mut store = self.lock();
            if store.stop {
                return;
            }
            if !store.taken.contains(key) && !store.ready.contains_key(key) {
                store.bytes += data.len();
                store.ready.insert(*key, data);
            }
        }
    }
}

impl Prefetch {
    /// Start prefetching `blocks`, unless there are none or no core to
    /// spare for it.
    fn start(
        blocks: Vec<(BlockKey, onelf_format::Block)>,
        file: &File,
        footer: &onelf_format::Footer,
        dict: Option<&[u8]>,
    ) -> Option<Self> {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        let threads = PREFETCH_THREADS.min(cores.saturating_sub(1));
        if threads == 0 || blocks.is_empty() {
            return None;
        }
        let shared = Arc::new(Prefetched::new(blocks, PREFETCH_WINDOW));
        let dict: Option<Arc<[u8]>> = dict.map(Arc::from);
        let mut workers = Vec::with_capacity(threads);
        for _ in 0..threads {
            let Ok(file) = file.try_clone() else {
                break;
            };
            let shared = Arc::clone(&shared);
            let footer = footer.clone();
            let dict = dict.clone();
            let spawned = std::thread::Builder::new()
                .name("onelf-prefetch".into())
                .spawn(move || shared.work(&file, &footer, dict.as_deref()));
            match spawned {
                Ok(worker) => workers.push(worker),
                Err(_) => break,
            }
        }
        if workers.is_empty() {
            return None;
        }
        Some(Self { shared, workers })
    }

    /// The block, when a worker has it ready. Otherwise the caller
    /// decompresses it, and no worker adds it afterwards.
    fn take(&self, key: BlockKey) -> Option<Vec<u8>> {
        if !self.shared.wanted.contains(&key) {
            return None;
        }
        let mut store = self.shared.lock();
        match store.ready.remove(&key) {
            Some(data) => {
                store.bytes -= data.len();
                self.shared.room.notify_one();
                Some(data)
            }
            None => {
                store.taken.insert(key);
                None
            }
        }
    }
}

impl Drop for Prefetch {
    fn drop(&mut self) {
        {
            let mut store = self.shared.lock();
            store.stop = true;
            store.ready.clear();
        }
        self.shared.room.notify_all();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

/// Symlinks served beside the package's own entries: the libraries the
/// launch takes from the host and from dependency sets, shown in the
/// package's library directory (see `crate::links`). Their inodes follow
/// the manifest's, in the order they were added.
#[derive(Default)]
struct Synthetic {
    /// Inode of the first link.
    first: u64,
    links: Vec<SyntheticLink>,
    by_name: HashMap<(u64, String), u64>,
}

struct SyntheticLink {
    parent: u64,
    name: String,
    target: Vec<u8>,
}

impl Synthetic {
    fn new(manifest: &Manifest) -> Self {
        Self {
            first: manifest.entries.len() as u64 + 1,
            ..Self::default()
        }
    }

    fn add(&mut self, parent: u64, name: String, target: Vec<u8>) {
        let key = (parent, name);
        if self.by_name.contains_key(&key) {
            return;
        }
        let inode = self.first + self.links.len() as u64;
        self.by_name.insert(key.clone(), inode);
        self.links.push(SyntheticLink {
            parent,
            name: key.1,
            target,
        });
    }

    fn get(&self, inode: u64) -> Option<&SyntheticLink> {
        let at = inode.checked_sub(self.first)?;
        self.links.get(usize::try_from(at).ok()?)
    }

    fn lookup(&self, parent: u64, name: &str) -> Option<u64> {
        self.by_name.get(&(parent, name.to_string())).copied()
    }

    /// The links in directory `parent`, with their inodes.
    fn children(&self, parent: u64) -> Vec<(u64, &SyntheticLink)> {
        self.links
            .iter()
            .enumerate()
            .filter(|(_, l)| l.parent == parent)
            .map(|(i, l)| (self.first + i as u64, l))
            .collect()
    }

    /// Attributes of the link at `inode`, timestamped like its directory.
    fn attr(&self, inode: u64, link: &SyntheticLink, manifest: &Manifest) -> FuseAttr {
        let dir = &manifest.entries[inode_to_entry(link.parent)];
        let mut attr = make_attr(link.parent, dir, manifest);
        attr.ino = inode;
        attr.mode = 0o120000 | 0o777;
        attr.nlink = 1;
        attr.size = link.target.len() as u64;
        attr.blocks = attr.size.div_ceil(512);
        attr
    }
}

pub struct FuseState<'a> {
    manifest: &'a Manifest,
    file: &'a mut File,
    footer: &'a onelf_format::Footer,
    dict: Option<&'a [u8]>,
    children: Vec<Vec<u64>>,
    cache: BlockCache,
    /// Per-inode verdict of whole-entry verification, used only for
    /// version-1 manifests whose blocks carry no hash of their own.
    verified: HashMap<u64, bool>,
    /// Present when `ONELF_TRACE` asked for a trace of this run.
    recorder: Option<TraceRecorder>,
    /// Present when `ONELF_PROFILE` asked for a profile of this run.
    profile: Option<ProfileRecorder>,
    /// Present while the package's startup profile is being prefetched.
    prefetch: Option<Prefetch>,
    synthetic: Synthetic,
    /// Where the launching child sends the links it resolved, until it has.
    inbox: Option<crate::links::Receiver>,
}

impl<'a> FuseState<'a> {
    pub fn new(
        manifest: &'a Manifest,
        file: &'a mut File,
        footer: &'a onelf_format::Footer,
        dict: Option<&'a [u8]>,
    ) -> Self {
        let children = build_children(manifest);
        let profiled = profiled_blocks(manifest, &children, file, footer, dict);
        let prefetch = Prefetch::start(profiled, file, footer, dict);
        Self {
            manifest,
            file,
            footer,
            dict,
            children,
            cache: BlockCache::new(cache_budget()),
            verified: HashMap::new(),
            recorder: TraceRecorder::from_env(),
            profile: ProfileRecorder::from_env(),
            prefetch,
            synthetic: Synthetic::new(manifest),
            inbox: None,
        }
    }

    /// Serve the links the launching child sends through `inbox`, once it
    /// has, and tell it so. Read while [`FuseState::run_loop`] serves.
    pub fn expect_links(&mut self, inbox: crate::links::Receiver) {
        self.inbox = Some(inbox);
    }

    fn take_links(&mut self) {
        let Some(inbox) = self.inbox.as_mut() else {
            return;
        };
        let crate::links::Received::Done(message) = inbox.read() else {
            return;
        };
        if let Some(message) = message
            && let Some(idx) = crate::links::dir_index(self.manifest, &message.dir)
        {
            for (name, target) in message.links {
                self.synthetic.add(entry_to_inode(idx), name, target);
            }
            inbox.ack();
        }
        self.inbox = None;
    }

    /// Confirm the entry may be served, for manifests that predate
    /// per-block hashes.
    ///
    /// Version 1 offers only a whole-entry hash, so honouring "never serve
    /// unverified bytes" means reassembling the entry once and remembering
    /// the verdict. That costs the size of the largest file, which is why
    /// version 2 records a hash per block; entries carrying one skip this
    /// entirely and are checked as each block is decompressed.
    fn verify_entry(&mut self, inode: u64, entry_idx: usize) -> bool {
        let entry = &self.manifest.entries[entry_idx];
        if entry.blocks.iter().all(|b| b.has_content_hash()) {
            return true;
        }
        if let Some(&ok) = self.verified.get(&inode) {
            return ok;
        }
        let ok = match loader::read_payload_blocks(self.file, self.footer, &entry.blocks, self.dict)
        {
            Ok(data) => blake3::hash(&data).as_bytes() == &entry.content_hash,
            Err(_) => false,
        };
        self.verified.insert(inode, ok);
        ok
    }

    /// Serve the mount until the launched process is done with it.
    ///
    /// Two things end it. `death_pipe` hangs up once every descendant has
    /// closed the inherited descriptor, which is the cleanest signal but only
    /// arrives when the whole tree is gone. `child_exit` fires as soon as the
    /// process that was launched exits, which is what the caller has to return
    /// on: a launcher that waits for a daemon its child started is a launcher
    /// that only returns once that daemon has given up, and reports it dead.
    /// The mount is handed to a background server in that case, so leaving
    /// here does not take the filesystem with it. Either signal may be left
    /// out; the caller passes no death pipe when it has already hung up and
    /// the child turned out to be alive.
    pub fn run_loop(
        &mut self,
        fuse_fd: &impl AsFd,
        death_pipe: Option<&impl AsFd>,
        child_exit: Option<&impl AsFd>,
        buf: &mut [u8],
    ) {
        use rustix::event::Timespec;

        let cache_timeout = Timespec {
            tv_sec: CACHE_IDLE_SECS as i64,
            tv_nsec: 0,
        };

        loop {
            self.clear_if_idle();

            let timeout = if self.cache.is_active() {
                Some(&cache_timeout)
            } else {
                None
            };

            let mut poll_fds = Vec::with_capacity(4);
            poll_fds.push(PollFd::new(fuse_fd, PollFlags::IN));
            if let Some(fd) = death_pipe {
                poll_fds.push(PollFd::new(fd, PollFlags::IN));
            }
            if let Some(fd) = child_exit {
                poll_fds.push(PollFd::new(fd, PollFlags::IN));
            }
            // Last, so the exit signals above stay at their indices.
            if let Some(inbox) = &self.inbox {
                poll_fds.push(PollFd::new(inbox.fd(), PollFlags::IN));
            }
            match poll(&mut poll_fds, timeout) {
                Ok(0) => continue,
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => return,
            }
            let revents: Vec<PollFlags> = poll_fds.iter().map(|p| p.revents()).collect();
            drop(poll_fds);
            let signals = 1 + usize::from(death_pipe.is_some()) + usize::from(child_exit.is_some());

            if revents[1..signals]
                .iter()
                .any(|r| r.intersects(PollFlags::HUP | PollFlags::IN))
            {
                return;
            }

            if revents
                .get(signals)
                .is_some_and(|r| r.intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR))
            {
                self.take_links();
            }

            // An external unmount (`fusermount3 -u -z`) signals ERR/HUP on
            // the FUSE fd, never IN. The connection is gone; polling on is
            // a 100%-core spin, since ERR is always ready.
            if revents[0].intersects(PollFlags::ERR | PollFlags::HUP) {
                return;
            }

            if !revents[0].intersects(PollFlags::IN) {
                continue;
            }

            if self.serve_ready(fuse_fd, buf).is_break() {
                return;
            }
        }
    }

    /// End the prefetch. Its workers do not survive a fork, so a caller
    /// that forks to keep serving calls this first.
    pub fn stop_prefetch(&mut self) {
        self.prefetch = None;
    }

    /// Drop the cached blocks once reads have stopped for a while, and with
    /// them whatever the prefetch still holds: by then the startup it was
    /// for is over.
    fn clear_if_idle(&mut self) {
        if self.cache.maybe_clear() {
            self.prefetch = None;
        }
    }

    /// Serve the mount until `is_idle` says nothing is using it any more.
    ///
    /// Runs after the death pipe has already hung up. That is not proof the
    /// package is finished: a process that daemonizes forks a background copy,
    /// lets the foreground exit, and closes every descriptor it inherited,
    /// which hangs the pipe up while the real work is still running out of the
    /// mount. There is no descriptor left to wait on, so the caller supplies
    /// the liveness test instead.
    pub fn serve_detached(
        &mut self,
        fuse_fd: &impl AsFd,
        buf: &mut [u8],
        mut is_idle: impl FnMut() -> bool,
    ) {
        use rustix::event::Timespec;

        // The first check comes quickly, so a package that nothing held on to
        // is reclaimed almost at once. It cannot be immediate: a process that
        // daemonizes has not necessarily forked yet when the foreground exits,
        // and asking too early sees nothing and tears the mount down under the
        // process about to appear. Later checks are spaced out, because by then
        // something is using the mount and the answer only changes when it goes.
        let mut wait = Timespec {
            tv_sec: DETACHED_GRACE_SECS as i64,
            tv_nsec: 0,
        };

        loop {
            self.clear_if_idle();

            let mut poll_fds = [PollFd::new(fuse_fd, PollFlags::IN)];
            let timed_out = match poll(&mut poll_fds, Some(&wait)) {
                Ok(0) => true,
                Ok(_) => {
                    // An external unmount arrives as ERR/HUP, not IN.
                    // The connection is gone; so is any reason to keep
                    // polling (and, without this, ERR is always ready
                    // and the loop spins a core flat).
                    if poll_fds[0]
                        .revents()
                        .intersects(PollFlags::ERR | PollFlags::HUP)
                    {
                        return;
                    }
                    !poll_fds[0].revents().intersects(PollFlags::IN)
                }
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => return,
            };

            // Nothing asked for anything: the moment to find out whether the
            // mount still has users.
            if timed_out {
                if is_idle() {
                    return;
                }
                wait = Timespec {
                    tv_sec: DETACHED_PROBE_SECS as i64,
                    tv_nsec: 0,
                };
                continue;
            }

            if self.serve_ready(fuse_fd, buf).is_break() {
                return;
            }
        }
    }

    /// Service one readable FUSE request. `Break` means the connection is
    /// finished and the caller should stop serving.
    fn serve_ready(&mut self, fuse_fd: &impl AsFd, buf: &mut [u8]) -> std::ops::ControlFlow<()> {
        use std::ops::ControlFlow::{Break, Continue};

        let n = match rustix::io::read(fuse_fd, &mut *buf) {
            Ok(n) if n >= IN_HEADER_SIZE => n,
            Ok(_) => return Continue(()),
            Err(rustix::io::Errno::INTR) => return Continue(()),
            Err(rustix::io::Errno::NODEV) => return Break(()),
            Err(_) => return Break(()),
        };

        let header: FuseInHeader = unsafe { read_struct(buf, 0).unwrap() };

        if header.opcode == FUSE_DESTROY {
            return Break(());
        }
        if header.opcode == FUSE_FORGET || header.opcode == FUSE_BATCH_FORGET {
            return Continue(());
        }

        if header.opcode == FUSE_READ {
            self.handle_read(fuse_fd, &header, &buf[IN_HEADER_SIZE..n]);
            return Continue(());
        }

        let tree = Tree {
            manifest: self.manifest,
            children: &self.children,
            synthetic: &self.synthetic,
        };
        let response = dispatch(&header, &buf[IN_HEADER_SIZE..n], &tree, &mut self.recorder);
        if !response.is_empty() {
            let _ = rustix::io::write(fuse_fd, &response);
        }
        Continue(())
    }

    fn handle_read(&mut self, fuse_fd: &impl AsFd, header: &FuseInHeader, body: &[u8]) {
        let read_in: FuseReadIn = match unsafe { read_struct(body, 0) } {
            Some(v) => v,
            None => {
                let r = reply_err(header, -libc_einval());
                let _ = rustix::io::write(fuse_fd, &r);
                return;
            }
        };

        let inode = read_in.fh;
        let entry_idx = inode_to_entry(inode);
        if inode == 0 || entry_idx >= self.manifest.entries.len() {
            let r = reply_err(header, -libc_enoent());
            let _ = rustix::io::write(fuse_fd, &r);
            return;
        }

        {
            let entry = &self.manifest.entries[entry_idx];
            if entry.blocks.is_empty() {
                let r = reply_err(header, -libc_eio());
                let _ = rustix::io::write(fuse_fd, &r);
                return;
            }
        }

        // Refuse to serve any bytes of a file whose content does not
        // match its recorded hash.
        if !self.verify_entry(inode, entry_idx) {
            let r = reply_err(header, -libc_eio());
            let _ = rustix::io::write(fuse_fd, &r);
            return;
        }

        let entry = &self.manifest.entries[entry_idx];

        let offset = read_in.offset as usize;
        let size = read_in.size as usize;
        let num_blocks = entry.blocks.len();

        // Build block offset map, stack-allocated for files <=32 blocks (2MB @ 64KB)
        const MAX_STACK: usize = 32;
        let use_stack = num_blocks <= MAX_STACK;
        let mut offsets_stack = [0usize; MAX_STACK];
        let mut offsets_heap = Vec::new();
        let mut total_size: usize = 0;

        if use_stack {
            for (i, block) in entry.blocks.iter().enumerate() {
                offsets_stack[i] = total_size;
                total_size = total_size.saturating_add(block.original_size as usize);
            }
        } else {
            offsets_heap.reserve(num_blocks);
            for block in &entry.blocks {
                offsets_heap.push(total_size);
                total_size = total_size.saturating_add(block.original_size as usize);
            }
        }
        let block_offsets: &[usize] = if use_stack {
            &offsets_stack[..num_blocks]
        } else {
            &offsets_heap
        };

        if offset >= total_size {
            let out_header = FuseOutHeader {
                len: OUT_HEADER_SIZE as u32,
                error: 0,
                unique: header.unique,
            };
            let hdr_bytes = unsafe {
                core::slice::from_raw_parts(
                    &out_header as *const FuseOutHeader as *const u8,
                    OUT_HEADER_SIZE,
                )
            };
            let _ = rustix::io::write(fuse_fd, hdr_bytes);
            return;
        }

        let end = (offset + size).min(total_size);
        let read_len = end - offset;

        // Find overlapping blocks
        let mut needed_stack = [0usize; MAX_STACK];
        let mut needed_heap = Vec::new();
        let mut needed_len = 0;
        for (block_idx, &block_start) in block_offsets.iter().enumerate() {
            let block_end =
                block_start.saturating_add(entry.blocks[block_idx].original_size as usize);
            if offset < block_end && end > block_start {
                if use_stack {
                    needed_stack[needed_len] = block_idx;
                    needed_len += 1;
                } else {
                    needed_heap.push(block_idx);
                }
            }
        }
        let needed_blocks: &[usize] = if use_stack {
            &needed_stack[..needed_len]
        } else {
            &needed_heap
        };

        // Only the blocks this read covers. The kernel already reads ahead
        // for a sequential reader, and decompressing the next block too,
        // before replying, made every scattered read wait for a block it did
        // not ask for. Loading libraries is almost nothing but such reads.
        for &block_idx in needed_blocks {
            if let Some(profile) = self.profile.as_mut() {
                profile.record(
                    self.manifest,
                    entry_idx,
                    block_idx,
                    block_offsets[block_idx],
                );
            }
            if self.cache.get_block(inode, block_idx).is_some() {
                continue;
            }
            let block = &entry.blocks[block_idx];
            let prefetched = self
                .prefetch
                .as_ref()
                .and_then(|p| p.take((inode, block_idx)));
            let data = match prefetched {
                Some(data) => Ok(data),
                None => loader::read_payload_entry(self.file, self.footer, block, self.dict),
            };
            match data {
                Ok(data) => {
                    self.cache
                        .insert_block(inode, block_idx, data, needed_blocks);
                }
                Err(_) => {
                    let r = reply_err(header, -libc_eio());
                    let _ = rustix::io::write(fuse_fd, &r);
                    return;
                }
            }
        }

        // Zero-copy response: writev directly from cached blocks. Every
        // slice is clamped to the block's real decompressed length, and
        // the header length is derived from what is actually written, so
        // a block whose true size differs from its claimed `original_size`
        // can never drive an out-of-bounds slice.
        if needed_blocks.len() == 1 {
            // Fast path: single block (most common)
            let block_idx = needed_blocks[0];
            let Some(block_data) = self.cache.get_block(inode, block_idx) else {
                let r = reply_err(header, -libc_eio());
                let _ = rustix::io::write(fuse_fd, &r);
                return;
            };
            let data_start = (offset - block_offsets[block_idx]).min(block_data.len());
            let data_end = data_start.saturating_add(read_len).min(block_data.len());
            let payload = &block_data[data_start..data_end];
            let out_header = FuseOutHeader {
                len: (OUT_HEADER_SIZE + payload.len()) as u32,
                error: 0,
                unique: header.unique,
            };
            let hdr_bytes = unsafe {
                core::slice::from_raw_parts(
                    &out_header as *const FuseOutHeader as *const u8,
                    OUT_HEADER_SIZE,
                )
            };
            let _ = rustix::io::writev(
                fuse_fd,
                &[io::IoSlice::new(hdr_bytes), io::IoSlice::new(payload)],
            );
        } else {
            // Multi-block: gather clamped slices.
            let mut payloads: Vec<&[u8]> = Vec::with_capacity(needed_blocks.len());
            let mut total = 0usize;
            for &block_idx in needed_blocks {
                let Some(block_data) = self.cache.get_block(inode, block_idx) else {
                    let r = reply_err(header, -libc_eio());
                    let _ = rustix::io::write(fuse_fd, &r);
                    return;
                };
                let block_start = block_offsets[block_idx];
                let slice_start = (offset.max(block_start) - block_start).min(block_data.len());
                let slice_end =
                    (end.min(block_start + block_data.len()) - block_start).min(block_data.len());
                let payload = &block_data[slice_start..slice_end];
                total += payload.len();
                payloads.push(payload);
            }
            let out_header = FuseOutHeader {
                len: (OUT_HEADER_SIZE + total) as u32,
                error: 0,
                unique: header.unique,
            };
            let hdr_bytes = unsafe {
                core::slice::from_raw_parts(
                    &out_header as *const FuseOutHeader as *const u8,
                    OUT_HEADER_SIZE,
                )
            };
            let mut slices = Vec::with_capacity(payloads.len() + 1);
            slices.push(io::IoSlice::new(hdr_bytes));
            for p in &payloads {
                slices.push(io::IoSlice::new(p));
            }
            let _ = rustix::io::writev(fuse_fd, &slices);
        }
    }
}

/// What requests are answered from: the package's entries and the links
/// served beside them.
struct Tree<'t> {
    manifest: &'t Manifest,
    children: &'t [Vec<u64>],
    synthetic: &'t Synthetic,
}

fn dispatch(
    header: &FuseInHeader,
    body: &[u8],
    tree: &Tree,
    recorder: &mut Option<TraceRecorder>,
) -> Vec<u8> {
    let manifest = tree.manifest;
    match header.opcode {
        FUSE_INIT => handle_init(header, body),
        FUSE_LOOKUP => handle_lookup(header, body, tree, recorder),
        FUSE_GETATTR => handle_getattr(header, tree),
        FUSE_OPEN => handle_open(header, manifest, recorder),
        FUSE_RELEASE | FUSE_RELEASEDIR => reply_ok(header, &[]),
        FUSE_OPENDIR => handle_opendir(header, manifest),
        FUSE_READDIR => handle_readdir(header, body, tree, false),
        FUSE_READDIRPLUS => handle_readdir(header, body, tree, true),
        FUSE_READLINK => handle_readlink(header, tree),
        FUSE_STATFS => handle_statfs(header, manifest),
        FUSE_ACCESS => handle_access(header, body, tree),
        _ => reply_err(header, -libc_enosys()),
    }
}

fn handle_init(header: &FuseInHeader, body: &[u8]) -> Vec<u8> {
    let init_in: FuseInitIn = match unsafe { read_struct(body, 0) } {
        Some(v) => v,
        None => return reply_err(header, -libc_einval()),
    };

    let mut flags = FUSE_ASYNC_READ | FUSE_BIG_WRITES;
    // FUSE_MAX_PAGES is what makes `max_pages` below count. Without it the
    // kernel caps every read at 32 pages, 128 KiB, so a block larger than
    // that takes two requests and a 1 MiB readahead takes eight.
    flags |= init_in.flags
        & (FUSE_DO_READDIRPLUS
            | FUSE_READDIRPLUS_AUTO
            | FUSE_CACHE_SYMLINKS
            | FUSE_NO_OPENDIR_SUPPORT
            | FUSE_MAX_PAGES);

    let init_out = FuseInitOut {
        major: 7,
        minor: 31,
        max_readahead: 1024 * 1024,
        flags,
        max_background: 16,
        congestion_threshold: 12,
        max_write: 0,
        time_gran: 1,
        max_pages: 256,
        map_alignment: 0,
        flags2: 0,
        unused: [0; 7],
    };

    let mut payload = Vec::new();
    unsafe { write_struct(&mut payload, &init_out) };
    reply_ok(header, &payload)
}

fn handle_lookup(
    header: &FuseInHeader,
    body: &[u8],
    tree: &Tree,
    recorder: &mut Option<TraceRecorder>,
) -> Vec<u8> {
    let manifest = tree.manifest;
    let parent_inode = header.nodeid;
    if parent_inode == 0 || inode_to_entry(parent_inode) >= manifest.entries.len() {
        return reply_err(header, -libc_enoent());
    }

    let name = match body.iter().position(|&b| b == 0) {
        Some(pos) => &body[..pos],
        None => body,
    };
    let name_str = match std::str::from_utf8(name) {
        Ok(s) => s,
        Err(_) => return reply_err(header, -libc_enoent()),
    };

    if parent_inode == entry_to_inode(0) && name_str == TRACE_ARM {
        if let Some(rec) = recorder {
            rec.armed = true;
        }
        return reply_err(header, -libc_enoent());
    }

    let found = tree.children[parent_inode as usize]
        .iter()
        .find(|&&child| {
            manifest.get_string(manifest.entries[inode_to_entry(child)].name) == name_str
        })
        .copied();
    let attr = match found {
        Some(child_inode) => {
            let entry_idx = inode_to_entry(child_inode);
            if let Some(rec) = recorder {
                rec.record(entry_idx, manifest);
            }
            make_attr(child_inode, &manifest.entries[entry_idx], manifest)
        }
        None => match tree.synthetic.lookup(parent_inode, name_str) {
            Some(inode) => match tree.synthetic.get(inode) {
                Some(link) => tree.synthetic.attr(inode, link, manifest),
                None => return reply_err(header, -libc_enoent()),
            },
            None => return reply_err(header, -libc_enoent()),
        },
    };
    let entry_out = FuseEntryOut {
        nodeid: attr.ino,
        generation: 0,
        entry_valid: ENTRY_TIMEOUT,
        attr_valid: ATTR_TIMEOUT,
        entry_valid_nsec: 0,
        attr_valid_nsec: 0,
        attr,
    };
    let mut payload = Vec::new();
    unsafe { write_struct(&mut payload, &entry_out) };
    reply_ok(header, &payload)
}

/// ACCESS answered honestly instead of with a blanket yes.
///
/// Both mount paths set `default_permissions`, so the kernel re-checks
/// every open against the recorded modes and a wrong answer here cannot
/// widen what a process may open; this reply only feeds `faccessat(2)`
/// and the shell's `[ -r ]`-style tests. The check mirrors what the
/// kernel does for the owner — the mount presents every file as owned
/// by the launching user — with one deliberate strictness: the mount is
/// read-only, so a write-access question is denied even when the mode
/// bits would allow it, matching the EROFS an actual open would get.
fn handle_access(header: &FuseInHeader, body: &[u8], tree: &Tree) -> Vec<u8> {
    let manifest = tree.manifest;
    // FUSE_ACCESS_IN carries the mask as one little-endian u32.
    if body.len() < 4 {
        return reply_err(header, -libc_eio());
    }
    let mask = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
    let inode = header.nodeid;
    let mode = if tree.synthetic.get(inode).is_some() {
        0o777
    } else if inode == 0 || inode_to_entry(inode) >= manifest.entries.len() {
        return reply_err(header, -libc_enoent());
    } else {
        manifest.entries[inode_to_entry(inode)].mode
    };
    if access_allowed(mode, mask) {
        reply_ok(header, &[])
    } else {
        reply_err(header, -libc_eacces())
    }
}

/// The owner-bit check for one access question. `mask` is a POSIX access
/// mask: 0 asks only for existence, otherwise R_OK|W_OK|X_OK bits, all
/// of which must be granted.
fn access_allowed(entry_mode: u32, mask: u32) -> bool {
    const W_OK: u32 = 2;
    if mask == 0 {
        return true;
    }
    if mask & W_OK != 0 {
        return false;
    }
    let owner = (entry_mode >> 6) & 0o7;
    owner & mask == mask
}

fn handle_getattr(header: &FuseInHeader, tree: &Tree) -> Vec<u8> {
    let manifest = tree.manifest;
    let inode = header.nodeid;
    let attr = if let Some(link) = tree.synthetic.get(inode) {
        tree.synthetic.attr(inode, link, manifest)
    } else if inode == 0 || inode_to_entry(inode) >= manifest.entries.len() {
        return reply_err(header, -libc_enoent());
    } else {
        make_attr(inode, &manifest.entries[inode_to_entry(inode)], manifest)
    };

    let attr_out = FuseAttrOut {
        attr_valid: ATTR_TIMEOUT,
        attr_valid_nsec: 0,
        dummy: 0,
        attr,
    };
    let mut payload = Vec::new();
    unsafe { write_struct(&mut payload, &attr_out) };
    reply_ok(header, &payload)
}

fn handle_open(
    header: &FuseInHeader,
    manifest: &Manifest,
    recorder: &mut Option<TraceRecorder>,
) -> Vec<u8> {
    let inode = header.nodeid;
    if inode == 0 || inode_to_entry(inode) >= manifest.entries.len() {
        return reply_err(header, -libc_enoent());
    }

    if let Some(rec) = recorder {
        rec.record(inode_to_entry(inode), manifest);
    }
    let entry = &manifest.entries[inode_to_entry(inode)];
    if entry.kind != EntryKind::File {
        return reply_err(header, -libc_eisdir());
    }

    let open_out = FuseOpenOut {
        fh: inode,
        open_flags: FOPEN_KEEP_CACHE,
        padding: 0,
    };
    let mut payload = Vec::new();
    unsafe { write_struct(&mut payload, &open_out) };
    reply_ok(header, &payload)
}

fn handle_opendir(header: &FuseInHeader, manifest: &Manifest) -> Vec<u8> {
    let inode = header.nodeid;
    if inode == 0 || inode_to_entry(inode) >= manifest.entries.len() {
        return reply_err(header, -libc_enoent());
    }

    let entry = &manifest.entries[inode_to_entry(inode)];
    if entry.kind != EntryKind::Dir {
        return reply_err(header, -libc_enotdir());
    }

    let open_out = FuseOpenOut {
        fh: inode,
        open_flags: 0,
        padding: 0,
    };
    let mut payload = Vec::new();
    unsafe { write_struct(&mut payload, &open_out) };
    reply_ok(header, &payload)
}

/// READDIR, or READDIRPLUS when `plus`: the directory's own entries, then
/// the links served in it.
fn handle_readdir(header: &FuseInHeader, body: &[u8], tree: &Tree, plus: bool) -> Vec<u8> {
    let manifest = tree.manifest;
    let read_in: FuseReadIn = match unsafe { read_struct(body, 0) } {
        Some(v) => v,
        None => return reply_err(header, -libc_einval()),
    };

    let inode = header.nodeid;
    if inode == 0 {
        return reply_err(header, -libc_enoent());
    }
    let entry_idx = inode_to_entry(inode);
    if entry_idx >= manifest.entries.len() {
        return reply_err(header, -libc_enoent());
    }

    let entry = &manifest.entries[entry_idx];
    let parent_inode = if entry.parent == u32::MAX {
        1u64
    } else {
        entry_to_inode(entry.parent as usize)
    };

    let max_size = read_in.size as usize;
    let start_offset = read_in.offset as usize;
    let mut payload = Vec::with_capacity(max_size.min(4096));

    let ch = &tree.children[inode as usize];
    let links = tree.synthetic.children(inode);
    let total_entries = 2 + ch.len() + links.len();

    for i in start_offset..total_entries {
        // `attr` is None for . and .., which READDIRPLUS reports with a
        // zero nodeid so the kernel does not cache them.
        let (ino, typ, name, attr): (u64, u32, &[u8], Option<FuseAttr>) = match i {
            0 => (inode, dir_type(EntryKind::Dir), b".", None),
            1 => (parent_inode, dir_type(EntryKind::Dir), b"..", None),
            _ if i - 2 < ch.len() => {
                let child_inode = ch[i - 2];
                let child_entry = &manifest.entries[inode_to_entry(child_inode)];
                (
                    child_inode,
                    dir_type(child_entry.kind),
                    manifest.get_string(child_entry.name).as_bytes(),
                    plus.then(|| make_attr(child_inode, child_entry, manifest)),
                )
            }
            _ => {
                let (link_inode, link) = links[i - 2 - ch.len()];
                (
                    link_inode,
                    dir_type(EntryKind::Symlink),
                    link.name.as_bytes(),
                    plus.then(|| tree.synthetic.attr(link_inode, link, manifest)),
                )
            }
        };
        let dent_size = if plus {
            direntplus_size(name.len())
        } else {
            dirent_size(name.len())
        };
        if payload.len() + dent_size > max_size {
            break;
        }
        if plus {
            let entry_out = match attr {
                Some(attr) => FuseEntryOut {
                    nodeid: ino,
                    generation: 0,
                    entry_valid: ENTRY_TIMEOUT,
                    attr_valid: ATTR_TIMEOUT,
                    entry_valid_nsec: 0,
                    attr_valid_nsec: 0,
                    attr,
                },
                None => unsafe { core::mem::zeroed() },
            };
            unsafe { write_struct(&mut payload, &entry_out) };
        }
        let dirent = FuseDirent {
            ino,
            off: (i + 1) as u64,
            namelen: name.len() as u32,
            typ,
        };
        unsafe { write_struct(&mut payload, &dirent) };
        payload.extend_from_slice(name);
        let used = if plus { ENTRY_OUT_SIZE } else { 0 } + DIRENT_BASE_SIZE + name.len();
        let padding = dent_size - used;
        if padding > 0 {
            payload.extend_from_slice(&[0u8; 8][..padding]);
        }
    }

    reply_ok(header, &payload)
}

fn handle_readlink(header: &FuseInHeader, tree: &Tree) -> Vec<u8> {
    let manifest = tree.manifest;
    let inode = header.nodeid;
    if let Some(link) = tree.synthetic.get(inode) {
        return reply_ok(header, &link.target);
    }
    if inode == 0 || inode_to_entry(inode) >= manifest.entries.len() {
        return reply_err(header, -libc_enoent());
    }

    let entry = &manifest.entries[inode_to_entry(inode)];
    if entry.kind != EntryKind::Symlink {
        return reply_err(header, -libc_einval());
    }

    let target = manifest.get_string(entry.symlink_target);
    reply_ok(header, target.as_bytes())
}

fn handle_statfs(header: &FuseInHeader, manifest: &Manifest) -> Vec<u8> {
    let total_files = manifest.entries.len() as u64;
    let total_blocks: u64 = manifest
        .entries
        .iter()
        .map(|e| e.blocks.iter().map(|b| b.original_size).sum::<u64>())
        .map(|size| size.div_ceil(512))
        .sum();

    let statfs = FuseStatfsOut {
        st: FuseKStatfs {
            blocks: total_blocks,
            bfree: 0,
            bavail: 0,
            files: total_files,
            ffree: 0,
            bsize: 512,
            namelen: 255,
            frsize: 512,
            padding: 0,
            spare: [0; 6],
        },
    };
    let mut payload = Vec::new();
    unsafe { write_struct(&mut payload, &statfs) };
    reply_ok(header, &payload)
}

fn reply_ok(header: &FuseInHeader, payload: &[u8]) -> Vec<u8> {
    let total_len = OUT_HEADER_SIZE + payload.len();
    let out_header = FuseOutHeader {
        len: total_len as u32,
        error: 0,
        unique: header.unique,
    };
    let mut buf = Vec::with_capacity(total_len);
    unsafe { write_struct(&mut buf, &out_header) };
    buf.extend_from_slice(payload);
    buf
}

fn reply_err(header: &FuseInHeader, error: i32) -> Vec<u8> {
    let out_header = FuseOutHeader {
        len: OUT_HEADER_SIZE as u32,
        error,
        unique: header.unique,
    };
    let mut buf = Vec::with_capacity(OUT_HEADER_SIZE);
    unsafe { write_struct(&mut buf, &out_header) };
    buf
}

fn dir_type(kind: EntryKind) -> u32 {
    match kind {
        EntryKind::Dir => 4,
        EntryKind::File => 8,
        EntryKind::Symlink => 10,
    }
}

fn libc_enoent() -> i32 {
    2
}
fn libc_eio() -> i32 {
    5
}
fn libc_enosys() -> i32 {
    38
}
fn libc_einval() -> i32 {
    22
}
fn libc_eisdir() -> i32 {
    21
}
fn libc_enotdir() -> i32 {
    20
}
fn libc_eacces() -> i32 {
    13
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(n: usize) -> Vec<u8> {
        vec![0u8; n]
    }

    /// The copier flattens every `usr/X/`, so every such directory has to
    /// regain the prefix, or the trace names paths no database lists and
    /// the trace tier prunes what the run used.
    #[test]
    fn a_trace_restores_usr_for_every_flattened_directory() {
        for (appdir, sysroot) in [
            ("bin/app", "usr/bin/app"),
            ("lib/libfoo.so.1", "usr/lib/libfoo.so.1"),
            ("lib32/libfoo.so.1", "usr/lib32/libfoo.so.1"),
            ("libexec/app/helper", "usr/libexec/app/helper"),
            ("share/app/data", "usr/share/app/data"),
            ("etc/app.conf", "etc/app.conf"),
            ("opt/vendor/run", "opt/vendor/run"),
            ("var/lib/app/state", "var/lib/app/state"),
        ] {
            assert_eq!(
                appdir_to_sysroot(appdir).as_deref(),
                Some(sysroot),
                "{appdir}"
            );
        }
        assert_eq!(appdir_to_sysroot(".onelf/env"), None);
    }

    #[test]
    fn access_masks_are_checked_against_owner_bits() {
        assert!(access_allowed(0o644, 0), "existence is always granted");
        assert!(access_allowed(0o644, 4), "readable to the owner");
        assert!(!access_allowed(0o644, 1), "not executable");
        assert!(!access_allowed(0o644, 2), "the mount is read-only");
        assert!(!access_allowed(0o644, 6), "R|W needs both, W is denied");
        assert!(access_allowed(0o400, 4), "owner-only read is readable");
        assert!(!access_allowed(0o400, 5), "R|X needs both, X is absent");
        assert!(access_allowed(0o755, 5), "R|X on an executable");
        assert!(!access_allowed(0o000, 4), "mode 000 grants nothing");
    }

    /// A sustained read never goes idle, so the byte budget is the only
    /// thing standing between a large file and the whole of it in memory.
    #[test]
    fn cache_evicts_to_stay_within_budget() {
        let mut c = BlockCache::new(300);
        for i in 0..10 {
            c.insert_block(1, i, block(100), &[]);
            assert!(
                c.bytes <= 300,
                "budget exceeded after {} inserts: {}",
                i + 1,
                c.bytes
            );
        }
        // Oldest go first, so the most recent inserts survive.
        assert!(c.get_block(1, 9).is_some());
        assert!(c.get_block(1, 0).is_none());
    }

    #[test]
    fn reinserting_a_block_does_not_double_count() {
        let mut c = BlockCache::new(1000);
        c.insert_block(1, 0, block(100), &[]);
        c.insert_block(1, 0, block(100), &[]);
        assert_eq!(c.bytes, 100);
        assert_eq!(c.order.len(), 1);
    }

    /// Refusing an oversized block would make the read it belongs to
    /// unservable, so it is admitted even though it breaches the budget.
    #[test]
    fn a_block_larger_than_the_budget_is_still_served() {
        let mut c = BlockCache::new(50);
        c.insert_block(1, 0, block(500), &[]);
        assert!(c.get_block(1, 0).is_some());
    }

    /// A read spanning more blocks than the budget holds must still be
    /// servable: evicting one of its own blocks mid-read left the gather
    /// borrowing a block that was no longer there.
    #[test]
    fn pinned_blocks_survive_eviction() {
        let mut c = BlockCache::new(250);
        let needed = [0usize, 1, 2, 3, 4];
        for i in needed {
            c.insert_block(1, i, block(100), &needed);
        }
        for i in needed {
            assert!(
                c.get_block(1, i).is_some(),
                "block {i} was needed by the read in progress"
            );
        }

        // Another block landing on a full cache must not take one either.
        c.insert_block(1, 5, block(100), &needed);
        for i in needed {
            assert!(c.get_block(1, i).is_some(), "an insert evicted block {i}");
        }

        // Once nothing is pinned, the budget is enforced again.
        c.insert_block(2, 0, block(100), &[]);
        assert!(c.bytes <= 600, "unpinned inserts must still evict");
    }

    /// A stored (uncompressed) payload of `contents`, one block each, in a
    /// file of its own, with the footer and blocks a read needs.
    fn stored_payload(
        tag: &str,
        contents: &[&[u8]],
    ) -> (File, onelf_format::Footer, Vec<onelf_format::Block>) {
        let path =
            std::env::temp_dir().join(format!("onelf-prefetch-{tag}-{}", std::process::id()));
        std::fs::write(&path, contents.concat()).unwrap();
        let file = File::open(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let mut offset = 0;
        let blocks = contents
            .iter()
            .map(|c| {
                let b = onelf_format::Block {
                    payload_offset: offset,
                    compressed_size: c.len() as u64,
                    original_size: c.len() as u64,
                    content_hash: *blake3::hash(c).as_bytes(),
                };
                offset += c.len() as u64;
                b
            })
            .collect();
        let footer = onelf_format::Footer {
            format_version: 2,
            flags: onelf_format::Flags::STORED,
            manifest_offset: 0,
            manifest_compressed: 0,
            manifest_original: 0,
            payload_offset: 0,
            payload_size: offset,
            dict_offset: 0,
            dict_size: 0,
            manifest_checksum: [0; 4],
        };
        (file, footer, blocks)
    }

    fn prefetch_of(blocks: &[onelf_format::Block], window: usize) -> Prefetch {
        let jobs = blocks
            .iter()
            .enumerate()
            .map(|(i, b)| ((1, i), b.clone()))
            .collect();
        Prefetch {
            shared: Arc::new(Prefetched::new(jobs, window)),
            workers: Vec::new(),
        }
    }

    /// What a read takes from the prefetch is byte for byte what it would
    /// have decompressed itself.
    #[test]
    fn a_prefetched_block_is_what_the_read_would_have_read() {
        let contents: [&[u8]; 3] = [b"first block", b"second block", b"third"];
        let (file, footer, blocks) = stored_payload("same", &contents);
        let prefetch = prefetch_of(&blocks, usize::MAX);
        prefetch.shared.work(&file, &footer, None);
        for (i, b) in blocks.iter().enumerate() {
            let direct = loader::read_payload_entry(&file, &footer, b, None).unwrap();
            assert_eq!(prefetch.take((1, i)).as_deref(), Some(contents[i]));
            assert_eq!(direct, contents[i]);
        }
        assert_eq!(
            prefetch.shared.lock().bytes,
            0,
            "taking a block frees its room"
        );
    }

    /// A read that got there first decompressed the block itself, so a
    /// worker finishing it later must not keep a copy nobody will take.
    #[test]
    fn a_block_a_read_took_is_not_kept_afterwards() {
        let (file, footer, blocks) = stored_payload("taken", &[b"aaaa", b"bbbb"]);
        let prefetch = prefetch_of(&blocks, usize::MAX);
        assert!(prefetch.take((1, 0)).is_none());
        prefetch.shared.work(&file, &footer, None);
        assert!(prefetch.take((1, 0)).is_none());
        assert_eq!(prefetch.take((1, 1)).as_deref(), Some(&b"bbbb"[..]));
    }

    /// Blocks the profile does not list are the read's alone and leave no
    /// trace in the prefetch.
    #[test]
    fn a_block_outside_the_profile_is_left_to_the_read() {
        let (_file, _footer, blocks) = stored_payload("outside", &[b"aaaa"]);
        let prefetch = prefetch_of(&blocks, usize::MAX);
        assert!(prefetch.take((2, 0)).is_none());
        assert!(prefetch.take((1, 7)).is_none());
        assert!(prefetch.shared.lock().taken.is_empty());
    }

    /// The window holds the workers back until reads take what they made,
    /// and dropping the prefetch stops a worker that is waiting for room.
    #[test]
    fn the_window_bounds_what_waits_unread() {
        let contents: [&[u8]; 3] = [b"0123", b"4567", b"89ab"];
        let (file, footer, blocks) = stored_payload("window", &contents);
        let mut prefetch = prefetch_of(&blocks, 4);
        let shared = Arc::clone(&prefetch.shared);
        prefetch.workers.push(std::thread::spawn(move || {
            shared.work(&file, &footer, None)
        }));

        let ready = |p: &Prefetch, key: BlockKey| {
            let deadline = Instant::now() + std::time::Duration::from_secs(10);
            while !p.shared.lock().ready.contains_key(&key) {
                assert!(Instant::now() < deadline, "block {key:?} never arrived");
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        };
        ready(&prefetch, (1, 0));
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(prefetch.shared.lock().ready.len(), 1, "the window is full");

        assert_eq!(prefetch.take((1, 0)).as_deref(), Some(contents[0]));
        ready(&prefetch, (1, 1));
        drop(prefetch);
    }

    #[test]
    fn idle_clear_releases_everything() {
        let mut c = BlockCache::new(1000);
        c.insert_block(1, 0, block(100), &[]);
        assert!(c.is_active());
        c.last_access = Some(Instant::now() - std::time::Duration::from_secs(CACHE_IDLE_SECS + 1));
        c.maybe_clear();
        assert_eq!(c.bytes, 0);
        assert!(!c.is_active());
        assert!(c.get_block(1, 0).is_none());
    }
}
