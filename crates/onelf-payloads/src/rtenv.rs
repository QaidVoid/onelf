//! The launch's evaluated per-view environment input, read back bounded.
//!
//! The runtime evaluates the framework path mappings and the registered
//! hooks against the application and its selected sets, applies them, and
//! encodes what the view requires into `<root>/.onelf/runtime-env`: one
//! entry per variable with the value the view provides, the application
//! policy the mapping declared, and whether the launch left that variable
//! set. It also records the active roots
//! and the host-restoration record, which the exec hooks of [`crate::exec`]
//! read and which a sandboxed `clearenv` takes away with everything else.
//!
//! Layout, all numbers decimal byte lengths:
//!
//! ```text
//! ONELF-RUNTIME-ENV 1 <entry count>\n
//! <name>=<policy><length>:<value><applied>;\n    entry count times
//! R<length>:<active roots>;
//! H<length>:<host restoration record>;
//! ```
//!
//! `policy` is `.` for preserve and `!` for replace. `applied` is `+` when
//! the launch left the variable set and `-` when it left it unset. Values
//! are raw bytes of the declared length, so a path list containing a
//! newline survives.
//!
//! Reading is bounded: the file is refused beyond [`MAX_RECORD_BYTES`],
//! every declared length is checked against what remains and against
//! [`MAX_VALUE_BYTES`], the entry count against [`MAX_ENTRIES`], names
//! against [`MAX_NAME_BYTES`], and anything else rejects the whole record
//! with a diagnostic on stderr and nothing applied from it. Nothing is
//! ever truncated or trusted.
//!
//! An entry applies when its variable is missing, which is what a
//! `clearenv` re-exec needs, unless it is a preserve mapping the launch
//! left unset. A present variable is never touched: one still holding what
//! the launch left is already correct, and one holding anything else is
//! an override the application made after the launch and is kept under
//! either policy. Rewriting a present value to the mapping's would either
//! change nothing or undo a preserved caller value, a hook's adjustment or
//! a recipe value, so no record of the applied value is needed to decide.
//! The packaged `.onelf/env` is applied after this record,
//! so a recipe value the package declares itself still wins. The active
//! roots and the host-restoration record return only when missing, since a
//! live record is the fresher one.

use crate::{getenv, setenv};

/// The complete record is capped at one mebibyte, the bound the workspace
/// puts on generated metadata transports.
pub(crate) const MAX_RECORD_BYTES: usize = 1024 * 1024;
/// The most entries one record may carry.
pub(crate) const MAX_ENTRIES: usize = 4096;
/// The longest variable name an entry may carry.
pub(crate) const MAX_NAME_BYTES: usize = 128;
/// The longest value, entry or scalar record, one may carry.
pub(crate) const MAX_VALUE_BYTES: usize = 32 * 1024;

const HEADER: &[u8] = b"ONELF-RUNTIME-ENV 1 ";

/// Why a record was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reject {
    /// The leading header is missing or garbled.
    Header,
    /// The declared entry count is missing, not a number, or over the cap.
    EntryCount,
    /// The record ends in the middle of an entry or scalar.
    Truncated,
    /// A declared length runs past the end of the record.
    Length,
    /// A value or scalar exceeds the per-value cap.
    ValueCap,
    /// A variable name is empty, over the name cap, or holds a NUL.
    Name,
    /// A value holds a NUL, which `setenv` cannot carry.
    Value,
    /// The policy character is neither preserve nor replace.
    Policy,
    /// The applied marker is neither `+` nor `-`.
    Applied,
    /// Bytes follow the last scalar.
    Trailing,
}

impl Reject {
    /// The fragment a stderr diagnostic names the reason by.
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Reject::Header => "bad header",
            Reject::EntryCount => "entry count",
            Reject::Truncated => "truncated",
            Reject::Length => "length past the record",
            Reject::ValueCap => "value beyond the cap",
            Reject::Name => "variable name",
            Reject::Value => "NUL in a value",
            Reject::Policy => "policy",
            Reject::Applied => "applied marker",
            Reject::Trailing => "trailing bytes",
        }
    }
}

