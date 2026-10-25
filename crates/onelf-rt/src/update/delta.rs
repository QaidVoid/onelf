//! Delta assembly from the release binary itself.
//!
//! The remote package's manifest names every payload block with the BLAKE3
//! of its content, so comparing it against the running package's manifest
//! names exactly which content changed. Unchanged content is copied from
//! the running binary, changed content is fetched, and the result is
//! written to a staging file byte for byte as the release was published.
//!
//! Two payload kinds are planned:
//!
//! - **Blocks** (the default): a block's compressed bytes are only
//!   reproducible when both packages compressed with the same dictionary,
//!   so reuse additionally requires the dictionaries to match. Every
//!   copied block is decoded and hashed before it is written, and a
//!   mismatch degrades that one block to a fetch.
//! - **Stipplefs images**: the image is planned from its own metadata,
//!   which is fetched from the release in a handful of small requests.
//!   Only fully stored images (built with `--no-compress`) are eligible:
//!   a compressed image re-encodes unchanged content against a dictionary
//!   retrained for every build, so its stored bytes are not stable across
//!   builds and nothing shorter than the whole payload can be reused.
//!   Stored images copy at slice granularity through the extent table,
//!   which follows the content wherever the image's layout put it.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::path::Path;

use onelf_format::payload::Codec;
use onelf_format::reader::{block_extent, block_original_size};
use onelf_format::{EntryKind, Flags, Footer, Manifest, FOOTER_SIZE};
use stipplefs::read::{Image, Reader};
use stipplefs::source::ByteSource;

use super::range::{RangeSource, RemotePackage};

/// What one assembly reused and what it had to download.
pub(super) struct Stats {
    /// Copy groups taken from the running binary.
    pub reused: usize,
    /// Bytes fetched from the release. The manifest, dictionary and
    /// footer are not counted: they are small and always fetched.
    pub fetched: u64,
}

/// Assemble the remote release into `staged` from `local_path` and range
/// fetches, returning what was reused. `Err` names why the payload cannot
/// be delta-planned at all and the caller should download in full.
pub(super) fn assemble(
    local_path: &Path,
    remote: &RemotePackage,
    staged: &std::fs::File,
    src: &dyn RangeSource,
) -> Result<Stats, String> {
    let local = crate::loader::load_from(local_path)
        .map_err(|e| format!("open the running package: {e}"))?;
    if local.manifest.header.version < 3 || remote.manifest.header.version < 3 {
        return Err("delta planning needs version-3 manifests on both sides".into());
    }
    if local.footer.is_stipple() != remote.footer.is_stipple() {
        return Err("the update changes the payload kind".into());
    }
    let plan = if remote.footer.is_stipple() {
        plan_stipple(&local, remote, src)?
    } else {
        plan_blocks(&local, remote, src)?
    };
    let dict = plan.dict.as_deref();
    write_plan(staged, &local.file, remote, &plan, dict, src)
}

/// One contiguous byte run copied from the running binary into the staged
/// file. `dst` and `src` are absolute package-file offsets.
struct CopyRange {
    dst: u64,
    src: u64,
    len: u64,
}

/// A unit of reuse verified as a whole. A block is one range; a stipplefs
/// slice may straddle stored blocks and arrive as several.
struct CopyGroup {
    ranges: Vec<CopyRange>,
    /// BLAKE3 the copied bytes must hash to.
    hash: [u8; 32],
    /// The bytes are the content (stored payloads): hash them directly.
    /// Otherwise they are compressed and decoded with `original` and the
    /// dictionary first.
    raw: bool,
    original: u64,
}

/// A contiguous remote span to download.
struct Span {
    start: u64,
    len: u64,
}

/// Everything the writer needs, decided before a byte is written.
struct Plan {
    /// The runtime prefix: copied when both manifests pin the same
    /// `prefix_hash`, fetched otherwise.
    prefix: Prefix,
    /// The remote dictionary bytes, when the release carries one.
    dict: Option<Vec<u8>>,
    copies: Vec<CopyGroup>,
    fetch: Vec<Span>,
}

enum Prefix {
    Copy { len: u64 },
    Fetch { len: u64 },
}

/// The remote dictionary when the release carries one.
fn fetch_dict(src: &dyn RangeSource, footer: &Footer) -> Result<Option<Vec<u8>>, String> {
    if !footer.flags.contains(Flags::HAS_DICT) || footer.dict_size == 0 {
        return Ok(None);
    }
    let mut dict = vec![0u8; footer.dict_size as usize];
    src.fetch_range(footer.dict_offset, footer.dict_size as u64)
        .map_err(|e| format!("fetch dictionary: {e}"))?
        .read_exact(&mut dict)
        .map_err(|e| format!("read dictionary: {e}"))?;
    Ok(Some(dict))
}

