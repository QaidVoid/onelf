use std::io::{self, Read, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EntryKind {
    Dir = 0,
    File = 1,
    Symlink = 2,
}

impl TryFrom<u8> for EntryKind {
    type Error = io::Error;

    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            0 => Ok(EntryKind::Dir),
            1 => Ok(EntryKind::File),
            2 => Ok(EntryKind::Symlink),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid entry kind: {v}"),
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WorkingDir {
    Inherit = 0,
    PackageRoot = 1,
    EntrypointParent = 2,
}

impl TryFrom<u8> for WorkingDir {
    type Error = io::Error;

    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            0 => Ok(WorkingDir::Inherit),
            1 => Ok(WorkingDir::PackageRoot),
            2 => Ok(WorkingDir::EntrypointParent),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid working_dir: {v}"),
            )),
        }
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct EntryPointFlags: u8 {
        const MEMFD_ELIGIBLE = 1 << 0;
    }
}

#[derive(Debug, Clone)]
pub struct Block {
    /// Byte offset into the payload section where this block's data begins.
    pub payload_offset: u64,
    /// Size of the block after compression.
    pub compressed_size: u64,
    /// Size of the block before compression.
    pub original_size: u64,
}

pub const BLOCK_SIZE: usize = 24;

impl Block {
    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        w.write_all(&self.payload_offset.to_le_bytes())?;
        w.write_all(&self.compressed_size.to_le_bytes())?;
        w.write_all(&self.original_size.to_le_bytes())?;
        Ok(())
    }

    pub fn read_from<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut buf = [0u8; BLOCK_SIZE];
        r.read_exact(&mut buf)?;

        Ok(Block {
            payload_offset: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            compressed_size: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
            original_size: u64::from_le_bytes(buf[16..24].try_into().unwrap()),
        })
    }
}

#[derive(Debug, Clone)]
pub struct EntryPoint {
    /// Offset into the string table for this entrypoint's name.
    pub name: u32,
    /// Index of the filesystem entry this entrypoint executes.
    pub target_entry: u32,
    /// Offset into the string table for the argument string.
    pub args: u32,
    /// Working directory strategy when launching this entrypoint.
    pub working_dir: WorkingDir,
    /// Behavioral flags for this entrypoint.
    pub flags: EntryPointFlags,
}

/// Size of serialized EntryPoint: 4+4+4+1+1 = 14 bytes
pub const ENTRYPOINT_SIZE: usize = 14;

impl EntryPoint {
    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        w.write_all(&self.name.to_le_bytes())?;
        w.write_all(&self.target_entry.to_le_bytes())?;
        w.write_all(&self.args.to_le_bytes())?;
        w.write_all(&[self.working_dir as u8])?;
        w.write_all(&[self.flags.bits()])?;
        Ok(())
    }

    pub fn read_from<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut buf = [0u8; ENTRYPOINT_SIZE];
        r.read_exact(&mut buf)?;

        Ok(EntryPoint {
            name: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
            target_entry: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
            args: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
            working_dir: WorkingDir::try_from(buf[12])?,
            flags: EntryPointFlags::from_bits_truncate(buf[13]),
        })
    }

    pub fn is_memfd_eligible(&self) -> bool {
        self.flags.contains(EntryPointFlags::MEMFD_ELIGIBLE)
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    /// Type of this entry (file, directory, or symlink).
    pub kind: EntryKind,
    /// Index of the parent directory entry, or `u32::MAX` for top-level entries.
    pub parent: u32,
    /// Offset into the string table for this entry's name.
    pub name: u32,
    /// Unix file mode (permissions and type bits).
    pub mode: u32,
    /// Modification time: seconds since Unix epoch.
    pub mtime_secs: u64,
    /// Modification time: nanosecond component.
    pub mtime_nsec: u32,
    /// BLAKE3 hash of the file content (files only).
    pub content_hash: [u8; 32],
    /// Number of compressed payload blocks (files only).
    pub num_blocks: u32,
    /// Compressed payload blocks containing this file's data (files only).
    pub blocks: Vec<Block>,
    /// Offset into the string table for the symlink target path (symlinks only).
    pub symlink_target: u32,
}

/// Size of serialized Entry (without blocks):
/// 1 + 4 + 4 + 4 + 8 + 4 + 32 + 4 + 4 = 65 bytes
/// Plus num_blocks * BLOCK_SIZE bytes of block data
pub const ENTRY_HEADER_SIZE: usize = 65;

impl Entry {
    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        w.write_all(&[self.kind as u8])?;
        w.write_all(&self.parent.to_le_bytes())?;
        w.write_all(&self.name.to_le_bytes())?;
        w.write_all(&self.mode.to_le_bytes())?;
        w.write_all(&self.mtime_secs.to_le_bytes())?;
        w.write_all(&self.mtime_nsec.to_le_bytes())?;
        w.write_all(&self.content_hash)?;
        w.write_all(&self.num_blocks.to_le_bytes())?;
        w.write_all(&self.symlink_target.to_le_bytes())?;
        for block in &self.blocks {
            block.write_to(w)?;
        }
        Ok(())
    }

    pub fn read_from<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut buf = [0u8; ENTRY_HEADER_SIZE];
        r.read_exact(&mut buf)?;

        let kind = EntryKind::try_from(buf[0])?;
        let parent = u32::from_le_bytes(buf[1..5].try_into().unwrap());
        let name = u32::from_le_bytes(buf[5..9].try_into().unwrap());
        let mode = u32::from_le_bytes(buf[9..13].try_into().unwrap());
        let mtime_secs = u64::from_le_bytes(buf[13..21].try_into().unwrap());
        let mtime_nsec = u32::from_le_bytes(buf[21..25].try_into().unwrap());
        let mut content_hash = [0u8; 32];
        content_hash.copy_from_slice(&buf[25..57]);
        let num_blocks = u32::from_le_bytes(buf[57..61].try_into().unwrap());
        let symlink_target = u32::from_le_bytes(buf[61..65].try_into().unwrap());

        let mut blocks = Vec::with_capacity(num_blocks as usize);
        for _ in 0..num_blocks {
            blocks.push(Block::read_from(r)?);
        }

        Ok(Entry {
            kind,
            parent,
            name,
            mode,
            mtime_secs,
            mtime_nsec,
            content_hash,
            num_blocks,
            blocks,
            symlink_target,
        })
    }
}