/// What a mapping does to a value the launch found already set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Policy {
    /// The launch kept a value that was present.
    Preserve,
    /// The launch put the mapping's value in place of a present one.
    Replace,
}

/// One entry of a decoded record.
pub(crate) struct Entry<'a> {
    /// The variable the entry restores.
    pub(crate) name: &'a [u8],
    /// The value the view provides.
    pub(crate) value: &'a [u8],
    /// How the launch applied the mapping behind the entry.
    pub(crate) policy: Policy,
    /// Whether the launch left the variable set.
    pub(crate) left_set: bool,
}

/// A record that validated completely.
pub(crate) struct Decoded<'a> {
    buf: &'a [u8],
    entries_start: usize,
    entry_count: usize,
    roots: Option<&'a [u8]>,
    host_record: Option<&'a [u8]>,
}

impl<'a> Decoded<'a> {
    /// Validate a whole record. Anything malformed rejects it.
    pub(crate) fn parse(buf: &'a [u8]) -> Result<Decoded<'a>, Reject> {
        if buf.len() < HEADER.len() || &buf[..HEADER.len()] != HEADER {
            return Err(Reject::Header);
        }
        let mut cur = Cursor {
            buf,
            pos: HEADER.len(),
        };
        let entry_count = cur.number(MAX_ENTRIES, Reject::EntryCount, Reject::EntryCount)?;
        cur.take(b'\n')?;
        let entries_start = cur.pos;
        for _ in 0..entry_count {
            cur.entry()?;
        }
        let roots = cur.scalar(b'R')?;
        let host_record = cur.scalar(b'H')?;
        if cur.pos != buf.len() {
            return Err(Reject::Trailing);
        }
        Ok(Decoded {
            buf,
            entries_start,
            entry_count,
            roots,
            host_record,
        })
    }

    /// The entries, in record order. A record that parsed completely
    /// yields only well-formed entries.
    pub(crate) fn entries(&self) -> impl Iterator<Item = Entry<'a>> {
        let mut cur = Cursor {
            buf: self.buf,
            pos: self.entries_start,
        };
        (0..self.entry_count).filter_map(move |_| cur.entry().ok())
    }