/// Same prefix: equal hashes pin equal bytes, and equal bytes pin equal
/// lengths. Fetched otherwise, since the release ships a new runtime.
fn prefix_plan(local: &Footer, local_manifest: &Manifest, remote: &RemotePackage) -> Prefix {
    let same = local_manifest.header.prefix_hash == remote.manifest.header.prefix_hash
        && local_manifest.header.version >= 3
        && local.manifest_offset == remote.footer.manifest_offset;
    if same {
        Prefix::Copy {
            len: remote.footer.manifest_offset,
        }
    } else {
        Prefix::Fetch {
            len: remote.footer.manifest_offset,
        }
    }
}

/// Sort and merge `ranges` into disjoint fetch spans.
fn complement(start: u64, end: u64, mut taken: Vec<Range<u64>>) -> Vec<Span> {
    taken.sort_by_key(|r| r.start);
    let mut spans = Vec::new();
    let mut at = start;
    for range in taken {
        if range.end <= at || range.end <= range.start {
            continue;
        }
        let s = range.start.max(at);
        if s > at {
            spans.push(Span { start: at, len: s - at });
        }
        at = at.max(range.end);
    }
    if at < end {
        spans.push(Span {
            start: at,
            len: end - at,
        });
    }
    spans
}

/// Merge a fetch of `start..start+len` into `spans`, extending the last
/// span when the runs touch.
fn extend_fetch(spans: &mut Vec<Span>, start: u64, len: u64) {
    if let Some(last) = spans.last_mut()
        && last.start + last.len == start
    {
        last.len += len;
        return;
    }
    spans.push(Span { start, len });
}

// ---------------------------------------------------------------------------
// Blocks payloads
// ---------------------------------------------------------------------------

fn plan_blocks(
    local: &crate::loader::PackageData,
    remote: &RemotePackage,
    src: &dyn RangeSource,
) -> Result<Plan, String> {
    let remote_dict = fetch_dict(src, &remote.footer)?;
    // Compressed bytes are only reproducible against the same dictionary,
    // so identical content is only reusable when both sides compressed
    // with the same one, or with none.
    let local_dict = local.codec.dict();
    let dicts_equal = match (local_dict, &remote_dict) {
        (None, None) => true,
        (Some(a), Some(b)) => a == b.as_slice(),
        _ => false,
    };
    let stored_equal = local.footer.is_stored() == remote.footer.is_stored();

    // Local block index: content hash to the block's absolute range.
    let mut local_index: HashMap<[u8; 32], (u64, u64)> = HashMap::new();
    let mut seen = HashSet::new();
    for entry in &local.manifest.entries {
        if entry.kind != EntryKind::File {
            continue;
        }
        for block in &entry.blocks {
            if seen.insert(block.payload_offset) {
                let (abs, len) = block_extent(&local.footer, block)
                    .map_err(|e| format!("running package: {e}"))?;
                local_index.insert(block.content_hash, (abs, len as u64));
            }
        }
    }

    let mut copies = Vec::new();
    let mut fetch = Vec::new();
    seen.clear();
    for entry in &remote.manifest.entries {
        if entry.kind != EntryKind::File {
            continue;
        }
        for block in &entry.blocks {
            if !seen.insert(block.payload_offset) {
                continue;
            }
            let (dst, len) =
                block_extent(&remote.footer, block).map_err(|e| format!("release: {e}"))?;
            let len = len as u64;
            let reusable = local_index
                .get(&block.content_hash)
                .map(|&(_src, src_len)| dicts_equal && stored_equal && src_len == len)
                .unwrap_or(false);
            if reusable {
                // Unwrapping is safe: `reusable` is only true when the
                // lookup above found the block.
                let (src, _) = local_index.get(&block.content_hash).unwrap();
                copies.push(CopyGroup {
                    ranges: vec![CopyRange { dst, src: *src, len }],
                    hash: block.content_hash,
                    raw: remote.footer.is_stored(),
                    original: block.original_size,
                });
            } else {
                extend_fetch(&mut fetch, dst, len);
            }
        }
    }

    let plan = Plan {
        prefix: prefix_plan(&local.footer, &local.manifest, remote),
        dict: remote_dict,
        copies,
        fetch,
    };
    Ok(plan)
}

// ---------------------------------------------------------------------------
// Stipplefs payloads
// ---------------------------------------------------------------------------

/// A byte source served by range fetches, so the remote image's own
/// metadata parser can run against the release without downloading it.
struct FetchingSource<'a> {
    src: &'a dyn RangeSource,
    size: u64,
}

impl ByteSource for FetchingSource<'_> {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> stipplefs::error::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let len = buf.len() as u64;
        let mut reader = self
            .src
            .fetch_range(offset, len)
            .map_err(|e| stipplefs::error::Error::Io(std::io::Error::other(e.to_string())))?;
        reader
            .read_exact(buf)
            .map_err(stipplefs::error::Error::Io)
    }
}

