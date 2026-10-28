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
//!   Fully stored images (built with `--no-compress`) copy at slice
//!   granularity through the extent table, which follows the content
//!   wherever the image's layout put it. Compressed images copy whole
//!   blocks, matched by the digests in the block index, and need
//!   content-defined blocks on both sides: a block cut at a fixed stream
//!   offset changes whenever anything before it grows or shrinks, so its
//!   compressed bytes are not stable across builds. A block coded against
//!   the image's dictionary, which is retrained for every build, never
//!   matches either.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::OnceLock;

use onelf_format::payload::Codec;
use onelf_format::reader::{block_extent, block_original_size};
use onelf_format::{EntryKind, FOOTER_SIZE, Flags, Footer, Manifest};
use stipplefs::format::Section;
use stipplefs::read::{Image, Reader};
use stipplefs::source::ByteSource;

use super::range::{RangeSource, RemotePackage};

/// What one assembly reused and what it had to download.
pub(super) struct Stats {
    /// Copy groups taken from the running binary.
    pub reused: usize,
    /// What a copy group is: a block, or a stipplefs slice.
    pub unit: &'static str,
    /// Bytes fetched while assembling: the prefix when it is not copied,
    /// every changed span, and each copy that failed its hash and was
    /// fetched instead. The footer, manifest, dictionary and stipplefs
    /// metadata are fetched before assembly and not counted.
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
    let mut plan = if remote.footer.is_stipple() {
        plan_stipple(&local, remote, src)?
    } else {
        plan_blocks(&local, remote, src)?
    };
    coalesce(&mut plan);
    let dict = plan.dict.as_deref();
    let (reused, fetched) = write_plan(staged, &local.file, remote, &plan, dict, src)?;
    Ok(Stats {
        reused,
        unit: plan.unit,
        fetched,
    })
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
    check: Check,
}

/// What the copied bytes of a group must prove before they are written.
enum Check {
    /// The bytes are the content (stored payloads) and hash to this BLAKE3.
    Raw([u8; 32]),
    /// The bytes are a compressed block whose decoded content hashes to
    /// `hash`. `original` is its decompressed size, already passed through
    /// the loader's ceiling.
    Decoded { hash: [u8; 32], original: usize },
    /// The bytes are a stipplefs block's stored bytes and the digest that
    /// follows them, and both match the release's block index.
    Block(u64),
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
    /// Stored stipplefs blocks, as absolute start and stored length. Each
    /// is followed by an 8-byte digest of its bytes, computed from the
    /// staged file once every byte is in place rather than fetched: a
    /// request per block would cost more than all the content.
    digests: Vec<(u64, u64)>,
    /// What a copy group is, for the summary: a block or a slice.
    unit: &'static str,
    /// Bytes the planner already fetched, written as they are: a stipplefs
    /// release image's superblock and metadata, at their absolute offset.
    known: Option<(u64, Vec<u8>)>,
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
    // A trained dictionary is kilobytes; a footer claiming more than this
    // is hostile, and the allocation would happen before any content
    // check could refuse it.
    const DICT_MAX_BYTES: u64 = 64 * 1024 * 1024;
    if footer.dict_size as u64 > DICT_MAX_BYTES {
        return Err(format!(
            "the release claims a {}-byte dictionary",
            footer.dict_size
        ));
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
            spans.push(Span {
                start: at,
                len: s - at,
            });
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

/// Largest run of reusable bytes worth downloading to save a request.
/// Fetching 128 KiB costs less than a round trip on most links, and a
/// redirecting host such as GitHub spends two round trips per request.
const FETCH_GAP_MAX: u64 = 128 * 1024;

/// Merge fetch spans separated by at most [`FETCH_GAP_MAX`] bytes, and
/// drop the copy groups a merged span now downloads anyway.
fn coalesce(plan: &mut Plan) {
    plan.fetch.sort_by_key(|s| s.start);
    let mut merged: Vec<Span> = Vec::with_capacity(plan.fetch.len());
    for span in plan.fetch.drain(..) {
        if let Some(last) = merged.last_mut() {
            let last_end = last.start + last.len;
            if span.start <= last_end.saturating_add(FETCH_GAP_MAX) {
                last.len = last_end.max(span.start + span.len) - last.start;
                continue;
            }
        }
        merged.push(span);
    }
    plan.fetch = merged;

    let fetch = &plan.fetch;
    let fetched = |range: &CopyRange| {
        let i = fetch.partition_point(|s| s.start <= range.dst);
        i > 0 && {
            let s = &fetch[i - 1];
            range.dst + range.len <= s.start + s.len
        }
    };
    plan.copies
        .retain(|group| !group.ranges.iter().all(fetched));
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
    if !dicts_equal {
        eprintln!(
            "onelf-rt: the release compresses against a different dictionary, so no \
             block can be copied from the running binary; fetching every block"
        );
    }
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
                    ranges: vec![CopyRange {
                        dst,
                        src: *src,
                        len,
                    }],
                    check: if remote.footer.is_stored() {
                        Check::Raw(block.content_hash)
                    } else {
                        Check::Decoded {
                            hash: block.content_hash,
                            // The loader's own ceiling, not the manifest's
                            // raw number: a crafted `original_size` must
                            // fail the plan cleanly instead of reaching the
                            // decoder's allocator.
                            original: onelf_format::reader::block_original_size(block)
                                .map_err(|e| format!("plan the delta: {e}"))?,
                        }
                    },
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
        digests: Vec::new(),
        unit: "blocks",
        known: None,
    };
    Ok(plan)
}

// ---------------------------------------------------------------------------
// Stipplefs payloads
// ---------------------------------------------------------------------------

/// Most metadata a release image may make the planner fetch and hold. The
/// largest tree measured carries about 100 KB of it; this leaves room for
/// millions of files while keeping a hostile image from exhausting memory
/// before its signature is checked.
const METADATA_MAX: u64 = 64 * 1024 * 1024;

/// A byte source served by range fetches, so the remote image's own
/// metadata parser can run against the release without downloading it.
struct FetchingSource<'a> {
    src: &'a dyn RangeSource,
    size: u64,
    /// The image's metadata sections, fetched in one request once the
    /// superblock names where they lie: an absolute start and its bytes.
    prefetched: &'a OnceLock<(u64, Vec<u8>)>,
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
        if let Some((start, bytes)) = self.prefetched.get()
            && let Some(at) = offset.checked_sub(*start)
            && let Some(cached) = bytes.get(at as usize..(at + len) as usize)
        {
            buf.copy_from_slice(cached);
            return Ok(());
        }
        let mut reader = self
            .src
            .fetch_range(offset, len)
            .map_err(|e| stipplefs::error::Error::Io(std::io::Error::other(e.to_string())))?;
        reader.read_exact(buf).map_err(stipplefs::error::Error::Io)
    }
}

/// The image-relative bytes from the image's start through every present
/// metadata section, up to the 4096-byte boundary the first data block
/// starts on, refused when they span more than [`METADATA_MAX`]. The
/// superblock has already checked that each section lies inside the image.
fn metadata_range(sections: &[Section], image_size: u64) -> Result<Option<Range<u64>>, String> {
    let Some(end) = sections
        .iter()
        .filter(|s| !s.is_absent())
        .map(|s| s.offset + s.size)
        .max()
    else {
        return Ok(None);
    };
    let end = end.div_ceil(4096).saturating_mul(4096).min(image_size);
    if end > METADATA_MAX {
        return Err(format!(
            "the release image's metadata spans {end} bytes, more than the {METADATA_MAX} \
             the updater fetches"
        ));
    }
    Ok(Some(0..end))
}

