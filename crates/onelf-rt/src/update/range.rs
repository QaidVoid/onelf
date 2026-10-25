//! The ranged HTTPS transport and the remote package probe.
//!
//! The update URL names the release binary itself. Two small requests, the
//! 76-byte footer and then the compressed manifest, describe every payload
//! block of the remote package; the delta planner compares those hashes
//! against the running package's manifest and names the exact byte spans
//! that must be fetched. A host that answers a range request with a
//! full-body 200 is reported as [`FetchError::NoRanges`] so the caller can
//! fall back to a whole-file download, which still needs no control file.

use std::fmt;
use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, Ordering};

use onelf_format::reader::validate_footer;
use onelf_format::{Footer, Manifest, FOOTER_SIZE};

use super::streaming_agent;

/// Why a fetch failed, and whether the caller can still get the release.
#[derive(Debug)]
pub(super) enum FetchError {
    /// The host answered a range request with an unscoped 200: it does not
    /// honor Range, so only a whole-file download can work.
    NoRanges(String),
    /// Anything else. The update cannot proceed over this transport.
    Other(String),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::NoRanges(m) | FetchError::Other(m) => f.write_str(m),
        }
    }
}

/// The bytes the updater needs from a remote release: its size, and any
/// byte range of it. Implementations must be usable from `&self` because
/// the stipplefs [`stipplefs::source::ByteSource`] reads through a shared
/// reference.
pub(super) trait RangeSource: Send + Sync {
    /// Total size of the remote file in bytes.
    fn size(&self) -> Result<u64, FetchError>;

    /// Exactly `len` bytes starting at `start`. Short reads are an error.
    fn fetch_range(&self, start: u64, len: u64) -> Result<Box<dyn Read + '_>, FetchError>;
}

/// A remote release over the hardened HTTPS client.
///
/// Range requests that come back as a plain 200 mark the host as not
/// honoring Range, and every later request fails with
/// [`FetchError::NoRanges`] without another round trip.
pub(super) struct HttpsRangeSource {
    url: String,
    agent: ureq::Agent,
    ranges: AtomicBool,
}

impl HttpsRangeSource {
    pub(super) fn new(url: String) -> Self {
        HttpsRangeSource {
            url,
            // Ranged fetches stream bodies of every size from a few
            // kilobytes to whole releases, so they want the per-read
            // budget rather than a total for the transfer.
            agent: streaming_agent(10),
            ranges: AtomicBool::new(true),
        }
    }
}