/// Every block in the image is stored raw. Compressed blocks re-encode
/// unchanged content against a dictionary retrained per build, so their
/// stored bytes are useless for cross-build reuse.
fn all_stored(reader: &Reader<'_>, what: &str) -> Result<(), String> {
    let blocks = reader.blocks();
    for i in 0..blocks.count() {
        let entry = blocks
            .get(i)
            .ok_or_else(|| format!("{what}: block {i} is outside the block index"))?;
        if entry.codec != stipplefs::format::Codec::Stored {
            return Err(format!(
                "{what}: the stipplefs image compresses its blocks, so its bytes are \
                 not stable across builds; only --no-compress images delta-update"
            ));
        }
    }
    Ok(())
}

/// The absolute package-file ranges holding `len` bytes of a file's
/// content, followed through the image's extent table into its stored
/// blocks. Stored blocks keep the content verbatim, so extent positions
/// are content positions.
fn slice_stored_ranges(
    reader: &Reader<'_>,
    base: u64,
    ino: u32,
    file_off: u64,
    len: u64,
    what: &str,
) -> Result<Vec<Range<u64>>, String> {
    let block_size = reader.superblock().block_size as u64;
    if block_size == 0 {
        return Err(format!("{what}: the image declares no block size"));
    }
    let end = file_off + len;
    let mut out = Vec::new();
    let mut covered = file_off;
    for (stream_off, ext_off, ext_len) in reader.file_extents(ino) {
        let ext_end = ext_off + ext_len;
        if ext_end <= covered {
            continue;
        }
        if ext_off > covered {
            return Err(format!("{what}: a hole sits before byte {covered}"));
        }
        let stop = end.min(ext_end);
        let mut at = stream_off + (covered - ext_off);
        let stream_end = stream_off + (stop - ext_off);
        while at < stream_end {
            let within_block = at % block_size;
            let index = u32::try_from(at / block_size)
                .map_err(|_| format!("{what}: the image holds too many blocks"))?;
            let entry = reader
                .blocks()
                .get(index)
                .ok_or_else(|| format!("{what}: block {index} is outside the block index"))?;
            if entry.codec != stipplefs::format::Codec::Stored {
                return Err(format!("{what}: a compressed block cannot be copied"));
            }
            let count = (block_size - within_block).min(stream_end - at);
            if u64::from(entry.stored_length) < within_block + count {
                return Err(format!("{what}: block {index} is shorter than its content"));
            }
            let start = base + entry.image_offset + within_block;
            out.push(start..start + count);
            at += count;
        }
        covered = stop;
        if covered >= end {
            break;
        }
    }
    if covered < end {
        return Err(format!("{what}: the image ends the file early"));
    }
    Ok(out)
}

/// Zip two range lists covering the same total length into copy ranges.
fn zip_ranges(dst: Vec<Range<u64>>, src: Vec<Range<u64>>) -> Result<Vec<CopyRange>, String> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut j = 0;
    let mut di = 0;
    let mut sj = 0;
    while i < dst.len() && j < src.len() {
        let n = (dst[i].end - dst[i].start - di).min(src[j].end - src[j].start - sj);
        out.push(CopyRange {
            dst: dst[i].start + di,
            src: src[j].start + sj,
            len: n,
        });
        di += n;
        sj += n;
        if di == dst[i].end - dst[i].start {
            i += 1;
            di = 0;
        }
        if sj == src[j].end - src[j].start {
            j += 1;
            sj = 0;
        }
    }
    if i < dst.len() || j < src.len() {
        return Err("matched slices cover different byte totals".into());
    }
    Ok(out)
}

