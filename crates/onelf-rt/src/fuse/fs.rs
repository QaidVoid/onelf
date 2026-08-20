//! FUSE filesystem operations.
//!
//! Implements the low-level FUSE ops (lookup, getattr, readdir, read, etc.)
//! against the in-memory manifest. File reads decompress payload blocks on
//! demand with a per-inode block cache and sequential prefetch.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io;
use std::os::fd::AsFd;
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

/// Default ceiling on decompressed blocks held in memory.
///
/// At the packer's 256 KiB block size this is 128 blocks, far more than the
/// one-block-ahead prefetch needs, while keeping the server itself from
/// becoming the memory problem it exists to avoid.
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
    /// Without that, a read spanning more blocks than the budget holds, or a
    /// prefetch landing on a full cache, evicts a block that is about to be
    /// borrowed.
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

    fn maybe_clear(&mut self) {
        if let Some(last) = self.last_access
            && last.elapsed().as_secs() >= CACHE_IDLE_SECS
        {
            self.data.clear();
            self.data.shrink_to_fit();
            self.order.clear();
            self.bytes = 0;
            self.last_access = None;
        }
    }

    fn is_active(&self) -> bool {
        self.last_access.is_some()
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
}

impl<'a> FuseState<'a> {
    pub fn new(
        manifest: &'a Manifest,
        file: &'a mut File,
        footer: &'a onelf_format::Footer,
        dict: Option<&'a [u8]>,
    ) -> Self {
        let children = build_children(manifest);
        Self {
            manifest,
            file,
            footer,
            dict,
            children,
            cache: BlockCache::new(cache_budget()),
            verified: HashMap::new(),
        }
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

    pub fn run_loop(&mut self, fuse_fd: &impl AsFd, death_pipe: &impl AsFd, buf: &mut [u8]) {
        use rustix::event::Timespec;

        let cache_timeout = Timespec {
            tv_sec: CACHE_IDLE_SECS as i64,
            tv_nsec: 0,
        };

        loop {
            self.cache.maybe_clear();

            let timeout = if self.cache.is_active() {
                Some(&cache_timeout)
            } else {
                None
            };

            let mut poll_fds = [
                PollFd::new(fuse_fd, PollFlags::IN),
                PollFd::new(death_pipe, PollFlags::IN),
            ];
            match poll(&mut poll_fds, timeout) {
                Ok(0) => continue,
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => return,
            }

            if poll_fds[1]
                .revents()
                .intersects(PollFlags::HUP | PollFlags::IN)
            {
                return;
            }

            if !poll_fds[0].revents().intersects(PollFlags::IN) {
                continue;
            }

            let n = match rustix::io::read(fuse_fd, &mut *buf) {
                Ok(n) if n >= IN_HEADER_SIZE => n,
                Ok(_) => continue,
                Err(rustix::io::Errno::INTR) => continue,
                Err(rustix::io::Errno::NODEV) => return,
                Err(_) => return,
            };

            let header: FuseInHeader = unsafe { read_struct(buf, 0).unwrap() };

            if header.opcode == FUSE_DESTROY {
                return;
            }
            if header.opcode == FUSE_FORGET || header.opcode == FUSE_BATCH_FORGET {
                continue;
            }

            if header.opcode == FUSE_READ {
                self.handle_read(fuse_fd, &header, &buf[IN_HEADER_SIZE..n]);
                continue;
            }

            let response = dispatch(
                &header,
                &buf[IN_HEADER_SIZE..n],
                self.manifest,
                &self.children,
            );
            if !response.is_empty() {
                let _ = rustix::io::write(fuse_fd, &response);
            }
        }
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
        if entry_idx >= self.manifest.entries.len() {
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

        // Build block offset map, stack-allocated for files <=32 blocks (8MB @ 256KB)
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

        // Decompress missing blocks
        for &block_idx in needed_blocks {
            if self.cache.get_block(inode, block_idx).is_some() {
                continue;
            }
            let block = &entry.blocks[block_idx];
            match loader::read_payload_entry(self.file, self.footer, block, self.dict) {
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

        // Prefetch next sequential block
        if let Some(&last_needed) = needed_blocks.last() {
            let next = last_needed + 1;
            if next < num_blocks && self.cache.get_block(inode, next).is_none() {
                let block = &entry.blocks[next];
                let _ = loader::read_payload_entry(self.file, self.footer, block, self.dict)
                    .map(|data| self.cache.insert_block(inode, next, data, needed_blocks));
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

fn dispatch(
    header: &FuseInHeader,
    body: &[u8],
    manifest: &Manifest,
    children: &[Vec<u64>],
) -> Vec<u8> {
    match header.opcode {
        FUSE_INIT => handle_init(header, body),
        FUSE_LOOKUP => handle_lookup(header, body, manifest, children),
        FUSE_GETATTR => handle_getattr(header, manifest),
        FUSE_OPEN => handle_open(header, manifest),
        FUSE_RELEASE | FUSE_RELEASEDIR => reply_ok(header, &[]),
        FUSE_OPENDIR => handle_opendir(header, manifest),
        FUSE_READDIR => handle_readdir(header, body, manifest, children),
        FUSE_READDIRPLUS => handle_readdirplus(header, body, manifest, children),
        FUSE_READLINK => handle_readlink(header, manifest),
        FUSE_STATFS => handle_statfs(header, manifest),
        FUSE_ACCESS => reply_ok(header, &[]),
        _ => reply_err(header, -libc_enosys()),
    }
}

fn handle_init(header: &FuseInHeader, body: &[u8]) -> Vec<u8> {
    let init_in: FuseInitIn = match unsafe { read_struct(body, 0) } {
        Some(v) => v,
        None => return reply_err(header, -libc_einval()),
    };

    let mut flags = FUSE_ASYNC_READ | FUSE_BIG_WRITES;
    flags |= init_in.flags
        & (FUSE_DO_READDIRPLUS
            | FUSE_READDIRPLUS_AUTO
            | FUSE_CACHE_SYMLINKS
            | FUSE_NO_OPENDIR_SUPPORT);

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
    manifest: &Manifest,
    children: &[Vec<u64>],
) -> Vec<u8> {
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

    let ch = &children[parent_inode as usize];
    for &child_inode in ch {
        let entry_idx = inode_to_entry(child_inode);
        let entry = &manifest.entries[entry_idx];
        let entry_name = manifest.get_string(entry.name);
        if entry_name == name_str {
            let attr = make_attr(child_inode, entry, manifest);
            let entry_out = FuseEntryOut {
                nodeid: child_inode,
                generation: 0,
                entry_valid: ENTRY_TIMEOUT,
                attr_valid: ATTR_TIMEOUT,
                entry_valid_nsec: 0,
                attr_valid_nsec: 0,
                attr,
            };
            let mut payload = Vec::new();
            unsafe { write_struct(&mut payload, &entry_out) };
            return reply_ok(header, &payload);
        }
    }

    reply_err(header, -libc_enoent())
}

fn handle_getattr(header: &FuseInHeader, manifest: &Manifest) -> Vec<u8> {
    let inode = header.nodeid;
    if inode == 0 || inode_to_entry(inode) >= manifest.entries.len() {
        return reply_err(header, -libc_enoent());
    }

    let entry = &manifest.entries[inode_to_entry(inode)];
    let attr = make_attr(inode, entry, manifest);

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

fn handle_open(header: &FuseInHeader, manifest: &Manifest) -> Vec<u8> {
    let inode = header.nodeid;
    if inode == 0 || inode_to_entry(inode) >= manifest.entries.len() {
        return reply_err(header, -libc_enoent());
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

fn handle_readdir(
    header: &FuseInHeader,
    body: &[u8],
    manifest: &Manifest,
    children: &[Vec<u64>],
) -> Vec<u8> {
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

    let ch = &children[inode as usize];
    let total_entries = 2 + ch.len();

    for i in start_offset..total_entries {
        let (ino, typ, name): (u64, u32, &[u8]) = match i {
            0 => (inode, dir_type(EntryKind::Dir), b"."),
            1 => (parent_inode, dir_type(EntryKind::Dir), b".."),
            _ => {
                let child_inode = ch[i - 2];
                let child_entry = &manifest.entries[inode_to_entry(child_inode)];
                let name = manifest.get_string(child_entry.name).as_bytes();
                (child_inode, dir_type(child_entry.kind), name)
            }
        };
        let dent_size = dirent_size(name.len());
        if payload.len() + dent_size > max_size {
            break;
        }
        let dirent = FuseDirent {
            ino,
            off: (i + 1) as u64,
            namelen: name.len() as u32,
            typ,
        };
        unsafe { write_struct(&mut payload, &dirent) };
        payload.extend_from_slice(name);
        let padding = dent_size - DIRENT_BASE_SIZE - name.len();
        if padding > 0 {
            payload.extend_from_slice(&[0u8; 8][..padding]);
        }
    }

    reply_ok(header, &payload)
}

fn handle_readdirplus(
    header: &FuseInHeader,
    body: &[u8],
    manifest: &Manifest,
    children: &[Vec<u64>],
) -> Vec<u8> {
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

    let ch = &children[inode as usize];
    let total_entries = 2 + ch.len();

    for i in start_offset..total_entries {
        let (child_inode, typ, name): (u64, u32, &[u8]) = match i {
            0 => (inode, dir_type(EntryKind::Dir), b"."),
            1 => (parent_inode, dir_type(EntryKind::Dir), b".."),
            _ => {
                let ci = ch[i - 2];
                let ce = &manifest.entries[inode_to_entry(ci)];
                (
                    ci,
                    dir_type(ce.kind),
                    manifest.get_string(ce.name).as_bytes(),
                )
            }
        };

        let dent_size = direntplus_size(name.len());
        if payload.len() + dent_size > max_size {
            break;
        }

        // For real entries: full entry_out so kernel populates dcache + icache
        // For . and ..: nodeid=0 tells kernel to skip cache population
        let entry_out = if i >= 2 {
            let ce = &manifest.entries[inode_to_entry(child_inode)];
            FuseEntryOut {
                nodeid: child_inode,
                generation: 0,
                entry_valid: ENTRY_TIMEOUT,
                attr_valid: ATTR_TIMEOUT,
                entry_valid_nsec: 0,
                attr_valid_nsec: 0,
                attr: make_attr(child_inode, ce, manifest),
            }
        } else {
            unsafe { core::mem::zeroed() }
        };
        unsafe { write_struct(&mut payload, &entry_out) };

        let dirent = FuseDirent {
            ino: child_inode,
            off: (i + 1) as u64,
            namelen: name.len() as u32,
            typ,
        };
        unsafe { write_struct(&mut payload, &dirent) };
        payload.extend_from_slice(name);
        let padding = dent_size - ENTRY_OUT_SIZE - DIRENT_BASE_SIZE - name.len();
        if padding > 0 {
            payload.extend_from_slice(&[0u8; 8][..padding]);
        }
    }

    reply_ok(header, &payload)
}

fn handle_readlink(header: &FuseInHeader, manifest: &Manifest) -> Vec<u8> {
    let inode = header.nodeid;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn block(n: usize) -> Vec<u8> {
        vec![0u8; n]
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

        // A prefetch on a full cache must not take one either.
        c.insert_block(1, 5, block(100), &needed);
        for i in needed {
            assert!(c.get_block(1, i).is_some(), "prefetch evicted block {i}");
        }

        // Once nothing is pinned, the budget is enforced again.
        c.insert_block(2, 0, block(100), &[]);
        assert!(c.bytes <= 600, "unpinned inserts must still evict");
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