    /// The active roots the launch recorded, when present.
    pub(crate) fn roots(&self) -> Option<&'a [u8]> {
        self.roots
    }

    /// The host-restoration record the launch made, when present.
    pub(crate) fn host_record(&self) -> Option<&'a [u8]> {
        self.host_record
    }
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn peek(&self) -> Option<u8> {
        self.buf.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let byte = self.peek();
        if byte.is_some() {
            self.pos += 1;
        }
        byte
    }

    fn take(&mut self, want: u8) -> Result<(), Reject> {
        match self.bump() {
            Some(byte) if byte == want => Ok(()),
            _ => Err(Reject::Truncated),
        }
    }

    /// A decimal number of at most `cap`: `missing` when no digit follows,
    /// `over` when the digits do not fit the cap or the type.
    fn number(&mut self, cap: usize, missing: Reject, over: Reject) -> Result<usize, Reject> {
        let mut n = 0usize;
        let mut digits = 0usize;
        while let Some(byte) = self.peek() {
            let Some(digit) = (byte as char).to_digit(10) else {
                break;
            };
            n = n
                .checked_mul(10)
                .and_then(|n| n.checked_add(digit as usize))
                .ok_or(over)?;
            if n > cap {
                return Err(over);
            }
            self.pos += 1;
            digits += 1;
        }
        if digits == 0 {
            return Err(missing);
        }
        Ok(n)
    }

    /// The bytes up to the next `=`, which stays unconsumed. A NUL in a
    /// name, an empty one, or one past the cap is rejected.
    fn name(&mut self) -> Result<&'a [u8], Reject> {
        let start = self.pos;
        loop {
            match self.peek() {
                Some(b'=') => break,
                Some(0) => return Err(Reject::Name),
                Some(_) => self.pos += 1,
                None => return Err(Reject::Truncated),
            }
        }
        let bytes = &self.buf[start..self.pos];
        if bytes.is_empty() || bytes.len() > MAX_NAME_BYTES {
            return Err(Reject::Name);
        }
        Ok(bytes)
    }

    /// A declared-length value: number, `:`, that many raw bytes. The
    /// length must fit the cap and the record, and the bytes must be free
    /// of NUL.
    fn value(&mut self) -> Result<&'a [u8], Reject> {
        let len = self.number(MAX_VALUE_BYTES, Reject::Truncated, Reject::ValueCap)?;
        self.take(b':')?;
        let rest = self.buf.len() - self.pos;
        if len > rest {
            return Err(Reject::Length);
        }
        let bytes = &self.buf[self.pos..self.pos + len];
        if bytes.contains(&0) {
            return Err(Reject::Value);
        }
        self.pos += len;
        Ok(bytes)
    }

    /// The applied marker: `+` for a variable the launch left set, `-`
    /// for one it left unset.
    fn applied(&mut self) -> Result<bool, Reject> {
        match self.bump() {
            Some(b'+') => Ok(true),
            Some(b'-') => Ok(false),
            _ => Err(Reject::Applied),
        }
    }

    /// One entry: name, policy, value, applied marker, terminator.
    fn entry(&mut self) -> Result<Entry<'a>, Reject> {
        let name = self.name()?;
        self.pos += 1;
        let policy = match self.bump() {
            Some(b'.') => Policy::Preserve,
            Some(b'!') => Policy::Replace,
            _ => return Err(Reject::Policy),
        };
        let value = self.value()?;
        let left_set = self.applied()?;
        self.take(b';')?;
        if self.peek() == Some(b'\n') {
            self.pos += 1;
        }
        Ok(Entry {
            name,
            value,
            policy,
            left_set,
        })
    }

    /// The `R` or `H` scalar record, or `None` when the next byte does not
    /// name that one. The record itself is a declared-length value.
    fn scalar(&mut self, tag: u8) -> Result<Option<&'a [u8]>, Reject> {
        if self.peek() != Some(tag) {
            return Ok(None);
        }
        self.pos += 1;
        let bytes = self.value()?;
        self.take(b';')?;
        if self.peek() == Some(b'\n') {
            self.pos += 1;
        }
        Ok(Some(bytes))
    }
}

// NUL-terminated copies for `setenv` and `getenv`. The constructor runs
// once, on the main thread, before any other code of the process.
static mut NAME_BUF: [u8; MAX_NAME_BYTES + 1] = [0; MAX_NAME_BYTES + 1];
static mut VALUE_BUF: [u8; MAX_VALUE_BYTES + 1] = [0; MAX_VALUE_BYTES + 1];

/// Validate a record, then apply it: entries to variables that are missing,
/// the active roots and the host-restoration record back into the variables
/// they belong to when those went missing with a cleared environment.
pub(crate) unsafe fn apply(buf: &[u8]) -> Result<(), Reject> {
    let decoded = Decoded::parse(buf)?;
    for entry in decoded.entries() {
        let name = terminated(core::ptr::addr_of_mut!(NAME_BUF), entry.name);
        if !getenv(name.as_ptr()).is_null() {
            continue;
        }
        // A preserve mapping the launch left unset invents nothing:
        // restoration is for what the view requires, and this one made no
        // claim on the variable.
        if entry.policy == Policy::Preserve && !entry.left_set {
            continue;
        }
        let value = terminated(core::ptr::addr_of_mut!(VALUE_BUF), entry.value);
        setenv(name.as_ptr(), value.as_ptr(), 1);
    }
    restore(c"ONELF_ROOTS", decoded.roots());
    restore(c"ONELF_HOST_ENV", decoded.host_record());
    Ok(())
}

/// Copy `bytes` into `buf` NUL-terminated, returning the whole copy
/// including the terminator. The decoder has already bounded every length
/// that can reach here.
unsafe fn terminated<'a>(buf: *mut [u8], bytes: &[u8]) -> &'a [u8] {
    let n = bytes.len();
    let dst = &mut *buf;
    let dst = &mut dst[..n + 1];
    dst[..n].copy_from_slice(bytes);
    dst[n] = 0;
    &*core::ptr::slice_from_raw_parts(buf.cast::<u8>(), n + 1)
}