fn plan_stipple(
    local: &crate::loader::PackageData,
    remote: &RemotePackage,
    src: &dyn RangeSource,
) -> Result<Plan, String> {
    let Codec::Stipple(local_reader) = &local.codec else {
        return Err("the running package is not a stipple payload".into());
    };
    all_stored(local_reader, "the running package")?;

    // The remote image's metadata: a superblock and a handful of small
    // sections, each its own range request. No data block is read here.
    let source = FetchingSource {
        src,
        size: remote.size,
    };
    let image = Image::new(source, remote.footer.payload_offset, remote.footer.payload_size)
        .map_err(|e| format!("read the release image: {e}"))?;
    let remote_reader = image
        .reader()
        .map_err(|e| format!("read the release image: {e}"))?;
    all_stored(&remote_reader, "the release")?;

    // Content hash to the local file's inode and file offset.
    let mut local_slices: HashMap<[u8; 32], (u32, u64, u64)> = HashMap::new();
    for entry in &local.manifest.entries {
        if entry.kind != EntryKind::File {
            continue;
        }
        for block in &entry.blocks {
            let ino = u32::try_from(block.compressed_size)
                .map_err(|_| "the running package names an inode outside the image")?;
            local_slices
                .entry(block.content_hash)
                .or_insert((ino, block.payload_offset, block.original_size));
        }
    }

    let mut copies = Vec::new();
    let mut taken = Vec::new();
    for entry in &remote.manifest.entries {
        if entry.kind != EntryKind::File {
            continue;
        }
        for block in &entry.blocks {
            let len = block_original_size(block)
                .map_err(|e| format!("release: {e}"))? as u64;
            let Some(&(ino_l, off_l, len_l)) = local_slices.get(&block.content_hash) else {
                continue;
            };
            if len_l != len {
                continue;
            }
            let ino_r = u32::try_from(block.compressed_size)
                .map_err(|_| "the release names an inode outside the image")?;
            let dst = slice_stored_ranges(
                &remote_reader,
                remote.footer.payload_offset,
                ino_r,
                block.payload_offset,
                len,
                "the release",
            )?;
            let srcs = slice_stored_ranges(
                local_reader,
                local.footer.payload_offset,
                ino_l,
                off_l,
                len,
                "the running package",
            )?;
            let ranges = zip_ranges(dst, srcs)?;
            taken.extend(ranges.iter().map(|r| r.dst..r.dst + r.len));
            copies.push(CopyGroup {
                ranges,
                hash: block.content_hash,
                raw: true,
                original: len,
            });
        }
    }
    let fetch = complement(
        remote.footer.payload_offset,
        remote.footer.payload_offset + remote.footer.payload_size,
        taken,
    );

    let plan = Plan {
        prefix: prefix_plan(&local.footer, &local.manifest, remote),
        dict: None,
        copies,
        fetch,
    };
    Ok(plan)
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

fn copy_run(
    from: &std::fs::File,
    to: &std::fs::File,
    src: u64,
    dst: u64,
    len: u64,
) -> Result<(), String> {
    let mut buf = vec![0u8; 1024 * 1024];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(buf.len() as u64) as usize;
        from.read_exact_at(&mut buf[..n], src + done)
            .map_err(|e| format!("read the running binary at {}: {e}", src + done))?;
        to.write_all_at(&buf[..n], dst + done)
            .map_err(|e| format!("write the staged update at {}: {e}", dst + done))?;
        done += n as u64;
    }
    Ok(())
}

fn fetch_span(
    src: &dyn RangeSource,
    staged: &std::fs::File,
    start: u64,
    len: u64,
) -> Result<u64, String> {
    if len == 0 {
        return Ok(0);
    }
    let mut reader = src
        .fetch_range(start, len)
        .map_err(|e| format!("fetch bytes {start}-{}: {e}", start + len - 1))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut done = 0;
    while done < len {
        let want = ((len - done) as usize).min(buf.len());
        reader
            .read_exact(&mut buf[..want])
            .map_err(|e| format!("read bytes at {}: {e}", start + done))?;
        staged
            .write_all_at(&buf[..want], start + done)
            .map_err(|e| format!("write the staged update at {}: {e}", start + done))?;
        done += want as u64;
    }
    Ok(len)
}

/// Copy one group, hashing what was copied. A hash mismatch degrades the
/// group to fetches, so encoder drift never fails an update outright;
/// the signature stays the final word on the assembled bytes.
fn write_group(
    staged: &std::fs::File,
    local_file: &std::fs::File,
    group: &CopyGroup,
    dict: Option<&[u8]>,
    src: &dyn RangeSource,
    fetched: &mut u64,
) -> Result<bool, String> {
    let total: u64 = group.ranges.iter().map(|r| r.len).sum();
    if total == 0 {
        return Ok(true);
    }
    let mut bytes = Vec::with_capacity(total as usize);
    for range in &group.ranges {
        let at = bytes.len();
        bytes.resize(at + range.len as usize, 0);
        local_file
            .read_exact_at(&mut bytes[at..], range.src)
            .map_err(|e| format!("read the running binary at {}: {e}", range.src))?;
    }
    let ok = if group.raw {
        blake3::hash(&bytes).as_bytes() == &group.hash
    } else {
        match crate::loader::decode_block(&bytes, group.original as usize, dict) {
            Ok(decoded) => blake3::hash(&decoded).as_bytes() == &group.hash,
            Err(_) => false,
        }
    };
    if !ok {
        for range in &group.ranges {
            *fetched += fetch_span(src, staged, range.dst, range.len)?;
        }
        return Ok(false);
    }
    let mut at = 0usize;
    for range in &group.ranges {
        staged
            .write_all_at(&bytes[at..at + range.len as usize], range.dst)
            .map_err(|e| format!("write the staged update at {}: {e}", range.dst))?;
        at += range.len as usize;
    }
    Ok(true)
}