/// Every block in the image is stored raw, so content can be copied at
/// any granularity.
fn all_stored(reader: &Reader<'_>) -> bool {
    let blocks = reader.blocks();
    (0..blocks.count()).all(|i| {
        blocks
            .get(i)
            .is_some_and(|e| e.codec == stipplefs::format::Codec::Stored)
    })
}

/// The image ends its blocks where the content says to, so unchanged
/// content sits in identical blocks across builds.
fn variable_blocks(reader: &Reader<'_>) -> bool {
    reader.superblock().flags & stipplefs::format::FLAG_VARIABLE_BLOCKS != 0
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
    let end = file_off
        .checked_add(len)
        .ok_or_else(|| format!("{what}: the file range overflows the image"))?;
    let mut out = Vec::new();
    let mut covered = file_off;
    for (stream_off, ext_off, ext_len) in reader.file_extents(ino) {
        let ext_end = ext_off
            .checked_add(ext_len)
            .ok_or_else(|| format!("{what}: an extent overflows the image"))?;
        if ext_end <= covered {
            continue;
        }
        if ext_off > covered {
            return Err(format!("{what}: a hole sits before byte {covered}"));
        }
        let stop = end.min(ext_end);
        let mut at = stream_off
            .checked_add(covered - ext_off)
            .ok_or_else(|| format!("{what}: an extent position overflows the image"))?;
        let stream_end = stream_off
            .checked_add(stop - ext_off)
            .ok_or_else(|| format!("{what}: an extent end overflows the image"))?;
        while at < stream_end {
            let (index, within_block) = reader
                .blocks()
                .locate(at)
                .ok_or_else(|| format!("{what}: stream byte {at} is outside every block"))?;
            let entry = reader
                .blocks()
                .get(index)
                .ok_or_else(|| format!("{what}: block {index} is outside the block index"))?;
            if entry.codec != stipplefs::format::Codec::Stored {
                return Err(format!("{what}: a compressed block cannot be copied"));
            }
            let count = (u64::from(entry.decoded_length) - within_block).min(stream_end - at);
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

    // The remote image's metadata: the superblock in one request, then
    // every section it names in a second. No data block is read here.
    let prefetched = OnceLock::new();
    let source = FetchingSource {
        src,
        size: remote.size,
        prefetched: &prefetched,
    };
    let mut image = Image::new(
        source,
        remote.footer.payload_offset,
        remote.footer.payload_size,
    )
    .map_err(|e| format!("read the release image: {e}"))?;
    image.set_metadata_limit(METADATA_MAX);
    // The superblock is fetched a second time with the sections: four more
    // kilobytes, so the whole range is written from memory below.
    let metadata = metadata_range(&image.superblock().sections, remote.footer.payload_size)?;
    if let Some(range) = &metadata {
        let start = remote.footer.payload_offset + range.start;
        let len = range.end - range.start;
        let mut bytes = vec![0u8; len as usize];
        src.fetch_range(start, len)
            .map_err(|e| format!("fetch the release image's metadata: {e}"))?
            .read_exact(&mut bytes)
            .map_err(|e| format!("read the release image's metadata: {e}"))?;
        let _ = prefetched.set((start, bytes));
    }
    let remote_reader = image
        .reader()
        .map_err(|e| format!("read the release image: {e}"))?;

    let known = metadata
        .map(|r| remote.footer.payload_offset + r.start..remote.footer.payload_offset + r.end);
    let mut plan = match (all_stored(local_reader), all_stored(&remote_reader)) {
        (true, true) => plan_stipple_slices(local, local_reader, remote, &remote_reader, known),
        (false, false) if !variable_blocks(&remote_reader) => Err(
            "the release compresses fixed-size stipplefs blocks, whose bytes shift \
             between builds"
                .into(),
        ),
        (false, false) if !variable_blocks(local_reader) => Err(
            "the running package compresses fixed-size stipplefs blocks, which the \
             release's content-defined blocks cannot match"
                .into(),
        ),
        (false, false) => plan_stipple_blocks(local, local_reader, remote, &remote_reader, known),
        _ => Err("the update changes whether the stipplefs payload is compressed".into()),
    }?;
    drop(remote_reader);
    drop(image);
    plan.known = prefetched.into_inner();
    Ok(plan)
}

/// A fully stored image on both sides: copy content slice by slice through
/// each image's extent table.
fn plan_stipple_slices(
    local: &crate::loader::PackageData,
    local_reader: &Reader<'_>,
    remote: &RemotePackage,
    remote_reader: &Reader<'_>,
    known: Option<Range<u64>>,
) -> Result<Plan, String> {
    // Content hash to the local file's inode and file offset.
    let mut local_slices: HashMap<[u8; 32], (u32, u64, u64)> = HashMap::new();
    for entry in &local.manifest.entries {
        if entry.kind != EntryKind::File {
            continue;
        }
        for block in &entry.blocks {
            let ino = u32::try_from(block.compressed_size)
                .map_err(|_| "the running package names an inode outside the image")?;
            local_slices.entry(block.content_hash).or_insert((
                ino,
                block.payload_offset,
                block.original_size,
            ));
        }
    }

    let mut copies = Vec::new();
    let mut taken = Vec::new();
    // A slice is planned once however often the manifest lists it, so a
    // crafted manifest cannot multiply the copying and hashing done before
    // verification.
    let mut seen = HashSet::new();
    for entry in &remote.manifest.entries {
        if entry.kind != EntryKind::File {
            continue;
        }
        for block in &entry.blocks {
            let len = block_original_size(block).map_err(|e| format!("release: {e}"))? as u64;
            let Some(&(ino_l, off_l, len_l)) = local_slices.get(&block.content_hash) else {
                continue;
            };
            if len_l != len {
                continue;
            }
            let ino_r = u32::try_from(block.compressed_size)
                .map_err(|_| "the release names an inode outside the image")?;
            if !seen.insert((ino_r, block.payload_offset)) {
                continue;
            }
            let dst = slice_stored_ranges(
                remote_reader,
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
                check: Check::Raw(block.content_hash),
            });
        }
    }
    let mut digests = Vec::new();
    let block_size = u64::from(remote_reader.superblock().block_size);
    let payload_end = remote.footer.payload_offset + remote.footer.payload_size;
    let blocks = remote_reader.blocks();
    for i in 0..blocks.count() {
        let entry = blocks
            .get(i)
            .ok_or_else(|| format!("the release: block {i} is outside the block index"))?;
        let len = u64::from(entry.stored_length);
        let start = remote.footer.payload_offset.checked_add(entry.image_offset);
        let end = start.and_then(|s| s.checked_add(len));
        let (Some(start), Some(end)) = (start, end) else {
            return Err(format!("the release: block {i} overflows its image"));
        };
        if len > block_size || end.saturating_add(8) > payload_end {
            return Err(format!("the release: block {i} does not fit its image"));
        }
        taken.push(end..end + 8);
        digests.push((start, len));
    }
    taken.extend(known);
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
        digests,
        unit: "slices",
        known: None,
    };
    Ok(plan)
}