/// Put a scalar record back into its variable when that went missing with
/// the environment it was cleared from. A live value stays.
unsafe fn restore(name: &core::ffi::CStr, value: Option<&[u8]>) {
    let Some(value) = value else {
        return;
    };
    let named = name.as_ptr().cast::<u8>();
    if !getenv(named).is_null() || value.len() > MAX_VALUE_BYTES {
        return;
    }
    let buf = core::ptr::addr_of_mut!(VALUE_BUF);
    let terminated = terminated(buf, value);
    setenv(named, terminated.as_ptr(), 1);
}

#[cfg(test)]
mod tests {
    use crate::harness::{env, env_lock, runtime_entry, runtime_record, set_env, unset_env};
    use crate::rtenv::{Decoded, MAX_ENTRIES, MAX_NAME_BYTES, MAX_VALUE_BYTES, Policy, Reject};

    /// An entry whose declared value length is larger than what follows it,
    /// but still within the per-value cap.
    fn overlong_value(name: &str) -> Vec<u8> {
        let entry = runtime_entry(name, Policy::Replace, b"v", true);
        let text = String::from_utf8(entry).unwrap();
        let start = text.find('=').unwrap() + 2;
        let end = text[start..].find(':').unwrap() + start;
        format!("{}30000{}", &text[..start], &text[end..]).into_bytes()
    }

    fn rejects_to(record: &[u8], want: Reject) {
        assert!(matches!(Decoded::parse(record), Err(got) if got == want));
    }