fn write_plan(
    staged: &std::fs::File,
    local_file: &std::fs::File,
    remote: &RemotePackage,
    plan: &Plan,
    dict: Option<&[u8]>,
    src: &dyn RangeSource,
) -> Result<Stats, String> {
    staged
        .set_len(remote.size)
        .map_err(|e| format!("size the staged update: {e}"))?;
    let mut fetched = 0;
    match plan.prefix {
        Prefix::Copy { len } => copy_run(local_file, staged, 0, 0, len)?,
        Prefix::Fetch { len } => fetched += fetch_span(src, staged, 0, len)?,
    }
    staged
        .write_all_at(&remote.manifest_bytes, remote.footer.manifest_offset)
        .map_err(|e| format!("write the staged manifest: {e}"))?;
    if let Some(dict) = &plan.dict {
        staged
            .write_all_at(dict, remote.footer.dict_offset)
            .map_err(|e| format!("write the staged dictionary: {e}"))?;
    }
    let mut reused = 0;
    for group in &plan.copies {
        if write_group(staged, local_file, group, dict, src, &mut fetched)? {
            reused += 1;
        }
    }
    for span in &plan.fetch {
        fetched += fetch_span(src, staged, span.start, span.len)?;
    }
    staged
        .write_all_at(
            &remote.footer_bytes,
            remote.size - FOOTER_SIZE as u64,
        )
        .map_err(|e| format!("write the staged footer: {e}"))?;
    Ok(Stats { reused, fetched })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::range::{FetchError, probe};
    use onelf_format::footer::FORMAT_VERSION;
    use onelf_format::{Block, Entry, ManifestHeader};
    use std::sync::Arc;

    /// An in-memory release: the stand-in for a release server.
    struct FakeSource {
        data: Arc<Vec<u8>>,
    }

    impl RangeSource for FakeSource {
        fn size(&self) -> Result<u64, FetchError> {
            Ok(self.data.len() as u64)
        }

        fn fetch_range(&self, start: u64, len: u64) -> Result<Box<dyn Read + '_>, FetchError> {
            let end = start
                .checked_add(len)
                .filter(|&e| e <= self.data.len() as u64)
                .ok_or_else(|| {
                    FetchError::Other(format!("range {start}-{} out of bounds", start + len))
                })?;
            Ok(Box::new(std::io::Cursor::new(
                self.data[start as usize..end as usize].to_vec(),
            )))
        }
    }

    fn blake3(data: &[u8]) -> [u8; 32] {
        *blake3::hash(data).as_bytes()
    }

    /// A stipplefs byte source over a file, standing in for the `fs`
    /// feature's FileSource, which this crate does not enable.
    struct LocalFile {
        file: std::fs::File,
        size: u64,
    }

    impl LocalFile {
        fn open(path: &std::path::Path) -> stipplefs::error::Result<Self> {
            let file = std::fs::File::open(path).map_err(stipplefs::error::Error::Io)?;
            let size = file
                .metadata()
                .map_err(stipplefs::error::Error::Io)?
                .len();
            Ok(LocalFile { file, size })
        }
    }

    impl ByteSource for LocalFile {
        fn size(&self) -> u64 {
            self.size
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> stipplefs::error::Result<()> {
            use std::os::unix::fs::FileExt;
            self.file
                .read_exact_at(buf, offset)
                .map_err(stipplefs::error::Error::Io)
        }
    }

    /// A fake runtime prefix. The planner only ever hashes these bytes.
    fn fake_prefix(tag: u8) -> Vec<u8> {
        let mut prefix = vec![0u8; 4096];
        prefix[..4].copy_from_slice(b"\x7fELF");
        prefix[16] = tag;
        prefix
    }

    fn string_table_for(files: &[(&str, Vec<u8>)]) -> (Vec<u8>, Vec<u32>) {
        let mut string_table = Vec::new();
        let mut offsets = Vec::new();
        for (name, _) in files {
            offsets.push(string_table.len() as u32);
            string_table.extend_from_slice(name.as_bytes());
            string_table.push(0);
        }
        (string_table, offsets)
    }

    fn footer_for(
        flags: Flags,
        prefix_len: usize,
        manifest_compressed: usize,
        manifest_original: usize,
        payload: &[u8],
        dict: Option<&[u8]>,
        checksum: [u8; 4],
    ) -> Footer {
        let manifest_offset = prefix_len as u64;
        let payload_offset = manifest_offset + manifest_compressed as u64;
        Footer {
            format_version: FORMAT_VERSION,
            flags,
            manifest_offset,
            manifest_compressed: manifest_compressed as u64,
            manifest_original: manifest_original as u64,
            payload_offset,
            payload_size: payload.len() as u64,
            dict_offset: payload_offset + payload.len() as u64,
            dict_size: dict.map_or(0, |d| d.len() as u32),
            manifest_checksum: checksum,
        }
    }

    fn finish_package(
        prefix: &[u8],
        manifest: &Manifest,
        payload: &[u8],
        dict: Option<&[u8]>,
        flags: Flags,
    ) -> Vec<u8> {
        let raw = manifest.serialize().expect("serialize manifest");
        let compressed = zstd::bulk::compress(&raw, 1).expect("compress manifest");
        let footer = footer_for(
            flags,
            prefix.len(),
            compressed.len(),
            raw.len(),
            payload,
            dict,
            onelf_format::manifest::manifest_checksum(&raw).expect("checksum"),
        );
        let mut out = Vec::with_capacity(prefix.len() + compressed.len() + payload.len() + 76);
        out.extend_from_slice(prefix);
        out.extend_from_slice(&compressed);
        out.extend_from_slice(payload);
        if let Some(d) = dict {
            out.extend_from_slice(d);
        }
        footer.write_to(&mut out).expect("write footer");
        out
    }

    /// Build a blocks-mode package in the real onelf layout:
    /// `[prefix][manifest][payload][dict][footer]`.
    fn build_blocks_package(prefix: &[u8], files: &[(&str, Vec<u8>)], dict: Option<&[u8]>) -> Vec<u8> {
        let (string_table, name_offsets) = string_table_for(files);
        let mut payload = Vec::new();
        let mut entries = Vec::new();
        for (i, (_, content)) in files.iter().enumerate() {
            let mut blocks = Vec::new();
            for chunk in content.chunks(64 * 1024) {
                let compressed = match dict {
                    Some(d) => zstd::bulk::Compressor::with_dictionary(3, d)
                        .expect("dict compressor")
                        .compress(chunk)
                        .expect("compress block"),
                    None => zstd::bulk::compress(chunk, 3).expect("compress block"),
                };
                blocks.push(Block {
                    payload_offset: payload.len() as u64,
                    compressed_size: compressed.len() as u64,
                    original_size: chunk.len() as u64,
                    content_hash: blake3(chunk),
                });
                payload.extend_from_slice(&compressed);
            }
            entries.push(Entry {
                kind: EntryKind::File,
                parent: u32::MAX,
                name: name_offsets[i],
                mode: 0o755,
                mtime_secs: 0,
                mtime_nsec: 0,
                content_hash: blake3(content),
                blocks,
                symlink_target: 0,
            });
        }
        let manifest = Manifest {
            header: ManifestHeader {
                version: 3,
                entry_count: entries.len() as u32,
                string_table_size: string_table.len() as u32,
                entrypoint_count: 0,
                default_entrypoint: 0,
                lib_dir_count: 0,
                name_offset: 0,
                package_id: [0; 32],
                prefix_hash: blake3(prefix),
                footer_hash: [0; 32],
            },
            entrypoints: Vec::new(),
            entries,
            lib_dir_offsets: Vec::new(),
            string_table,
        };
        let flags = if dict.is_some() {
            Flags::HAS_DICT
        } else {
            Flags::empty()
        };
        finish_package(prefix, &manifest, &payload, dict, flags)
    }

    /// Build a `STIPPLE_PAYLOAD` package the way the packer does: write
    /// the stipplefs image, then describe it with one manifest slice per
    /// 64 KiB of every file.
    fn build_stipple_package(prefix: &[u8], files: &[(&str, Vec<u8>)], stored_only: bool) -> Vec<u8> {
        use stipplefs::format::Codec;
        use stipplefs::write::{Builder, FileOrder, Node, Options, Timestamps};

        let codecs = if stored_only {
            Codec::Stored.bit()
        } else {
            Codec::Stored.bit()
                | Codec::Zstd.bit()
                | Codec::ZstdDict.bit()
                | Codec::ZstdBcj.bit()
                | Codec::ZstdDictBcj.bit()
        };
        let mut builder = Builder::new(Options {
            block_size: 64 * 1024,
            chunk_average: 4096,
            codecs,
            level: 3,
            dictionary_size: if stored_only { 0 } else { 112 * 1024 },
            threads: 1,
            memory_limit: None,
            timestamps: Timestamps::Clamp,
            compress_metadata: !stored_only,
        })
        .expect("stipple builder");
        builder.set_order(FileOrder::Extension);
        for (name, content) in files {
            builder
                .insert(
                    name.as_bytes(),
                    Node::File {
                        content: content.clone(),
                        mode: 0o755,
                        uid: 0,
                        gid: 0,
                        link_group: None,
                    },
                )
                .expect("insert file");
        }
        let mut payload = Vec::new();
        builder.build_into(&mut payload).expect("build image");

        let image_path = std::env::temp_dir().join(format!(
            "onelf-delta-image-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::write(&image_path, &payload).expect("write image");
        let image = stipplefs::read::Image::new(
            LocalFile::open(&image_path).expect("open image"),
            0,
            payload.len() as u64,
        )
        .expect("open image");
        let reader = image.reader().expect("image reader");
        let mut buf = vec![0u8; onelf_format::payload::SLICE_SIZE as usize];

        let (string_table, name_offsets) = string_table_for(files);
        let mut entries = Vec::new();
        for (i, (name, _)) in files.iter().enumerate() {
            let ino = reader
                .resolve(name.as_bytes())
                .unwrap_or_else(|| panic!("{name} missing from the image"));
            let size = reader.inode(ino).map_or(0, |n| n.size);
            let mut whole = blake3::Hasher::new();
            let mut blocks = Vec::new();
            let mut offset = 0;
            while offset < size {
                let n = reader.read(ino, offset, &mut buf).expect("read slice");
                assert!(n > 0, "{name}: image ends the file early");
                let slice = &buf[..n];
                whole.update(slice);
                blocks.push(Block {
                    payload_offset: offset,
                    compressed_size: u64::from(ino),
                    original_size: n as u64,
                    content_hash: blake3(slice),
                });
                offset += n as u64;
            }
            entries.push(Entry {
                kind: EntryKind::File,
                parent: u32::MAX,
                name: name_offsets[i],
                mode: 0o755,
                mtime_secs: 0,
                mtime_nsec: 0,
                content_hash: whole.finalize().into(),
                blocks,
                symlink_target: 0,
            });
        }
        drop(reader);
        drop(image);
        let _ = std::fs::remove_file(&image_path);

        let manifest = Manifest {
            header: ManifestHeader {
                version: 3,
                entry_count: entries.len() as u32,
                string_table_size: string_table.len() as u32,
                entrypoint_count: 0,
                default_entrypoint: 0,
                lib_dir_count: 0,
                name_offset: 0,
                package_id: [0; 32],
                prefix_hash: blake3(prefix),
                footer_hash: [0; 32],
            },
            entrypoints: Vec::new(),
            entries,
            lib_dir_offsets: Vec::new(),
            string_table,
        };
        finish_package(
            prefix,
            &manifest,
            &payload,
            None,
            Flags::STIPPLE_PAYLOAD | Flags::STORED,
        )
    }

    /// A scratch directory holding the running binary, the release, and
    /// the staging file one update writes.
    struct Stage {
        dir: std::path::PathBuf,
    }

    impl Stage {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "onelf-delta-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).expect("scratch dir");
            Stage { dir }
        }

        /// Probe `remote_bytes` as the updater would, assemble it from
        /// `local_bytes`, and hand back the staged bytes, the stats and
        /// the probe result.
        fn run(
            &self,
            local_bytes: &[u8],
            remote_bytes: &[u8],
        ) -> Result<(Vec<u8>, Stats, RemotePackage), String> {
            let local_path = self.dir.join("local");
            std::fs::write(&local_path, local_bytes).expect("write local");
            let src = FakeSource {
                data: Arc::new(remote_bytes.to_vec()),
            };
            let remote = probe(&src).expect("probe release");
            let staged_path = self.dir.join("staged");
            let staged = std::fs::File::create_new(&staged_path).expect("claim staging file");
            let stats = assemble(&local_path, &remote, &staged, &src);
            drop(staged);
            let out = std::fs::read(&staged_path).expect("read staged file");
            stats.map(|s| (out, s, remote))
        }

        /// The error `assemble` names, failing the test when it succeeds.
        fn run_err(&self, local_bytes: &[u8], remote_bytes: &[u8]) -> String {
            match self.run(local_bytes, remote_bytes) {
                Err(e) => e,
                Ok(_) => panic!("assembly should have been refused"),
            }
        }
    }

    impl Drop for Stage {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn identical_blocks_packages_fetch_nothing() {
        let stage = Stage::new("identical");
        let prefix = fake_prefix(1);
        let app = vec![b'a'; 200_000];
        let libs: Vec<u8> = (0..100_000u32).map(|i| i as u8).collect();
        let refs: Vec<(&str, Vec<u8>)> = vec![("bin/app", app), ("lib/x.so", libs)];
        let package = build_blocks_package(&prefix, &refs, None);

        let (out, stats, _) = stage.run(&package, &package).expect("assemble");
        assert_eq!(out, package, "staged bytes must equal the release");
        assert_eq!(stats.fetched, 0, "identical packages need no download");
        assert_eq!(
            stats.reused, 6,
            "every block copies from the running binary"
        );
    }

    #[test]
    fn one_changed_block_fetches_only_that_block() {
        let stage = Stage::new("one-block");
        let prefix = fake_prefix(2);
        let mut app = vec![b'a'; 200_000];
        let libs = (0..100_000u32).map(|i| i as u8).collect::<Vec<u8>>();
        let local = build_blocks_package(&prefix, &[("bin/app", app.clone()), ("lib/x.so", libs.clone())], None);

        app[100_000] = b'z';
        let remote = build_blocks_package(&prefix, &[("bin/app", app), ("lib/x.so", libs)], None);

        let (out, stats, remote_pkg) = stage.run(&local, &remote).expect("assemble");
        assert_eq!(out, remote);
        assert_eq!(stats.reused, 5, "five of six blocks are unchanged");
        assert!(
            stats.fetched > 0 && stats.fetched < 4096,
            "one small block changes, fetched {} bytes",
            stats.fetched
        );
        assert!(stats.fetched < remote_pkg.footer.payload_size);
    }

    #[test]
    fn a_new_runtime_prefix_is_fetched_and_its_blocks_copied() {
        let stage = Stage::new("prefix");
        let refs: Vec<(&str, Vec<u8>)> = vec![("bin/app", vec![b'q'; 150_000])];
        let local = build_blocks_package(&fake_prefix(3), &refs, None);
        let remote = build_blocks_package(&fake_prefix(4), &refs, None);

        let (out, stats, _) = stage.run(&local, &remote).expect("assemble");
        assert_eq!(out, remote);
        assert_eq!(stats.reused, 3, "all payload blocks are unchanged");
        assert_eq!(stats.fetched, 4096, "only the runtime prefix is fetched");
    }

    #[test]
    fn a_dictionary_change_refetches_every_block() {
        let stage = Stage::new("dict");
        let prefix = fake_prefix(5);
        let refs: Vec<(&str, Vec<u8>)> = vec![("bin/app", vec![b'w'; 80_000])];
        let local = build_blocks_package(&prefix, &refs, Some(b"local raw content dictionary"));
        let remote = build_blocks_package(&prefix, &refs, Some(b"remote raw content dictionary"));

        let (out, stats, remote_pkg) = stage.run(&local, &remote).expect("assemble");
        assert_eq!(out, remote);
        assert_eq!(stats.reused, 0);
        assert_eq!(
            stats.fetched, remote_pkg.footer.payload_size,
            "compressed bytes are only reproducible under one dictionary"
        );
    }

    #[test]
    fn stored_stipple_images_delta_across_builds() {
        let stage = Stage::new("stipple-stored");
        let prefix = fake_prefix(6);
        let stable: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let local = build_stipple_package(
            &prefix,
            &[("bin/app", stable.clone()), ("share/data", vec![1u8; 50_000])],
            true,
        );
        let remote = build_stipple_package(
            &prefix,
            &[("bin/app", stable), ("share/data", vec![9u8; 50_000])],
            true,
        );

        let (out, stats, remote_pkg) = stage.run(&local, &remote).expect("assemble");
        assert_eq!(out, remote);
        assert!(stats.reused > 0, "the unchanged file's slices are reused");
        assert!(
            stats.fetched < remote_pkg.footer.payload_size,
            "fetched {} of {} payload bytes",
            stats.fetched,
            remote_pkg.footer.payload_size
        );
    }

    #[test]
    fn compressed_stipple_images_are_refused() {
        let stage = Stage::new("stipple-zstd");
        let prefix = fake_prefix(7);
        let local = build_stipple_package(
            &prefix,
            &[("bin/app", (0..300_000u32).map(|i| (i * 3 % 251) as u8).collect())],
            false,
        );
        let remote = build_stipple_package(
            &prefix,
            &[("bin/app", (0..300_000u32).map(|i| (i * 5 % 251) as u8).collect())],
            false,
        );

        let err = stage.run_err(&local, &remote);
        assert!(
            err.contains("compress"),
            "the refusal should name the cause: {err}"
        );
    }

    #[test]
    fn a_stipple_prefix_change_fetches_only_the_prefix() {
        let stage = Stage::new("stipple-prefix");
        let refs: Vec<(&str, Vec<u8>)> =
            vec![("bin/app", (0..100_000u32).map(|i| (i * 11 % 251) as u8).collect())];
        let local = build_stipple_package(&fake_prefix(8), &refs, true);
        let remote = build_stipple_package(&fake_prefix(9), &refs, true);

        let (out, stats, remote_pkg) = stage.run(&local, &remote).expect("assemble");
        assert_eq!(out, remote);
        assert_eq!(
            stats.reused, 2,
            "both slices copy from the running binary"
        );
        // The prefix is fetched, and so is the image's own bookkeeping:
        // superblock and metadata sections sit outside the onelf manifest,
        // so no slice can describe them. They are small and fixed-size.
        assert!(stats.fetched > 4096, "the prefix alone is {}", stats.fetched);
        assert!(
            stats.fetched < remote_pkg.footer.payload_size,
            "fetched {} of {} payload bytes: content is still copied",
            stats.fetched,
            remote_pkg.footer.payload_size
        );
    }

    #[test]
    fn changing_the_payload_kind_is_refused() {
        let stage = Stage::new("kind");
        let prefix = fake_prefix(10);
        let local = build_blocks_package(&prefix, &[("bin/app", vec![b'x'; 4096])], None);
        let remote = build_stipple_package(&prefix, &[("bin/app", vec![b'y'; 4096])], true);

        let err = stage.run_err(&local, &remote);
        assert!(err.contains("payload kind"), "got: {err}");
    }
}
