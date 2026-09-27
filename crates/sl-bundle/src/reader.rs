use crate::error::io;
use crate::{Control, Error, Result};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
    sync::Arc,
};

#[derive(Debug)]
pub(crate) struct DirectoryInfo {
    pub(crate) entries: u64,
    pub(crate) size: u64,
    pub(crate) offset: u64,
}

#[cfg(unix)]
use std::os::unix::fs::FileExt;

#[cfg(not(unix))]
use std::sync::Mutex;

/// Clones share the open file but keep independent offsets. Positional reads
/// permit parallel decompression without mapping the archive or copying it.
#[derive(Debug, Clone)]
pub(crate) struct ArchiveReader<'a> {
    #[cfg(unix)]
    file: Arc<File>,
    #[cfg(not(unix))]
    file: Arc<Mutex<File>>,
    position: u64,
    length: u64,
    flipped: bool,
    control: Control<'a>,
}

impl<'a> ArchiveReader<'a> {
    pub(crate) fn open(path: &Path, control: Control<'a>) -> Result<Self> {
        if !io(path, std::fs::metadata(path))?.is_file() {
            return Err(Error::Archive("archive input must be a regular file".into()));
        }

        let mut file = io(path, File::open(path))?;
        let length = io(path, file.metadata())?.len();
        let mut magic = [0; 2];
        io(path, file.read_exact(&mut magic))?;

        let flipped = match magic {
            [0x50, 0x4b] => false,
            [0xfa, 0xe1] => true,
            _ => return Err(Error::Archive("input is neither ZIP nor a flipped ZIP".into())),
        };

        Ok(Self {
            #[cfg(unix)]
            file: Arc::new(file),
            #[cfg(not(unix))]
            file: Arc::new(Mutex::new(file)),
            position: 0,
            length,
            flipped,
            control,
        })
    }

    /// Inspect the terminal EOCD before the ZIP library allocates its index.
    pub(crate) fn directory_info(&mut self) -> Result<DirectoryInfo> {
        let window = self.length.min(65535 + 22);
        self.seek(SeekFrom::End(-(window as i64))).map_err(Error::from_io)?;
        let mut tail = vec![0; window as usize];
        self.read_exact(&mut tail).map_err(Error::from_io)?;

        let offset = (0..tail.len().saturating_sub(21))
            .rev()
            .find(|&offset| {
                tail.get(offset..offset + 4) == Some(&[0x50, 0x4b, 0x05, 0x06])
                    && usize::from(le16(&tail, offset + 20)) + offset + 22 == tail.len()
            })
            .ok_or_else(|| Error::Archive("missing terminal ZIP directory record".into()))?;

        if le16(&tail, offset + 4) != 0
            || le16(&tail, offset + 6) != 0
            || le16(&tail, offset + 8) != le16(&tail, offset + 10)
        {
            return Err(Error::Archive("split ZIP archives are unsupported".into()));
        }

        let count = le16(&tail, offset + 10);
        let directory_size = le32(&tail, offset + 12);
        let directory_offset = le32(&tail, offset + 16);

        if count != u16::MAX && directory_size != u32::MAX && directory_offset != u32::MAX {
            return Ok(DirectoryInfo {
                entries: u64::from(count),
                size: u64::from(directory_size),
                offset: u64::from(directory_offset),
            });
        }

        let eocd_offset = self.length - window + offset as u64;
        let locator_offset =
            eocd_offset.checked_sub(20).ok_or_else(|| Error::Archive("missing ZIP64 locator".into()))?;
        self.seek(SeekFrom::Start(locator_offset)).map_err(Error::from_io)?;

        let mut locator = [0; 20];
        self.read_exact(&mut locator).map_err(Error::from_io)?;

        if locator[..8] != [0x50, 0x4b, 0x06, 0x07, 0, 0, 0, 0] || locator[16..] != [1, 0, 0, 0] {
            return Err(Error::Archive("invalid or split ZIP64 locator".into()));
        }

        let record_offset =
            u64::from_le_bytes(locator[8..16].try_into().map_err(|_| Error::Archive("ZIP64 offset".into()))?);
        self.seek(SeekFrom::Start(record_offset)).map_err(Error::from_io)?;

        let mut record = [0; 56];
        self.read_exact(&mut record).map_err(Error::from_io)?;

        if record[..4] != [0x50, 0x4b, 0x06, 0x06] || record[16..24] != [0; 8] || record[24..32] != record[32..40] {
            return Err(Error::Archive("invalid or split ZIP64 record".into()));
        }

        let count = u64::from_le_bytes(record[32..40].try_into().map_err(|_| Error::Archive("ZIP64 count".into()))?);
        let size = u64::from_le_bytes(record[40..48].try_into().map_err(|_| Error::Archive("ZIP64 size".into()))?);

        let offset =
            u64::from_le_bytes(record[48..56].try_into().map_err(|_| Error::Archive("ZIP64 directory offset".into()))?);

        Ok(DirectoryInfo { entries: count, size, offset })
    }