    #[test]
    fn a_record_parses_back() {
        let value = b"/pkg/lib/gstreamer-1.0";
        let record = runtime_record(
            &[
                runtime_entry("GST_PLUGIN_SYSTEM_PATH", Policy::Replace, value, true),
                runtime_entry(
                    "ALSA_CONFIG_PATH",
                    Policy::Preserve,
                    b"/pkg/share/alsa",
                    false,
                ),
            ],
            Some(b"/pkg:/set"),
            Some(b"A=-16:/x;"),
        );
        let decoded = Decoded::parse(&record).unwrap();
        let entries: Vec<_> = decoded.entries().collect();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, b"GST_PLUGIN_SYSTEM_PATH");
        assert_eq!(entries[0].value, value);
        assert_eq!(entries[0].policy, Policy::Replace);
        assert!(entries[0].left_set);
        assert_eq!(entries[1].name, b"ALSA_CONFIG_PATH");
        assert_eq!(entries[1].policy, Policy::Preserve);
        assert!(!entries[1].left_set);
        assert_eq!(decoded.roots(), Some(b"/pkg:/set".as_slice()));
        assert_eq!(decoded.host_record(), Some(b"A=-16:/x;".as_slice()));
    }

    /// The golden record the runtime's encoder must produce byte for byte
    /// parses back to the entries and scalars it was encoded from.
    #[test]
    fn the_runtimes_golden_record_parses_back() {
        let golden: &[u8] = include_bytes!("../../onelf-rt/tests/fixtures/runtime-env.golden");
        let decoded = Decoded::parse(golden).unwrap();
        let entries: Vec<_> = decoded
            .entries()
            .map(|e| (e.name, e.value, e.policy, e.left_set))
            .collect();
        assert_eq!(
            entries,
            [
                (
                    b"MAPPED_KEEP".as_slice(),
                    b"/view/plugins".as_slice(),
                    Policy::Preserve,
                    true
                ),
                (
                    b"MAPPED_LINE".as_slice(),
                    b"/view/a\n/view/b".as_slice(),
                    Policy::Replace,
                    true
                ),
                (
                    b"MAPPED_NONE".as_slice(),
                    b"/view/none".as_slice(),
                    Policy::Preserve,
                    false
                ),
                (
                    b"CHANGED".as_slice(),
                    b"new".as_slice(),
                    Policy::Replace,
                    true
                ),
            ]
        );
        assert_eq!(decoded.roots(), Some(b"/view:/set".as_slice()));
        assert_eq!(
            decoded.host_record(),
            Some(b"CHANGED=0123456789abcdef3:old;".as_slice())
        );
    }

    #[test]
    fn a_truncated_record_is_rejected() {
        let record = runtime_record(
            &[runtime_entry("A", Policy::Replace, b"1", true)],
            None,
            None,
        );
        for cut in 2..record.len() {
            let at = record.len() - cut;
            assert!(
                Decoded::parse(&record[..at]).is_err(),
                "a record cut to {at} bytes must not be accepted"
            );
        }
    }

    #[test]
    fn a_declared_length_past_the_record_is_rejected() {
        rejects_to(
            &runtime_record(&[overlong_value("A")], None, None),
            Reject::Length,
        );
    }

    #[test]
    fn an_oversized_value_is_rejected() {
        let big = vec![b'x'; MAX_VALUE_BYTES + 1];
        let record = runtime_record(
            &[runtime_entry("A", Policy::Replace, &big, true)],
            None,
            None,
        );
        rejects_to(&record, Reject::ValueCap);
    }

    #[test]
    fn an_oversized_scalar_is_rejected() {
        let big = vec![b'x'; MAX_VALUE_BYTES + 1];
        rejects_to(&runtime_record(&[], Some(&big), None), Reject::ValueCap);
    }

    #[test]
    fn an_oversized_entry_count_is_rejected() {
        let mut record = b"ONELF-RUNTIME-ENV 1 ".to_vec();
        record.extend_from_slice((MAX_ENTRIES + 1).to_string().as_bytes());
        record.push(b'\n');
        rejects_to(&record, Reject::EntryCount);
    }

    #[test]
    fn a_garbled_header_is_rejected() {
        rejects_to(b"", Reject::Header);
        rejects_to(b"NOT-A-RECORD 0\n", Reject::Header);
        rejects_to(b"ONELF-RUNTIME-ENV 2 0\n", Reject::Header);
    }

    #[test]
    fn names_of_the_wrong_shape_are_rejected() {
        let empty = runtime_record(
            &[runtime_entry("", Policy::Replace, b"1", true)],
            None,
            None,
        );
        rejects_to(&empty, Reject::Name);

        let long = "A".repeat(MAX_NAME_BYTES + 1);
        let oversized = runtime_record(
            &[runtime_entry(&long, Policy::Replace, b"1", true)],
            None,
            None,
        );
        rejects_to(&oversized, Reject::Name);
    }

    #[test]
    fn a_nul_in_a_value_is_rejected() {
        let mut entry = runtime_entry("A", Policy::Replace, b"1", true);
        let colon = entry.iter().position(|&b| b == b':').unwrap();
        entry[colon + 1] = 0;
        rejects_to(&runtime_record(&[entry], None, None), Reject::Value);
    }

    #[test]
    fn a_bad_policy_or_marker_is_rejected() {
        let mut entry = runtime_entry("A", Policy::Replace, b"1", true);
        let at = entry.iter().position(|&b| b == b'=').unwrap() + 1;
        entry[at] = b'?';
        rejects_to(&runtime_record(&[entry], None, None), Reject::Policy);

        let mut entry = runtime_entry("A", Policy::Replace, b"1", true);
        let at = entry.iter().position(|&b| b == b'+').unwrap();
        entry[at] = b'*';
        rejects_to(&runtime_record(&[entry], None, None), Reject::Applied);
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut record = runtime_record(
            &[runtime_entry("A", Policy::Replace, b"1", true)],
            None,
            None,
        );
        record.extend_from_slice(b"junk");
        rejects_to(&record, Reject::Trailing);
    }

    #[test]
    fn a_cleared_variable_regains_its_generated_value() {
        let _guard = env_lock();
        let value = "/view/lib/plugins";
        unsafe {
            unset_env("ONELF_T_CLEARED");
            let record = runtime_record(
                &[runtime_entry(
                    "ONELF_T_CLEARED",
                    Policy::Replace,
                    value.as_bytes(),
                    true,
                )],
                None,
                None,
            );
            crate::rtenv::apply(&record).unwrap();
        }
        assert_eq!(env("ONELF_T_CLEARED").as_deref(), Some(value));
        unsafe { unset_env("ONELF_T_CLEARED") };
    }

    #[test]
    fn an_override_survives_a_replace_entry() {
        let _guard = env_lock();
        let mapped = "/view/lib/plugins";
        unsafe {
            set_env("ONELF_T_OVERRIDE", "/app/chose/this");
            let record = runtime_record(
                &[runtime_entry(
                    "ONELF_T_OVERRIDE",
                    Policy::Replace,
                    mapped.as_bytes(),
                    true,
                )],
                None,
                None,
            );
            crate::rtenv::apply(&record).unwrap();
        }
        assert_eq!(env("ONELF_T_OVERRIDE").as_deref(), Some("/app/chose/this"));
        unsafe { unset_env("ONELF_T_OVERRIDE") };
    }

    #[test]
    fn a_value_the_launch_left_is_not_reapplied() {
        let _guard = env_lock();
        let callers = "/caller/plugins";
        let mapped = "/view/lib/plugins";
        unsafe {
            set_env("ONELF_T_LEFT", callers);
            let record = runtime_record(
                &[runtime_entry(
                    "ONELF_T_LEFT",
                    Policy::Preserve,
                    mapped.as_bytes(),
                    true,
                )],
                None,
                None,
            );
            crate::rtenv::apply(&record).unwrap();
        }
        assert_eq!(env("ONELF_T_LEFT").as_deref(), Some(callers));
        unsafe { unset_env("ONELF_T_LEFT") };
    }

    #[test]
    fn a_preserve_entry_the_launch_never_applied_invents_nothing() {
        let _guard = env_lock();
        unsafe {
            unset_env("ONELF_T_INVENT");
            let record = runtime_record(
                &[runtime_entry(
                    "ONELF_T_INVENT",
                    Policy::Preserve,
                    b"/view/lib/plugins",
                    false,
                )],
                None,
                None,
            );
            crate::rtenv::apply(&record).unwrap();
        }
        assert_eq!(env("ONELF_T_INVENT"), None);
    }

    #[test]
    fn the_roots_and_host_record_return_when_missing() {
        let _guard = env_lock();
        unsafe {
            unset_env("ONELF_ROOTS");
            unset_env("ONELF_HOST_ENV");
            let record = runtime_record(&[], Some(b"/view"), Some(b"A=-16:/x;"));
            crate::rtenv::apply(&record).unwrap();
        }
        assert_eq!(env("ONELF_ROOTS").as_deref(), Some("/view"));
        assert_eq!(env("ONELF_HOST_ENV").as_deref(), Some("A=-16:/x;"));

        unsafe {
            set_env("ONELF_ROOTS", "/live");
            set_env("ONELF_HOST_ENV", "live");
            let record = runtime_record(&[], Some(b"/stale"), Some(b"stale"));
            crate::rtenv::apply(&record).unwrap();
        }
        assert_eq!(env("ONELF_ROOTS").as_deref(), Some("/live"));
        assert_eq!(env("ONELF_HOST_ENV").as_deref(), Some("live"));
        unsafe {
            unset_env("ONELF_ROOTS");
            unset_env("ONELF_HOST_ENV");
        }
    }

    #[test]
    fn a_rejected_record_applies_nothing() {
        let _guard = env_lock();
        unsafe {
            unset_env("ONELF_T_ALL_OR_NOTHING");
            let mut record = runtime_record(
                &[runtime_entry(
                    "ONELF_T_ALL_OR_NOTHING",
                    Policy::Replace,
                    b"/view/lib",
                    true,
                )],
                None,
                None,
            );
            record.extend_from_slice(b"junk");
            assert!(crate::rtenv::apply(&record).is_err());
        }
        assert_eq!(env("ONELF_T_ALL_OR_NOTHING"), None);
    }
}