/// Content-defined compressed blocks on both sides: copy every release
/// block whose stored bytes the running package already holds, matched by
/// the digest, lengths and codec its block index records.
fn plan_stipple_blocks(
    local: &crate::loader::PackageData,
    local_reader: &Reader<'_>,
    remote: &RemotePackage,
    remote_reader: &Reader<'_>,
    known: Option<Range<u64>>,
) -> Result<Plan, String> {
    type Key = (u64, u32, u32, u8);
    let key = |e: &stipplefs::read::BlockEntry, what: &str| -> Result<Key, String> {
        let digest = e
            .digest
            .ok_or_else(|| format!("{what}: a block index entry records no digest"))?;
        Ok((digest, e.stored_length, e.decoded_length, e.codec.id()))
    };

    let mut local_blocks: HashMap<Key, u64> = HashMap::new();
    let blocks = local_reader.blocks();
    for i in 0..blocks.count() {
        let entry = blocks
            .get(i)
            .ok_or_else(|| format!("the running package: block {i} is outside its index"))?;
        local_blocks
            .entry(key(&entry, "the running package")?)
            .or_insert(local.footer.payload_offset + entry.image_offset);
    }

    let payload_start = remote.footer.payload_offset;
    let payload_end = payload_start + remote.footer.payload_size;
    let mut copies = Vec::new();
    let mut taken = Vec::new();
    // Release blocks must follow one another without overlapping, which
    // also bounds the copying planned here by the size of the payload, so
    // a crafted index cannot multiply it.
    let mut after = payload_start;
    let blocks = remote_reader.blocks();
    for i in 0..blocks.count() {
        let entry = blocks
            .get(i)
            .ok_or_else(|| format!("the release: block {i} is outside its index"))?;
        let len = u64::from(entry.stored_length) + 8;
        let dst = payload_start.checked_add(entry.image_offset);
        let fits = |d: u64| d >= after && d.checked_add(len).is_some_and(|e| e <= payload_end);
        let Some(dst) = dst.filter(|&d| fits(d)) else {
            return Err(format!(
                "the release: block {i} overlaps another or its image"
            ));
        };
        after = dst + len;
        if let Some(&src) = local_blocks.get(&key(&entry, "the release")?) {
            taken.push(dst..dst + len);
            copies.push(CopyGroup {
                ranges: vec![CopyRange { dst, src, len }],
                check: Check::Block(entry.digest.unwrap_or_default()),
            });
        }
    }

    taken.extend(known);
    Ok(Plan {
        prefix: prefix_plan(&local.footer, &local.manifest, remote),
        dict: None,
        copies,
        fetch: complement(payload_start, payload_end, taken),
        digests: Vec::new(),
        unit: "blocks",
        known: None,
    })
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// Copy one contiguous byte run, hashing the bytes en route. `dst` and
/// `src` are absolute package-file offsets; the caller compares the hash
/// against the manifest's claim before trusting what was copied.
fn copy_run(
    from: &std::fs::File,
    to: &std::fs::File,
    src: u64,
    dst: u64,
    len: u64,
    buf: &mut Vec<u8>,
) -> Result<[u8; 32], String> {
    let mut hasher = blake3::Hasher::new();
    buf.resize(1024 * 1024, 0);
    let mut done = 0;
    while done < len {
        let n = (len - done).min(buf.len() as u64) as usize;
        from.read_exact_at(&mut buf[..n], src + done)
            .map_err(|e| format!("read the running binary at {}: {e}", src + done))?;
        hasher.update(&buf[..n]);
        to.write_all_at(&buf[..n], dst + done)
            .map_err(|e| format!("write the staged update at {}: {e}", dst + done))?;
        done += n as u64;
    }
    Ok(*hasher.finalize().as_bytes())
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
/// the signature stays the final word on the assembled bytes. `bytes` is
/// scratch space reused across groups.
fn write_group(
    staged: &std::fs::File,
    local_file: &std::fs::File,
    group: &CopyGroup,
    dict: Option<&[u8]>,
    src: &dyn RangeSource,
    fetched: &mut u64,
    bytes: &mut Vec<u8>,
) -> Result<bool, String> {
    let total: u64 = group.ranges.iter().map(|r| r.len).sum();
    if total == 0 {
        return Ok(true);
    }
    bytes.clear();
    for range in &group.ranges {
        let at = bytes.len();
        bytes.resize(at + range.len as usize, 0);
        local_file
            .read_exact_at(&mut bytes[at..], range.src)
            .map_err(|e| format!("read the running binary at {}: {e}", range.src))?;
    }
    let ok = match group.check {
        Check::Raw(hash) => blake3::hash(bytes).as_bytes() == &hash,
        Check::Decoded { hash, original } => {
            match crate::loader::decode_block(bytes, original, dict) {
                Ok(decoded) => blake3::hash(&decoded).as_bytes() == &hash,
                Err(_) => false,
            }
        }
        Check::Block(digest) => {
            bytes.len() >= 8 && {
                let (stored, trailing) = bytes.split_at(bytes.len() - 8);
                trailing == digest.to_le_bytes() && xxhash_rust::xxh3::xxh3_64(stored) == digest
            }
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
) -> Result<(usize, u64), String> {
    staged
        .set_len(remote.size)
        .map_err(|e| format!("size the staged update: {e}"))?;
    let mut fetched = 0;
    let mut scratch = Vec::new();
    match plan.prefix {
        Prefix::Copy { len } => {
            // The planner picks a copy when both manifests pin the same
            // prefix_hash, but the running binary's actual bytes can
            // diverge from its own manifest through bit rot or an old
            // truncated write. The hash is checked against the release's
            // claim, and a mismatch fetches the prefix rather than
            // copying rot into the staged update.
            let hash = copy_run(local_file, staged, 0, 0, len, &mut scratch)?;
            if hash != remote.manifest.header.prefix_hash {
                eprintln!(
                    "onelf-rt: the running binary's prefix does not match its own manifest; \
                     fetching the release prefix"
                );
                fetched += fetch_span(src, staged, 0, len)?;
            }
        }
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
    if let Some((at, bytes)) = &plan.known {
        staged
            .write_all_at(bytes, *at)
            .map_err(|e| format!("write the staged image metadata: {e}"))?;
    }
    let mut reused = 0;
    for group in &plan.copies {
        if write_group(
            staged,
            local_file,
            group,
            dict,
            src,
            &mut fetched,
            &mut scratch,
        )? {
            reused += 1;
        }
    }
    for span in &plan.fetch {
        fetched += fetch_span(src, staged, span.start, span.len)?;
    }
    for &(start, len) in &plan.digests {
        scratch.resize(len as usize, 0);
        staged
            .read_exact_at(&mut scratch, start)
            .map_err(|e| format!("read the staged block at {start}: {e}"))?;
        let digest = xxhash_rust::xxh3::xxh3_64(&scratch).to_le_bytes();
        staged
            .write_all_at(&digest, start + len)
            .map_err(|e| format!("write the staged block digest at {}: {e}", start + len))?;
    }
    staged
        .write_all_at(&remote.footer_bytes, remote.size - FOOTER_SIZE as u64)
        .map_err(|e| format!("write the staged footer: {e}"))?;
    Ok((reused, fetched))
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

        fn fetch_whole(&self) -> Result<Box<dyn Read + '_>, FetchError> {
            Ok(Box::new(std::io::Cursor::new(self.data.to_vec())))
        }

        fn fetch_signature(&self) -> Result<Vec<u8>, FetchError> {
            Err(FetchError::Other("the fake release is unsigned".into()))
        }
    }

    /// A release whose range answers, whole-file body and signature can
    /// disagree, the way a corrupted transfer or a release replaced
    /// mid-update makes them disagree. Counts whole-file downloads.
    struct FakeRelease {
        ranged: Vec<u8>,
        whole: Vec<u8>,
        sig: Option<Vec<u8>>,
        whole_fetches: std::sync::atomic::AtomicUsize,
    }

    impl FakeRelease {
        fn new(ranged: Vec<u8>, whole: Vec<u8>, sig: Option<Vec<u8>>) -> Self {
            FakeRelease {
                ranged,
                whole,
                sig,
                whole_fetches: Default::default(),
            }
        }

        fn whole_fetches(&self) -> usize {
            self.whole_fetches
                .load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    impl RangeSource for FakeRelease {
        fn size(&self) -> Result<u64, FetchError> {
            Ok(self.ranged.len() as u64)
        }

        fn fetch_range(&self, start: u64, len: u64) -> Result<Box<dyn Read + '_>, FetchError> {
            let range = start as usize..(start + len) as usize;
            let bytes = self
                .ranged
                .get(range)
                .ok_or_else(|| FetchError::Other(format!("range {start}+{len} out of bounds")))?;
            Ok(Box::new(bytes))
        }

        fn fetch_whole(&self) -> Result<Box<dyn Read + '_>, FetchError> {
            self.whole_fetches
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(Box::new(self.whole.as_slice()))
        }

        fn fetch_signature(&self) -> Result<Vec<u8>, FetchError> {
            self.sig
                .clone()
                .ok_or_else(|| FetchError::Other("fetch signature: HTTP 404".into()))
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
            let size = file.metadata().map_err(stipplefs::error::Error::Io)?.len();
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
    fn build_blocks_package(
        prefix: &[u8],
        files: &[(&str, Vec<u8>)],
        dict: Option<&[u8]>,
    ) -> Vec<u8> {
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

    /// How a test stipplefs image stores its blocks.
    #[derive(Clone, Copy, PartialEq)]
    enum Layout {
        /// Every block stored raw, as `--no-compress` packs.
        Stored,
        /// Compressed fixed-size blocks against a trained dictionary, as
        /// packages built before content-defined blocks were.
        Fixed,
        /// Compressed content-defined blocks, as the packer now writes.
        Variable,
    }

    /// Build a `STIPPLE_PAYLOAD` package the way the packer does: write
    /// the stipplefs image, then describe it with one manifest slice per
    /// 64 KiB of every file.
    fn build_stipple_package(prefix: &[u8], files: &[(&str, Vec<u8>)], layout: Layout) -> Vec<u8> {
        use stipplefs::format::Codec;
        use stipplefs::write::{Builder, FileOrder, Node, Options, Timestamps};

        let stored_only = layout == Layout::Stored;
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
            dictionary_size: if layout == Layout::Fixed {
                112 * 1024
            } else {
                0
            },
            threads: 1,
            memory_limit: None,
            timestamps: Timestamps::Clamp,
            compress_metadata: !stored_only,
            variable_blocks: layout == Layout::Variable,
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
    fn unsigned_integrity_checks_support_blocks_dictionaries_and_stipple() {
        let stage = Stage::new("unsigned-codecs");
        let prefix = fake_prefix(12);
        let files = [("app", vec![b'x'; 150_000]), ("empty", Vec::new())];
        for package in [
            build_blocks_package(&prefix, &files, None),
            build_blocks_package(&prefix, &files, Some(b"shared dictionary")),
            build_stipple_package(&prefix, &files, Layout::Stored),
            build_stipple_package(&prefix, &files, Layout::Fixed),
            build_stipple_package(&prefix, &files, Layout::Variable),
        ] {
            let path = stage.dir.join("staged");
            std::fs::write(&path, &package).unwrap();
            let file = std::fs::File::open(&path).unwrap();
            super::super::verify_unsigned(&file).unwrap();

            // Authentic payload blocks with a false whole-entry hash
            // must still fail the entry-level check.
            let mut remote = probe(&FakeSource {
                data: Arc::new(package.clone()),
            })
            .unwrap();
            remote.manifest.entries[0].content_hash[0] ^= 1;
            let payload = &package[remote.footer.payload_offset as usize
                ..(remote.footer.payload_offset + remote.footer.payload_size) as usize];
            let dict = if remote.footer.dict_size > 0 {
                Some(
                    &package[remote.footer.dict_offset as usize
                        ..remote.footer.dict_offset as usize + remote.footer.dict_size as usize],
                )
            } else {
                None
            };
            let bad = finish_package(
                &prefix,
                &remote.manifest,
                payload,
                dict,
                remote.footer.flags,
            );
            std::fs::write(&path, bad).unwrap();
            assert!(
                super::super::verify_unsigned(&file)
                    .unwrap_err()
                    .contains("entry content hash")
            );
        }
    }

    #[test]
    fn unsigned_updates_refuse_payload_bytes_from_a_later_release() {
        let stage = Stage::new("unsigned-mixed");
        let prefix = fake_prefix(13);
        let local_path = stage.dir.join("local");
        let local = build_blocks_package(
            &prefix,
            &[("app", vec![b'a'; 32_000]), ("stable", vec![b's'; 32_000])],
            None,
        );
        let release = build_blocks_package(
            &prefix,
            &[("app", vec![b'b'; 32_000]), ("stable", vec![b's'; 32_000])],
            None,
        );
        let remote = probe(&FakeSource {
            data: Arc::new(release.clone()),
        })
        .unwrap();
        let mut changed = release;
        let block = &remote.manifest.entries[0].blocks[0];
        let (offset, len) = block_extent(&remote.footer, block).unwrap();
        let replacement = zstd::bulk::compress(&vec![b'c'; 32_000], 3).unwrap();
        assert_eq!(replacement.len(), len);
        changed[offset as usize..offset as usize + len].copy_from_slice(&replacement);
        std::fs::write(&local_path, local).unwrap();
        let path = stage.dir.join("staged");
        let file = super::super::claim_staging(&path).unwrap();
        let stats = assemble(
            &local_path,
            &remote,
            &file,
            &FakeSource {
                data: Arc::new(changed),
            },
        )
        .unwrap();
        assert!(stats.reused > 0);
        assert!(super::super::verify_unsigned(&file).is_err());
    }

    #[test]
    fn unsigned_updates_refuse_a_changed_runtime_prefix() {
        let stage = Stage::new("unsigned-prefix");
        let mut package =
            build_blocks_package(&fake_prefix(14), &[("app", vec![b'a'; 1000])], None);
        package[100] ^= 1;
        let path = stage.dir.join("staged");
        std::fs::write(&path, package).unwrap();
        assert!(
            super::super::verify_unsigned(&std::fs::File::open(path).unwrap())
                .unwrap_err()
                .contains("runtime prefix hash mismatch")
        );
    }

    #[test]
    fn identical_full_download_leaves_the_executable_and_rollback_untouched() {
        let stage = Stage::new("identical-install");
        let package = build_blocks_package(&fake_prefix(15), &[("app", vec![b'a'; 1000])], None);
        let installed = stage.dir.join("app");
        let prev = super::super::sibling_path(&installed, ".onelf-prev");
        let tmp = stage.dir.join("download");
        std::fs::write(&installed, &package).unwrap();
        std::fs::write(&prev, b"previous release").unwrap();
        std::fs::write(&tmp, &package).unwrap();
        let identity =
            super::super::file_identity(&std::fs::File::open(&installed).unwrap()).unwrap();
        let file = std::fs::File::open(&tmp).unwrap();
        let staged = super::super::pin_verified(tmp.clone(), file).unwrap();
        let local_id = super::super::local_package_id(&installed).unwrap();
        let lock = super::super::InstallLock::acquire(&installed).unwrap();
        assert_eq!(
            super::super::install_staged(&installed, &lock, Some(&local_id), None, staged),
            0
        );
        assert_eq!(std::fs::read(&installed).unwrap(), package);
        assert_eq!(std::fs::read(&prev).unwrap(), b"previous release");
        assert_eq!(
            super::super::file_identity(&std::fs::File::open(installed).unwrap()).unwrap(),
            identity
        );
        assert!(!tmp.exists());
    }

    #[test]
    fn a_symlink_to_the_verified_inode_is_not_installed() {
        let stage = Stage::new("symlink-install");
        let package = build_blocks_package(&fake_prefix(16), &[("app", vec![b'a'; 1000])], None);
        let installed = stage.dir.join("app");
        let tmp = stage.dir.join("download");
        let moved = stage.dir.join("moved");
        std::fs::write(&installed, b"running binary").unwrap();
        std::fs::write(&tmp, package).unwrap();
        let file = std::fs::File::open(&tmp).unwrap();
        let staged = super::super::pin_verified(tmp.clone(), file).unwrap();
        std::fs::rename(&tmp, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, &tmp).unwrap();
        let lock = super::super::InstallLock::acquire(&installed).unwrap();
        assert_eq!(
            super::super::install_staged(&installed, &lock, None, None, staged),
            2
        );
        assert_eq!(std::fs::read(installed).unwrap(), b"running binary");
    }

    #[test]
    fn a_downloaded_revision_that_does_not_parse_is_refused() {
        let stage = Stage::new("bad-revision");
        let package = build_blocks_package(
            &fake_prefix(18),
            &[(".onelf/update-revision", b"not a number".to_vec())],
            None,
        );
        let path = stage.dir.join("download");
        std::fs::write(&path, package).unwrap();
        let err = super::super::staged_info(&std::fs::File::open(&path).unwrap()).unwrap_err();
        assert!(err.contains("does not hold a number"), "{err}");
    }

    #[test]
    fn a_binary_replaced_after_the_lock_is_not_overwritten() {
        let stage = Stage::new("replaced-install");
        let package = build_blocks_package(&fake_prefix(19), &[("app", vec![b'a'; 1000])], None);
        let installed = stage.dir.join("app");
        let tmp = stage.dir.join("download");
        std::fs::write(&installed, b"running binary").unwrap();
        std::fs::write(&tmp, package).unwrap();
        let lock = super::super::InstallLock::acquire(&installed).unwrap();
        // Something else installs over the name while the update runs.
        let other = stage.dir.join("other");
        std::fs::write(&other, b"someone else's install").unwrap();
        std::fs::rename(&other, &installed).unwrap();

        let staged =
            super::super::pin_verified(tmp.clone(), std::fs::File::open(&tmp).unwrap()).unwrap();
        assert_eq!(
            super::super::install_staged(&installed, &lock, None, None, staged),
            2
        );
        assert_eq!(
            std::fs::read(&installed).unwrap(),
            b"someone else's install"
        );
        assert!(!super::super::sibling_path(&installed, ".onelf-prev").exists());
        assert!(!tmp.exists());
    }

    #[test]
    fn a_revision_that_cannot_be_read_is_refused_not_skipped() {
        let stage = Stage::new("unreadable-revision");
        let package = build_blocks_package(
            &fake_prefix(24),
            &[(".onelf/update-revision", b"7".to_vec())],
            None,
        );
        // Wreck the entry's compressed block so it no longer decodes.
        let remote = probe(&FakeSource {
            data: Arc::new(package.clone()),
        })
        .unwrap();
        let (offset, len) =
            block_extent(&remote.footer, &remote.manifest.entries[0].blocks[0]).unwrap();
        let mut bad = package;
        bad[offset as usize..offset as usize + len].fill(0xff);
        let path = stage.dir.join("download");
        std::fs::write(&path, bad).unwrap();

        let err = super::super::staged_info(&std::fs::File::open(&path).unwrap()).unwrap_err();
        assert!(err.contains("cannot be read"), "{err}");
    }

    #[test]
    fn staged_metadata_comes_from_the_verified_handle_after_a_path_swap() {
        let stage = Stage::new("metadata-swap");
        let package = build_blocks_package(&fake_prefix(17), &[("app", vec![b'a'; 1000])], None);
        let tmp = stage.dir.join("download");
        std::fs::write(&tmp, package).unwrap();
        let file = std::fs::File::open(&tmp).unwrap();
        let expected = super::super::staged_info(&file).unwrap();
        std::fs::remove_file(&tmp).unwrap();
        std::fs::write(&tmp, b"unverified replacement").unwrap();
        assert_eq!(super::super::staged_info(&file).unwrap(), expected);
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
        let local = build_blocks_package(
            &prefix,
            &[("bin/app", app.clone()), ("lib/x.so", libs.clone())],
            None,
        );

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

    /// The running binary's own manifest can lie about its bytes: bit
    /// rot, an ancient truncated write, tampering. The planner trusts the
    /// manifest when it picks a copy source, so the writer's per-group
    /// re-hash is what catches the divergence and fetches the true bytes
    /// instead.
    #[test]
    fn a_local_byte_that_disagrees_with_its_manifest_is_refetched() {
        let stage = Stage::new("degrade");
        let prefix = fake_prefix(3);
        let app = vec![b'a'; 200_000];
        // Incompressible content, so the last block's compressed bytes are
        // far longer than the corrupted span below.
        let libs: Vec<u8> = (0..100_000u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        let package = build_blocks_package(&prefix, &[("bin/app", app), ("lib/x.so", libs)], None);

        // Flip one byte near the end of the payload: inside the last
        // block's compressed bytes, under a manifest that still claims
        // the original hash for it.
        let mut local = package.clone();
        let payload_end = local.len() - FOOTER_SIZE;
        local[payload_end - 500] ^= 0xff;

        let (out, stats, _) = stage.run(&local, &package).expect("assemble");
        assert_eq!(
            out, package,
            "the degraded fetch must still assemble the release"
        );
        assert_eq!(
            stats.reused, 5,
            "five groups verify, the corrupted one degrades to a fetch"
        );
        assert!(
            stats.fetched > 0 && stats.fetched < 100_000,
            "only the corrupted block is re-fetched, got {} bytes",
            stats.fetched
        );
    }

    #[test]
    fn a_local_prefix_byte_that_disagrees_with_its_manifest_is_refetched() {
        let stage = Stage::new("prefix-rot");
        let refs: Vec<(&str, Vec<u8>)> = vec![("bin/app", vec![b'a'; 200_000])];
        let package = build_blocks_package(&fake_prefix(5), &refs, None);

        // Flip one byte inside the runtime prefix, under a manifest that
        // still claims the original prefix_hash. The writer must catch
        // the divergence and fetch the release prefix instead of copying
        // the rot into the staged update.
        let mut local = package.clone();
        local[100] ^= 0xff;

        let (out, stats, remote_pkg) = stage.run(&local, &package).expect("assemble");
        assert_eq!(
            out, package,
            "the degraded prefix fetch must still assemble the release"
        );
        assert_eq!(
            stats.fetched, remote_pkg.footer.manifest_offset,
            "exactly the runtime prefix is re-fetched"
        );
    }

    #[test]
    fn a_dictionary_past_the_ceiling_is_refused_before_any_fetch() {
        let src = FakeSource {
            data: Arc::new(vec![0u8; 16]),
        };
        let footer = Footer {
            format_version: FORMAT_VERSION,
            flags: Flags::HAS_DICT,
            manifest_offset: 0,
            manifest_compressed: 0,
            manifest_original: 0,
            payload_offset: 0,
            payload_size: 0,
            dict_offset: 0,
            dict_size: 64 * 1024 * 1024 + 1,
            manifest_checksum: [0; 4],
        };

        // The cap fires from the footer alone: no fetch happens, so the
        // 16-byte source, which cannot satisfy one, is never asked.
        let err = fetch_dict(&src, &footer).unwrap_err();
        assert!(
            err.contains("claims a 67108865-byte dictionary"),
            "the ceiling must refuse before any fetch, got: {err}"
        );

        // A dictionary under the cap is fetched as usual.
        let mut small = footer;
        small.dict_size = 16;
        assert_eq!(fetch_dict(&src, &small).unwrap(), Some(vec![0u8; 16]));
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
            &[
                ("bin/app", stable.clone()),
                ("share/data", vec![1u8; 50_000]),
            ],
            Layout::Stored,
        );
        let remote = build_stipple_package(
            &prefix,
            &[("bin/app", stable), ("share/data", vec![9u8; 50_000])],
            Layout::Stored,
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
    fn fixed_block_compressed_stipple_images_are_refused() {
        let stage = Stage::new("stipple-zstd");
        let prefix = fake_prefix(7);
        let local = build_stipple_package(
            &prefix,
            &[(
                "bin/app",
                (0..300_000u32).map(|i| (i * 3 % 251) as u8).collect(),
            )],
            Layout::Fixed,
        );
        let remote = build_stipple_package(
            &prefix,
            &[(
                "bin/app",
                (0..300_000u32).map(|i| (i * 5 % 251) as u8).collect(),
            )],
            Layout::Fixed,
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
        // Larger than the fetch gap ceiling, so its slices are not merged
        // into the metadata fetches around them.
        let len = 300_000u32;
        let refs: Vec<(&str, Vec<u8>)> =
            vec![("bin/app", (0..len).map(|i| (i * 11 % 251) as u8).collect())];
        let local = build_stipple_package(&fake_prefix(8), &refs, Layout::Stored);
        let remote = build_stipple_package(&fake_prefix(9), &refs, Layout::Stored);

        let (out, stats, remote_pkg) = stage.run(&local, &remote).expect("assemble");
        assert_eq!(out, remote);
        let slices = u64::from(len).div_ceil(onelf_format::payload::SLICE_SIZE) as usize;
        assert_eq!(
            stats.reused, slices,
            "every slice copies from the running binary"
        );
        // The prefix is fetched, and so is the image's own bookkeeping:
        // superblock and metadata sections sit outside the onelf manifest,
        // so no slice can describe them. They are small and fixed-size.
        assert!(
            stats.fetched > 4096,
            "the prefix alone is {}",
            stats.fetched
        );
        assert!(
            stats.fetched < remote_pkg.footer.payload_size,
            "fetched {} of {} payload bytes: content is still copied",
            stats.fetched,
            remote_pkg.footer.payload_size
        );
    }

    /// Counts the range requests a delta makes.
    struct CountingSource {
        inner: FakeSource,
        requests: std::sync::atomic::AtomicUsize,
    }

    impl RangeSource for CountingSource {
        fn size(&self) -> Result<u64, FetchError> {
            self.inner.size()
        }

        fn fetch_range(&self, start: u64, len: u64) -> Result<Box<dyn Read + '_>, FetchError> {
            self.requests
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.fetch_range(start, len)
        }

        fn fetch_whole(&self) -> Result<Box<dyn Read + '_>, FetchError> {
            self.inner.fetch_whole()
        }

        fn fetch_signature(&self) -> Result<Vec<u8>, FetchError> {
            self.inner.fetch_signature()
        }
    }

    #[test]
    fn stored_stipple_requests_do_not_grow_with_the_block_count() {
        let requests = |stable_len: u32| {
            let stage = Stage::new("stipple-requests");
            let prefix = fake_prefix(10);
            let stable: Vec<u8> = (0..stable_len).map(|i| (i * 7 % 251) as u8).collect();
            let local = build_stipple_package(
                &prefix,
                &[
                    ("bin/app", stable.clone()),
                    ("share/data", vec![1u8; 50_000]),
                ],
                Layout::Stored,
            );
            let remote = build_stipple_package(
                &prefix,
                &[("bin/app", stable), ("share/data", vec![9u8; 50_000])],
                Layout::Stored,
            );
            let local_path = stage.dir.join("local");
            std::fs::write(&local_path, local).unwrap();
            let src = CountingSource {
                inner: FakeSource {
                    data: Arc::new(remote.clone()),
                },
                requests: Default::default(),
            };
            let remote_pkg = probe(&src).unwrap();
            let staged_path = stage.dir.join("staged");
            let staged = std::fs::File::create_new(&staged_path).unwrap();
            let stats = assemble(&local_path, &remote_pkg, &staged, &src).unwrap();
            assert_eq!(std::fs::read(&staged_path).unwrap(), remote);
            (
                src.requests.load(std::sync::atomic::Ordering::Relaxed),
                stats.reused,
            )
        };
        // Sixteen times the data adds 240 blocks. Fetching their digests
        // would cost a request each, or, merged into one span, would
        // download every block they sit between. Only the larger metadata
        // may cost a few more requests, and every unchanged slice copies.
        let (small, _) = requests(1 << 20);
        let (large, reused) = requests(16 << 20);
        assert!(large < small + 8, "{small} requests grew to {large}");
        assert!(reused >= 256, "only {reused} slices were copied");
    }

    /// Bytes from a sixteen-letter alphabet: they compress, so the image
    /// keeps compressed blocks, but never repeat, so every chunk is its own.
    fn noise(seed: u64, len: usize) -> Vec<u8> {
        let mut x = seed | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                b"etaoinshrdlucmfw"[(x >> 24) as usize & 15]
            })
            .collect()
    }

    type Tree = Vec<(&'static str, Vec<u8>)>;

    /// A tree whose first file in the image grows in the release, shifting
    /// everything placed after it.
    fn shifted_trees(stable_len: usize) -> (Tree, Tree) {
        let first = noise(1, 200_000);
        let mut grown = first.clone();
        grown.splice(100_000..100_000, noise(2, 5_000));
        let rest = [
            ("lib/b.bin", noise(3, stable_len)),
            ("lib/c.bin", noise(4, 300_000)),
        ];
        let mut local = vec![("lib/a.bin", first)];
        local.extend(rest.iter().cloned());
        let mut remote = vec![("lib/a.bin", grown)];
        remote.extend(rest);
        (local, remote)
    }

    #[test]
    fn compressed_stipple_images_delta_across_builds() {
        let stage = Stage::new("stipple-variable");
        let prefix = fake_prefix(13);
        let (local, remote) = shifted_trees(1_000_000);
        let local = build_stipple_package(&prefix, &local, Layout::Variable);
        let remote = build_stipple_package(&prefix, &remote, Layout::Variable);

        let (out, stats, remote_pkg) = stage.run(&local, &remote).expect("assemble");
        assert_eq!(out, remote);
        assert_eq!(stats.unit, "blocks");
        assert!(stats.reused > 0, "no block was copied");
        assert!(
            stats.fetched < remote_pkg.footer.payload_size / 4,
            "fetched {} of {} payload bytes after a 5 KB insertion",
            stats.fetched,
            remote_pkg.footer.payload_size
        );
    }

    #[test]
    fn a_fixed_running_package_cannot_seed_a_variable_release() {
        let stage = Stage::new("stipple-fixed-to-variable");
        let prefix = fake_prefix(14);
        let (local, remote) = shifted_trees(100_000);
        let local = build_stipple_package(&prefix, &local, Layout::Fixed);
        let remote = build_stipple_package(&prefix, &remote, Layout::Variable);
        let err = stage.run_err(&local, &remote);
        assert!(err.contains("fixed-size"), "{err}");
    }

    #[test]
    fn a_corrupt_copied_stipple_block_is_fetched_instead() {
        let stage = Stage::new("stipple-corrupt");
        let prefix = fake_prefix(15);
        let (local, remote) = shifted_trees(1_000_000);
        let mut local = build_stipple_package(&prefix, &local, Layout::Variable);
        let remote = build_stipple_package(&prefix, &remote, Layout::Variable);
        let (_, clean, _) = stage.run(&local, &remote).expect("assemble");
        assert_eq!(clean.unit, "blocks");

        // Flip a byte in the running package's last block, which the
        // release shares: the copy must fail its digest and be fetched.
        let footer = probe(&FakeSource {
            data: Arc::new(local.clone()),
        })
        .unwrap()
        .footer;
        let payload =
            footer.payload_offset as usize..(footer.payload_offset + footer.payload_size) as usize;
        let image = Image::new(local[payload].to_vec(), 0, footer.payload_size).unwrap();
        let index = image.block_index().unwrap();
        let last = index.get(index.count() - 1).unwrap();
        local[(footer.payload_offset + last.image_offset) as usize] ^= 0xff;

        let stage = Stage::new("stipple-corrupt-2");
        let (out, corrupt, _) = stage.run(&local, &remote).expect("assemble");
        assert_eq!(out, remote);
        assert_eq!(corrupt.reused, clean.reused - 1);
        assert!(corrupt.fetched > clean.fetched);
    }

    #[test]
    fn compressed_stipple_requests_do_not_grow_with_the_block_count() {
        let requests = |stable_len: usize| {
            let stage = Stage::new("stipple-variable-requests");
            let prefix = fake_prefix(16);
            let (local, remote) = shifted_trees(stable_len);
            let local = build_stipple_package(&prefix, &local, Layout::Variable);
            let remote = build_stipple_package(&prefix, &remote, Layout::Variable);
            let local_path = stage.dir.join("local");
            std::fs::write(&local_path, local).unwrap();
            let src = CountingSource {
                inner: FakeSource {
                    data: Arc::new(remote.clone()),
                },
                requests: Default::default(),
            };
            let remote_pkg = probe(&src).unwrap();
            let staged_path = stage.dir.join("staged");
            let staged = std::fs::File::create_new(&staged_path).unwrap();
            let stats = assemble(&local_path, &remote_pkg, &staged, &src).unwrap();
            assert_eq!(stats.unit, "blocks");
            assert_eq!(std::fs::read(&staged_path).unwrap(), remote);
            (
                src.requests.load(std::sync::atomic::Ordering::Relaxed),
                stats.reused,
            )
        };
        // Sixteen times the stable data adds hundreds of blocks, all
        // matched from the block index the metadata already fetched.
        let (small, _) = requests(1 << 20);
        let (large, reused) = requests(16 << 20);
        assert!(large < small + 8, "{small} requests grew to {large}");
        assert!(reused >= 256, "only {reused} blocks were copied");
    }

    #[test]
    fn an_identical_stipple_release_plans_in_three_requests() {
        for layout in [Layout::Stored, Layout::Variable] {
            let stage = Stage::new("stipple-metadata-requests");
            let (tree, _) = shifted_trees(300_000);
            let package = build_stipple_package(&fake_prefix(17), &tree, layout);
            let local_path = stage.dir.join("local");
            std::fs::write(&local_path, &package).unwrap();
            let src = CountingSource {
                inner: FakeSource {
                    data: Arc::new(package.clone()),
                },
                requests: Default::default(),
            };
            let remote_pkg = probe(&src).unwrap();
            let after_probe = src.requests.load(std::sync::atomic::Ordering::Relaxed);
            let staged_path = stage.dir.join("staged");
            let staged = std::fs::File::create_new(&staged_path).unwrap();
            assemble(&local_path, &remote_pkg, &staged, &src).unwrap();
            assert_eq!(std::fs::read(&staged_path).unwrap(), package);
            // Every block and slice of an identical release copies. What
            // is left is the superblock, every metadata section in one
            // range that is then written without a refetch, and the padding
            // that ends the image.
            let planned = src.requests.load(std::sync::atomic::Ordering::Relaxed) - after_probe;
            assert_eq!(planned, 3);
        }
    }

    #[test]
    fn metadata_beyond_the_ceiling_is_refused() {
        let section = |offset, size| Section { offset, size };
        let sections = [section(4096, 4096), section(8192, 100), Section::default()];
        assert_eq!(metadata_range(&sections, 1 << 20).unwrap(), Some(0..12288));
        assert_eq!(
            metadata_range(&[Section::default()], 1 << 20).unwrap(),
            None
        );
        let far = [section(4096, 4096), section(METADATA_MAX, 8)];
        let err = metadata_range(&far, METADATA_MAX * 2).unwrap_err();
        assert!(err.contains("more than"), "{err}");
    }

    /// Serve `data` over loopback HTTP for `connections` requests, each on
    /// its own connection, honoring Range only when `ranges` is set.
    /// Returns the URL and a handle yielding each request line.
    fn loopback_release(
        data: Vec<u8>,
        ranges: bool,
        connections: usize,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/app.onelf", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..connections {
                let (mut stream, _) = listener.accept().unwrap();
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    head.push(byte[0]);
                }
                let head = String::from_utf8(head).unwrap();
                let range = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("range: bytes=")
                            .map(str::to_owned)
                    })
                    .filter(|_| ranges)
                    .map(|r| {
                        let (a, b) = r.split_once('-').unwrap();
                        (a.parse::<usize>().unwrap(), b.parse::<usize>().unwrap())
                    });
                let (status, body, extra) = match range {
                    Some((a, b)) => (
                        "206 Partial Content",
                        &data[a..=b],
                        format!("Content-Range: bytes {a}-{b}/{}\r\n", data.len()),
                    ),
                    None => ("200 OK", &data[..], String::new()),
                };
                let is_head = head.starts_with("HEAD ");
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n",
                    body.len()
                );
                if !is_head {
                    // A client that saw what it needed may hang up early.
                    let _ = stream.write_all(body);
                }
                requests.push(head.lines().next().unwrap().to_owned());
            }
            requests
        });
        (url, server)
    }

    #[test]
    fn the_http_source_probes_a_real_release_in_three_requests() {
        let release = build_blocks_package(&fake_prefix(22), &[("app", vec![b'p'; 100_000])], None);
        let (url, server) = loopback_release(release.clone(), true, 3);
        let src = crate::update::range::HttpsRangeSource::over_plain_http(url);

        let remote = probe(&src).expect("probe over HTTP");
        assert_eq!(remote.size, release.len() as u64);
        let local = probe(&FakeSource {
            data: Arc::new(release),
        })
        .unwrap();
        assert_eq!(remote.package_id(), local.package_id());
        assert_eq!(remote.manifest_bytes, local.manifest_bytes);

        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("HEAD "), "{requests:?}");
        assert!(
            requests[1..].iter().all(|r| r.starts_with("GET ")),
            "{requests:?}"
        );
    }

    #[test]
    fn the_http_source_reports_a_host_that_ignores_ranges() {
        let release = build_blocks_package(&fake_prefix(23), &[("app", vec![b'q'; 1000])], None);
        // HEAD for the size, then the footer read answered in full.
        let (url, server) = loopback_release(release, false, 2);
        let src = crate::update::range::HttpsRangeSource::over_plain_http(url);

        assert!(matches!(probe(&src), Err(FetchError::NoRanges(_))));
        // Later reads fail the same way without another request.
        assert!(matches!(
            src.fetch_range(0, 1),
            Err(FetchError::NoRanges(_))
        ));
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[test]
    fn changing_the_payload_kind_is_refused() {
        let stage = Stage::new("kind");
        let prefix = fake_prefix(10);
        let local = build_blocks_package(&prefix, &[("bin/app", vec![b'x'; 4096])], None);
        let remote =
            build_stipple_package(&prefix, &[("bin/app", vec![b'y'; 4096])], Layout::Stored);

        let err = stage.run_err(&local, &remote);
        assert!(err.contains("payload kind"), "got: {err}");
    }

    #[test]
    fn nearby_fetches_merge_and_drop_the_copies_they_cover() {
        let group = |dst: u64, len: u64| CopyGroup {
            ranges: vec![CopyRange { dst, src: 0, len }],
            check: Check::Raw([0; 32]),
        };
        let span = |start: u64, len: u64| Span { start, len };
        let far = 10 * FETCH_GAP_MAX;
        let mut plan = Plan {
            prefix: Prefix::Fetch { len: 0 },
            dict: None,
            copies: vec![group(1000, 500), group(far - 100, 200), group(far * 3, 100)],
            // Out of order, as a deduplicating packer can lay them out.
            fetch: vec![span(far, 100), span(0, 1000), span(1500, 100)],
            digests: Vec::new(),
            unit: "blocks",
            known: None,
        };
        coalesce(&mut plan);

        let fetch: Vec<(u64, u64)> = plan.fetch.iter().map(|s| (s.start, s.len)).collect();
        assert_eq!(fetch, vec![(0, 1600), (far, 100)]);
        // The copy inside the merged span is downloaded with it; one that
        // only straddles a span, and one far from any, still copy.
        let copies: Vec<u64> = plan.copies.iter().map(|g| g.ranges[0].dst).collect();
        assert_eq!(copies, vec![far - 100, far * 3]);
    }

    #[test]
    fn spans_past_the_gap_ceiling_stay_separate() {
        let mut plan = Plan {
            prefix: Prefix::Fetch { len: 0 },
            dict: None,
            copies: Vec::new(),
            fetch: vec![
                Span { start: 0, len: 10 },
                Span {
                    start: 10 + FETCH_GAP_MAX + 1,
                    len: 10,
                },
            ],
            digests: Vec::new(),
            unit: "blocks",
            known: None,
        };
        coalesce(&mut plan);
        assert_eq!(plan.fetch.len(), 2);
    }

    /// A running package and a release that changes one file and shares
    /// another, so a delta reuses blocks.
    fn running_and_release(stage: &Stage) -> (std::path::PathBuf, Vec<u8>) {
        let prefix = fake_prefix(20);
        let stable = (0..200_000u32)
            .map(|i| (i * 7 % 251) as u8)
            .collect::<Vec<_>>();
        let local = build_blocks_package(
            &prefix,
            &[("app", vec![b'a'; 32_000]), ("stable", stable.clone())],
            None,
        );
        let release = build_blocks_package(
            &prefix,
            &[("app", vec![b'b'; 32_000]), ("stable", stable)],
            None,
        );
        let local_path = stage.dir.join("local");
        std::fs::write(&local_path, local).unwrap();
        (local_path, release)
    }

    /// `release` with one byte flipped inside the changed file's block,
    /// which a delta fetches rather than copies.
    fn corrupt_fetched_block(release: &[u8]) -> Vec<u8> {
        let remote = probe(&FakeSource {
            data: Arc::new(release.to_vec()),
        })
        .unwrap();
        let (offset, len) =
            block_extent(&remote.footer, &remote.manifest.entries[0].blocks[0]).unwrap();
        let mut bad = release.to_vec();
        bad[offset as usize + len / 2] ^= 0xff;
        bad
    }

    fn signer() -> ed25519_compact::KeyPair {
        ed25519_compact::KeyPair::from_seed(ed25519_compact::Seed::new([5u8; 32]))
    }

    /// Everything in the stage beside the running binary: a failed update
    /// must leave nothing behind.
    fn leftovers(stage: &Stage) -> Vec<String> {
        std::fs::read_dir(&stage.dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name != "local")
            .collect()
    }

    #[test]
    fn a_signed_delta_that_fails_its_signature_retries_in_full() {
        let stage = Stage::new("signed-retry");
        let (local_path, release) = running_and_release(&stage);
        let kp = signer();
        let sig = kp.sk.sign(&release, None).to_vec();
        let src = FakeRelease::new(corrupt_fetched_block(&release), release.clone(), Some(sig));
        let remote = probe(&src).unwrap();

        let staged =
            super::super::fetch_verified(&local_path, Some(kp.pk.as_ref()), Some(remote), &src)
                .expect("the full download verifies");
        assert_eq!(src.whole_fetches(), 1);
        assert_eq!(std::fs::read(&staged.path).unwrap(), release);
    }

    #[test]
    fn a_signed_delta_without_reuse_is_refused_without_a_full_download() {
        let stage = Stage::new("signed-no-reuse");
        let (_, release) = running_and_release(&stage);
        // A running package sharing nothing with the release.
        let local_path = stage.dir.join("local");
        std::fs::write(
            &local_path,
            build_blocks_package(&fake_prefix(21), &[("other", vec![b'z'; 1000])], None),
        )
        .unwrap();
        let kp = signer();
        let sig = kp.sk.sign(&release, None).to_vec();
        let src = FakeRelease::new(corrupt_fetched_block(&release), release, Some(sig));
        let remote = probe(&src).unwrap();

        let result =
            super::super::fetch_verified(&local_path, Some(kp.pk.as_ref()), Some(remote), &src);
        assert_eq!(result.err(), Some(2));
        assert_eq!(
            src.whole_fetches(),
            0,
            "nothing was copied, so nothing to retry"
        );
        assert!(leftovers(&stage).is_empty(), "left {:?}", leftovers(&stage));
    }

    #[test]
    fn a_missing_signature_fails_before_any_download() {
        let stage = Stage::new("signed-missing");
        let (local_path, release) = running_and_release(&stage);
        let src = FakeRelease::new(release.clone(), release, None);
        let remote = probe(&src).unwrap();

        let result = super::super::fetch_verified(
            &local_path,
            Some(signer().pk.as_ref()),
            Some(remote),
            &src,
        );
        assert_eq!(result.err(), Some(2));
        assert_eq!(src.whole_fetches(), 0);
        assert!(leftovers(&stage).is_empty(), "left {:?}", leftovers(&stage));
    }

    #[test]
    fn an_unsigned_delta_that_fails_integrity_retries_in_full() {
        let stage = Stage::new("unsigned-retry");
        let (local_path, release) = running_and_release(&stage);
        let src = FakeRelease::new(corrupt_fetched_block(&release), release.clone(), None);
        let remote = probe(&src).unwrap();

        let staged = super::super::fetch_verified(&local_path, None, Some(remote), &src)
            .expect("the full download passes the integrity check");
        assert_eq!(src.whole_fetches(), 1);
        assert_eq!(std::fs::read(&staged.path).unwrap(), release);
    }

    #[test]
    fn an_unsigned_full_download_that_stays_corrupt_is_refused() {
        let stage = Stage::new("unsigned-corrupt");
        let (local_path, release) = running_and_release(&stage);
        let bad = corrupt_fetched_block(&release);
        let src = FakeRelease::new(bad.clone(), bad, None);
        let remote = probe(&src).unwrap();

        let result = super::super::fetch_verified(&local_path, None, Some(remote), &src);
        assert_eq!(result.err(), Some(2));
        assert_eq!(src.whole_fetches(), 1);
        assert!(leftovers(&stage).is_empty(), "left {:?}", leftovers(&stage));
    }

    #[test]
    fn a_full_download_longer_than_the_probed_size_is_refused() {
        let stage = Stage::new("oversized");
        let (local_path, release) = running_and_release(&stage);
        let mut endless = release.clone();
        endless.extend(std::iter::repeat_n(0u8, 1 << 20));
        let src = FakeRelease::new(release, endless, None);

        let result = super::super::fetch_verified(&local_path, None, None, &src);
        assert_eq!(result.err(), Some(2));
        assert!(leftovers(&stage).is_empty(), "left {:?}", leftovers(&stage));
    }
}