impl RangeSource for HttpsRangeSource {
    fn size(&self) -> Result<u64, FetchError> {
        // HEAD first, and only a success response's Content-Length is
        // the release size: ureq hands back error responses as `Ok`, and
        // a 404 page's length would poison every later offset. Hosts
        // that refuse HEAD usually still answer a one-byte range, whose
        // Content-Range carries the total; a plain 200 there still
        // yields Content-Length, only without range support.
        if let Ok(response) = self.agent.head(&self.url).call()
            && response.status().is_success()
            && let Some(len) = content_length(response.headers())
        {
            return Ok(len);
        }
        let response = self
            .agent
            .get(&self.url)
            .header("Range", "bytes=0-0")
            .call()
            .map_err(|e| FetchError::Other(format!("probe {}: {e}", self.url)))?;
        match response.status().as_u16() {
            // The one-byte probe's 206 is held to the same exactness as
            // every ranged fetch: only a header naming `bytes 0-0` with
            // a well-formed total may have that total read.
            206 => {
                let scoped = response
                    .headers()
                    .get("content-range")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| {
                        v.strip_prefix("bytes 0-0/")
                            .is_some_and(|total| !total.is_empty() && !total.contains('/'))
                    });
                if !scoped {
                    return Err(FetchError::Other(format!(
                        "{}: 206 without a `bytes 0-0/` Content-Range",
                        self.url
                    )));
                }
                content_range_total(response.headers()).ok_or_else(|| {
                    FetchError::Other(format!(
                        "{}: 206 without a parseable Content-Range total",
                        self.url
                    ))
                })
            }
            200 => content_length(response.headers()).ok_or_else(|| {
                FetchError::Other(format!("{}: no Content-Length", self.url))
            }),
            status => Err(FetchError::Other(format!(
                "{}: HTTP {status} probing the release size",
                self.url
            ))),
        }
    }

    fn fetch_range(&self, start: u64, len: u64) -> Result<Box<dyn Read + '_>, FetchError> {
        if len == 0 {
            return Ok(Box::new(io::empty()));
        }
        if !self.ranges.load(Ordering::Relaxed) {
            return Err(FetchError::NoRanges(
                "the release host does not honor range requests".into(),
            ));
        }
        let end = start.checked_add(len - 1).ok_or_else(|| {
            FetchError::Other(format!("range {start}+{len} overflows the file size"))
        })?;
        let response = self
            .agent
            .get(&self.url)
            .header("Range", format!("bytes={start}-{end}"))
            .call()
            .map_err(|e| FetchError::Other(format!("range {start}-{end}: {e}")))?;
        let status = response.status().as_u16();
        let content_range = response
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok());
        // The verdict runs before any body byte is read: a 200 here must
        // poison range use before the body can be misread, and a 206 that
        // does not name exactly the requested span must never be trusted.
        match range_verdict(status, content_range, start, end) {
            Ok(true) => {
                // The body is capped at the requested length: a buggy
                // server sending more must not shift anything downstream.
                Ok(Box::new(response.into_body().into_reader().take(len)))
            }
            Ok(false) => {
                self.ranges.store(false, Ordering::Relaxed);
                Err(FetchError::NoRanges(format!(
                    "{url} answered a range request with a full 200",
                    url = self.url
                )))
            }
            Err(why) => Err(FetchError::Other(format!("{}: {why}", self.url))),
        }
    }
}

/// What a response to a `bytes={start}-{end}` request means, from its
/// status and Content-Range alone. `Ok(true)` is a scoped 206 whose body
/// may be read, `Ok(false)` is a plain 200 meaning the host ignores
/// Range, and an error refuses the response outright.
///
/// A 206 that does not name exactly the requested span is refused rather
/// than trusted: its bytes would land at offsets they do not belong to,
/// and only the manifest checksums or the signature would notice, never
/// the unsigned policy.
fn range_verdict(
    status: u16,
    content_range: Option<&str>,
    start: u64,
    end: u64,
) -> Result<bool, String> {
    match status {
        206 => {
            let Some(header) = content_range else {
                return Err(format!(
                    "a 206 for bytes {start}-{end} carries no Content-Range header"
                ));
            };
            let header = header.trim();
            if !header.starts_with(&format!("bytes {start}-{end}/")) {
                return Err(format!(
                    "a 206 for bytes {start}-{end} claims Content-Range {header:?}"
                ));
            }
            Ok(true)
        }
        200 => Ok(false),
        status => Err(format!("HTTP {status} for bytes {start}-{end}")),
    }
}

