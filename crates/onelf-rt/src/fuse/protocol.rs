// FUSE kernel protocol structures (ABI version 7.x)
// Reference: linux/include/uapi/linux/fuse.h

// Opcodes
pub const FUSE_LOOKUP: u32 = 1;
pub const FUSE_FORGET: u32 = 2;
pub const FUSE_GETATTR: u32 = 3;
pub const FUSE_READLINK: u32 = 5;
pub const FUSE_OPEN: u32 = 14;
pub const FUSE_READ: u32 = 15;
pub const FUSE_RELEASE: u32 = 18;
pub const FUSE_STATFS: u32 = 17;
pub const FUSE_INIT: u32 = 26;
pub const FUSE_OPENDIR: u32 = 27;
pub const FUSE_READDIR: u32 = 28;
pub const FUSE_RELEASEDIR: u32 = 29;
pub const FUSE_ACCESS: u32 = 34;
pub const FUSE_DESTROY: u32 = 38;
pub const FUSE_BATCH_FORGET: u32 = 42;
pub const FUSE_READDIRPLUS: u32 = 44;

// Init flags
pub const FUSE_ASYNC_READ: u32 = 1 << 0;
pub const FUSE_BIG_WRITES: u32 = 1 << 3;
pub const FUSE_DO_READDIRPLUS: u32 = 1 << 13;
pub const FUSE_READDIRPLUS_AUTO: u32 = 1 << 14;
pub const FUSE_CACHE_SYMLINKS: u32 = 1 << 23;
pub const FUSE_NO_OPENDIR_SUPPORT: u32 = 1 << 24;

// Open flags
pub const FOPEN_KEEP_CACHE: u32 = 1 << 1;

// Attr/entry valid timeout in seconds.
// Must not overflow when the kernel computes `sec * HZ` (HZ up to 1000).
// 1 billion seconds (~31 years) is effectively infinite without overflowing u64.
pub const ATTR_TIMEOUT: u64 = 1_000_000_000;
pub const ENTRY_TIMEOUT: u64 = 1_000_000_000;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseInHeader {
    pub len: u32,
    pub opcode: u32,
    pub unique: u64,
    pub nodeid: u64,
    pub uid: u32,
    pub gid: u32,
    pub pid: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseOutHeader {
    pub len: u32,
    pub error: i32,
    pub unique: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseInitIn {
    pub major: u32,
    pub minor: u32,
    pub max_readahead: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseInitOut {
    pub major: u32,
    pub minor: u32,
    pub max_readahead: u32,
    pub flags: u32,
    pub max_background: u16,
    pub congestion_threshold: u16,
    pub max_write: u32,
    pub time_gran: u32,
    pub max_pages: u16,
    pub map_alignment: u16,
    pub flags2: u32,
    pub unused: [u32; 7],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseAttr {
    pub ino: u64,
    pub size: u64,
    pub blocks: u64,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub atimensec: u32,
    pub mtimensec: u32,
    pub ctimensec: u32,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u32,
    pub blksize: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseGetAttrIn {
    pub getattr_flags: u32,
    pub dummy: u32,
    pub fh: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseAttrOut {
    pub attr_valid: u64,
    pub attr_valid_nsec: u32,
    pub dummy: u32,
    pub attr: FuseAttr,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseEntryOut {
    pub nodeid: u64,
    pub generation: u64,
    pub entry_valid: u64,
    pub attr_valid: u64,
    pub entry_valid_nsec: u32,
    pub attr_valid_nsec: u32,
    pub attr: FuseAttr,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseOpenIn {
    pub flags: u32,
    pub open_flags: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseOpenOut {
    pub fh: u64,
    pub open_flags: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseReadIn {
    pub fh: u64,
    pub offset: u64,
    pub size: u32,
    pub read_flags: u32,
    pub lock_owner: u64,
    pub flags: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseAccessIn {
    pub mask: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseStatfsOut {
    pub st: FuseKStatfs,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseKStatfs {
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
    pub files: u64,
    pub ffree: u64,
    pub bsize: u32,
    pub namelen: u32,
    pub frsize: u32,
    pub padding: u32,
    pub spare: [u32; 6],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseDirent {
    pub ino: u64,
    pub off: u64,
    pub namelen: u32,
    pub typ: u32,
    // name follows (variable length, padded to 8 bytes)
}

// Size helpers
pub const IN_HEADER_SIZE: usize = core::mem::size_of::<FuseInHeader>();
pub const OUT_HEADER_SIZE: usize = core::mem::size_of::<FuseOutHeader>();
pub const DIRENT_BASE_SIZE: usize = core::mem::size_of::<FuseDirent>();
pub const ENTRY_OUT_SIZE: usize = core::mem::size_of::<FuseEntryOut>();

pub fn dirent_size(name_len: usize) -> usize {
    // dirent header + name, padded to 8-byte boundary
    (DIRENT_BASE_SIZE + name_len + 7) & !7
}

pub fn direntplus_size(name_len: usize) -> usize {
    // entry_out + dirent header + name, padded to 8-byte boundary
    (ENTRY_OUT_SIZE + DIRENT_BASE_SIZE + name_len + 7) & !7
}

/// Read a `#[repr(C)]` struct from a byte slice at the given offset.
///
/// # Safety
/// The caller must ensure `T` is a plain-old-data `#[repr(C)]` struct
/// with no padding invariants.
pub unsafe fn read_struct<T: Copy>(buf: &[u8], offset: usize) -> Option<T> {
    let size = core::mem::size_of::<T>();
    if offset + size > buf.len() {
        return None;
    }
    Some(unsafe { core::ptr::read_unaligned(buf[offset..].as_ptr() as *const T) })
}

/// Write a `#[repr(C)]` struct into a byte vector.
///
/// # Safety
/// The caller must ensure `T` is a plain-old-data `#[repr(C)]` struct.
pub unsafe fn write_struct<T: Copy>(buf: &mut Vec<u8>, val: &T) {
    let size = core::mem::size_of::<T>();
    let ptr = val as *const T as *const u8;
    buf.extend_from_slice(unsafe { core::slice::from_raw_parts(ptr, size) });
}