    /// Enforce the declared metadata extent against actual record lengths before
    /// allocating the library's directory index.
    pub(crate) fn validate_directory(&mut self, info: &DirectoryInfo, control: Control<'_>) -> Result<()> {
        let end = info
            .offset
            .checked_add(info.size)
            .filter(|end| *end <= self.length)
            .ok_or_else(|| Error::Archive("central directory exceeds the input".into()))?;
        self.seek(SeekFrom::Start(info.offset)).map_err(Error::from_io)?;

        for _ in 0..info.entries {
            control.check()?;

            if end.saturating_sub(self.position) < 46 {
                return Err(Error::Archive("truncated central directory record".into()));
            }

            let mut header = [0; 46];
            self.read_exact(&mut header).map_err(Error::from_io)?;

            if header[..4] != [0x50, 0x4b, 0x01, 0x02] {
                return Err(Error::Archive("invalid central directory record".into()));
            }

            let variable_size =
                u64::from(le16(&header, 28)) + u64::from(le16(&header, 30)) + u64::from(le16(&header, 32));
            let next = self
                .position
                .checked_add(variable_size)
                .filter(|next| *next <= end)
                .ok_or_else(|| Error::Archive("central directory fields exceed their extent".into()))?;
            self.seek(SeekFrom::Start(next)).map_err(Error::from_io)?;
        }

        if self.position != end {
            return Err(Error::Archive("central directory size or entry count is inconsistent".into()));
        }

        Ok(())
    }
}

impl Read for ArchiveReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.control.check().map_err(std::io::Error::other)?;

        #[cfg(unix)]
        let size = self.file.read_at(buffer, self.position)?;

        #[cfg(not(unix))]
        let size = {
            let mut file = self.file.lock().map_err(|_| std::io::Error::other("archive reader lock poisoned"))?;
            file.seek(SeekFrom::Start(self.position))?;
            file.read(buffer)?
        };

        if self.flipped {
            for byte in &mut buffer[..size] {
                *byte ^= 0xaa;
            }
        }

        self.position += size as u64;

        Ok(size)
    }
}

impl Seek for ArchiveReader<'_> {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        let position = match from {
            SeekFrom::Start(offset) => i128::from(offset),
            SeekFrom::Current(offset) => i128::from(self.position) + i128::from(offset),
            SeekFrom::End(offset) => i128::from(self.length) + i128::from(offset),
        };

        self.position = u64::try_from(position)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid archive offset"))?;

        Ok(self.position)
    }
}

fn le16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn le32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([bytes[offset], bytes[offset + 1], bytes[offset + 2], bytes[offset + 3]])
}

impl Error {
    fn from_io(source: std::io::Error) -> Self {
        crate::error::from_io(Path::new("<archive>"), source)
    }
}