fn content_length(headers: &ureq::http::HeaderMap) -> Option<u64> {
    headers
        .get("content-length")?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// The total from a `Content-Range: bytes 0-0/12345` header.
fn content_range_total(headers: &ureq::http::HeaderMap) -> Option<u64> {
    let value = headers.get("content-range")?.to_str().ok()?;
    let total = value.rsplit('/').next()?.trim();
    if total == "*" {
        None
    } else {
        total.parse().ok()
    }
}

/// Ceiling on any single range read the probe is willing to buffer. The
/// server also bounds these through the footer it served, but a hostile
/// host controls both sides of that check.
const MAX_PROBE_READ: u64 = 256 << 20;

/// The remote release, as far as two small range requests can see it.
pub(super) struct RemotePackage {
    pub footer: Footer,
    pub footer_bytes: [u8; FOOTER_SIZE],
    pub manifest: Manifest,
    /// The compressed manifest exactly as fetched, written back into the
    /// assembled file byte for byte.
    pub manifest_bytes: Vec<u8>,
    pub size: u64,
}

impl RemotePackage {
    pub fn package_id(&self) -> &[u8; 32] {
        &self.manifest.header.package_id
    }
}

/// Fetch the remote footer and manifest.
///
/// Every later decision the delta planner makes rests on these bytes, so
/// both are checked the same way the runtime checks a local package: the
/// footer against the file size, the manifest against its checksum.
pub(super) fn probe(src: &dyn RangeSource) -> Result<RemotePackage, FetchError> {
    let size = src.size()?;
    if size < FOOTER_SIZE as u64 {
        return Err(FetchError::Other(format!(
            "the remote release is {size} bytes, smaller than an onelf footer"
        )));
    }
    let read = |start: u64, len: u64| -> Result<Vec<u8>, FetchError> {
        let mut buf = vec![0u8; usize::try_from(len).map_err(|_| {
            FetchError::Other(format!("a {len}-byte range does not fit in memory"))
        })?];
        src.fetch_range(start, len)?
            .read_exact(&mut buf)
            .map_err(|e| FetchError::Other(format!("read {len} bytes at {start}: {e}")))?;
        Ok(buf)
    };

    let footer_bytes: [u8; FOOTER_SIZE] = read(size - FOOTER_SIZE as u64, FOOTER_SIZE as u64)?
        .try_into()
        .expect("the footer read is FOOTER_SIZE bytes");
    let footer = Footer::from_bytes(&footer_bytes)
        .map_err(|e| FetchError::Other(format!("remote footer: {e}")))?;
    validate_footer(&footer, size)
        .map_err(|e| FetchError::Other(format!("remote footer: {e}")))?;

    // The manifest read is sized by the footer the server itself served,
    // so a hostile host can name a huge region and make this process
    // allocate for it. Real manifests are kilobytes to a few megabytes;
    // the loader bounds only the decompressed manifest, so the probe
    // adds this compressed-side ceiling of its own: past it is a
    // refusal, not an allocation.
    if footer.manifest_compressed > MAX_PROBE_READ {
        return Err(FetchError::Other(format!(
            "the remote manifest claims {} compressed bytes",
            footer.manifest_compressed
        )));
    }
    let manifest_bytes = read(footer.manifest_offset, footer.manifest_compressed)?;
    let manifest_raw = zstd::bulk::decompress(&manifest_bytes, footer.manifest_original as usize)
        .map_err(|e| FetchError::Other(format!("remote manifest decompression: {e}")))?;
    let checksum = onelf_format::manifest::manifest_checksum(&manifest_raw)
        .map_err(|e| FetchError::Other(format!("remote manifest: {e}")))?;
    if checksum != footer.manifest_checksum {
        return Err(FetchError::Other("remote manifest checksum mismatch".into()));
    }
    let manifest = Manifest::deserialize(&manifest_raw)
        .map_err(|e| FetchError::Other(format!("remote manifest: {e}")))?;

    Ok(RemotePackage {
        footer,
        footer_bytes,
        manifest,
        manifest_bytes,
        size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An in-memory release: the stand-in for a release server in tests.
    pub(super) struct FakeSource {
        pub data: std::sync::Arc<Vec<u8>>,
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
            Ok(Box::new(io::Cursor::new(
                self.data[start as usize..end as usize].to_vec(),
            )))
        }
    }

    /// A release that claims a size its bytes cannot back: the footer is
    /// real, everything else is refused, so the probe's ceiling must fire
    /// before the manifest read rather than after a failed fetch.
    struct PhantomRelease {
        claimed_size: u64,
        footer: [u8; FOOTER_SIZE],
    }

    impl RangeSource for PhantomRelease {
        fn size(&self) -> Result<u64, FetchError> {
            Ok(self.claimed_size)
        }

        fn fetch_range(&self, start: u64, len: u64) -> Result<Box<dyn Read + '_>, FetchError> {
            if start == self.claimed_size - FOOTER_SIZE as u64 && len == FOOTER_SIZE as u64 {
                return Ok(Box::new(io::Cursor::new(self.footer.to_vec())));
            }
            Err(FetchError::Other(
                "the phantom release serves only its footer".into(),
            ))
        }
    }

    #[test]
    fn content_range_totals_parse_and_reject_unknown_lengths() {
        let mut headers = ureq::http::HeaderMap::new();
        headers.insert("content-range", "bytes 0-0/12345".parse().unwrap());
        assert_eq!(content_range_total(&headers), Some(12345));

        headers.insert("content-range", "bytes 0-0/*".parse().unwrap());
        assert_eq!(content_range_total(&headers), None);

        headers.insert("content-range", "nonsense".parse().unwrap());
        assert_eq!(content_range_total(&headers), None);
    }

    #[test]
    fn range_verdicts_accept_only_exactly_scoped_206s() {
        use super::range_verdict;

        // A 206 naming exactly the requested span is readable.
        assert!(range_verdict(206, Some("bytes 100-199/12345"), 100, 199).unwrap());
        assert!(range_verdict(206, Some("bytes 100-199/*"), 100, 199).unwrap());
        // A different span, a missing header, or multipart nonsense is
        // refused: its bytes would land at offsets they do not belong to.
        assert!(range_verdict(206, Some("bytes 0-99/12345"), 100, 199).is_err());
        assert!(range_verdict(206, Some("bytes 100-198/12345"), 100, 199).is_err());
        assert!(range_verdict(206, None, 100, 199).is_err());
        assert!(range_verdict(206, Some("bytes 100-199, 300-399/12345"), 100, 199).is_err());
        // A plain 200 means no range support, whatever headers say.
        assert!(!range_verdict(200, Some("bytes 100-199/12345"), 100, 199).unwrap());
        assert!(!range_verdict(200, None, 100, 199).unwrap());
        // Anything else is a transport failure.
        assert!(range_verdict(404, None, 100, 199).is_err());
        assert!(range_verdict(500, None, 100, 199).is_err());
    }

    #[test]
    fn probe_rejects_a_source_smaller_than_a_footer() {
        let src = FakeSource {
            data: std::sync::Arc::new(vec![0u8; 10]),
        };
        assert!(matches!(
            probe(&src),
            Err(FetchError::Other(m)) if m.contains("smaller than an onelf footer")
        ));
    }

    #[test]
    fn probe_rejects_a_corrupt_manifest_checksum() {
        let mut data = vec![b'p'; 4096];
        // A footer whose checksum field cannot match any manifest.
        data.extend_from_slice(&[0u8; 40]);
        let mut footer = [0u8; FOOTER_SIZE];
        footer[0..8].copy_from_slice(&onelf_format::MAGIC);
        footer[8..10].copy_from_slice(&1u16.to_le_bytes());
        footer[64..68].copy_from_slice(&[1, 2, 3, 4]);
        footer[68..76].copy_from_slice(&onelf_format::END_MAGIC);
        data.extend_from_slice(&footer);

        let src = FakeSource {
            data: std::sync::Arc::new(data),
        };
        assert!(matches!(
            probe(&src),
            Err(FetchError::Other(m)) if m.contains("footer") || m.contains("manifest")
        ));
    }

    #[test]
    fn probe_refuses_a_manifest_read_past_its_ceiling_before_any_read() {
        use onelf_format::footer::{Flags, FORMAT_VERSION};

        // A release whose size and footer agree on a manifest far past
        // the probe's read ceiling. The source serves only the footer;
        // any other read is refused, so a cap applied too late would
        // surface as the wrong message and fail the assertion.
        let footer = Footer {
            format_version: FORMAT_VERSION,
            flags: Flags::empty(),
            manifest_offset: 1024,
            manifest_compressed: MAX_PROBE_READ + 1,
            manifest_original: 1024,
            payload_offset: 0,
            payload_size: 0,
            dict_offset: 0,
            dict_size: 0,
            manifest_checksum: [1, 2, 3, 4],
        };
        let mut bytes = Vec::new();
        footer.write_to(&mut bytes).unwrap();
        let footer_bytes: [u8; FOOTER_SIZE] = bytes.try_into().unwrap();

        let src = PhantomRelease {
            claimed_size: footer.manifest_offset + footer.manifest_compressed + FOOTER_SIZE as u64,
            footer: footer_bytes,
        };
        assert!(matches!(
            probe(&src),
            Err(FetchError::Other(m)) if m.contains("compressed bytes")
        ));
    }
}
