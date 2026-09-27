//! Bounds-checked Mach-O parsing and lossless load-command editing.
//!
//! Supports 32/64-bit thin and universal images in either byte order.
//! Format constants follow Apple's `mach-o/loader.h` and `mach-o/fat.h`.
//! Unknown load commands are preserved verbatim; no pointer casts are used.

#![forbid(unsafe_code)]

use std::ops::Range;
use thiserror::Error;

mod libraries;
mod metadata;

pub use libraries::library_key;
pub use metadata::{ArchitectureMetadata, BinaryMetadata};

pub const CPU_TYPE_ARM64: u32 = 0x0100_000c;
pub const LC_SEGMENT: u32 = 1;
pub const LC_SYMTAB: u32 = 2;
pub const LC_LOAD_DYLIB: u32 = 0xc;
pub const LC_ID_DYLIB: u32 = 0xd;
pub const LC_LOAD_WEAK_DYLIB: u32 = 0x8000_0018;
pub const LC_SEGMENT_64: u32 = 0x19;
pub const LC_RPATH: u32 = 0x8000_001c;
pub const LC_CODE_SIGNATURE: u32 = 0x1d;
pub const LC_DYLIB_CODE_SIGN_DRS: u32 = 0x2b;
pub const LC_ENCRYPTION_INFO: u32 = 0x21;
pub const LC_ENCRYPTION_INFO_64: u32 = 0x2c;

#[derive(Debug, Error)]
pub enum Error {
    #[error("cannot read Mach-O metadata: {0}")]
    Io(#[from] std::io::Error),
    #[error("malformed Mach-O: {0}")]
    Malformed(String),
    #[error("load commands require {needed} bytes; only {available} bytes are available")]
    HeaderSpace { needed: usize, available: usize },
    #[error("Mach-O field exceeds its representable range: {0}")]
    Overflow(&'static str),
    #[error("cannot allocate Mach-O output: {0}")]
    Allocation(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

fn malformed(message: impl Into<String>) -> Error {
    Error::Malformed(message.into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endian {
    Little,
    Big,
}

impl Endian {
    pub fn u32(self, bytes: &[u8], offset: usize) -> Result<u32> {
        let end = offset.checked_add(4).ok_or(Error::Overflow("u32 offset"))?;
        let mut value = [0; 4];
        value.copy_from_slice(bytes.get(offset..end).ok_or_else(|| malformed("truncated u32"))?);

        Ok(match self {
            Self::Little => u32::from_le_bytes(value),
            Self::Big => u32::from_be_bytes(value),
        })
    }

    pub fn u64(self, bytes: &[u8], offset: usize) -> Result<u64> {
        let end = offset.checked_add(8).ok_or(Error::Overflow("u64 offset"))?;
        let mut value = [0; 8];
        value.copy_from_slice(bytes.get(offset..end).ok_or_else(|| malformed("truncated u64"))?);

        Ok(match self {
            Self::Little => u64::from_le_bytes(value),
            Self::Big => u64::from_be_bytes(value),
        })
    }

    pub fn put_u32(self, bytes: &mut [u8], offset: usize, value: u32) -> Result<()> {
        let end = offset.checked_add(4).ok_or(Error::Overflow("u32 offset"))?;

        let encoded = match self {
            Self::Little => value.to_le_bytes(),
            Self::Big => value.to_be_bytes(),
        };

        let target = bytes.get_mut(offset..end).ok_or_else(|| malformed("truncated u32"))?;
        target.copy_from_slice(&encoded);

        Ok(())
    }

    pub fn put_u64(self, bytes: &mut [u8], offset: usize, value: u64) -> Result<()> {
        let end = offset.checked_add(8).ok_or(Error::Overflow("u64 offset"))?;

        let encoded = match self {
            Self::Little => value.to_le_bytes(),
            Self::Big => value.to_be_bytes(),
        };

        let target = bytes.get_mut(offset..end).ok_or_else(|| malformed("truncated u64"))?;
        target.copy_from_slice(&encoded);

        Ok(())
    }
}

pub fn align_up(value: usize, alignment: usize) -> Result<usize> {
    if !alignment.is_power_of_two() {
        return Err(malformed("alignment is not a power of two"));
    }

    value.checked_add(alignment - 1).map(|v| v & !(alignment - 1)).ok_or(Error::Overflow("alignment"))
}

pub fn checked_range(offset: u64, size: u64, limit: usize) -> Result<Range<usize>> {
    let end = offset.checked_add(size).ok_or(Error::Overflow("file range"))?;

    if end > limit as u64 {
        return Err(malformed(format!("file range {offset}..{end} exceeds {limit} bytes")));
    }

    Ok(offset as usize..end as usize)
}

pub fn to_u32(value: usize, field: &'static str) -> Result<u32> {
    value.try_into().map_err(|_| Error::Overflow(field))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Thin,
    Fat { endian: Endian, is_64: bool },
}

#[derive(Debug, Clone)]
pub struct Binary<'a> {
    pub format: Format,
    pub slices: Vec<Slice<'a>>,
}

#[derive(Debug, Clone)]
pub struct Slice<'a> {
    pub cpu_type: u32,
    pub cpu_subtype: u32,
    pub alignment_exponent: u32,
    pub offset: usize,
    pub image: MachO<'a>,
}

impl<'a> Binary<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        let format = match bytes.get(..4).ok_or_else(|| malformed("missing magic"))? {
            [0xca, 0xfe, 0xba, 0xbe] => Format::Fat { endian: Endian::Big, is_64: false },
            [0xbe, 0xba, 0xfe, 0xca] => Format::Fat { endian: Endian::Little, is_64: false },
            [0xca, 0xfe, 0xba, 0xbf] => Format::Fat { endian: Endian::Big, is_64: true },
            [0xbf, 0xba, 0xfe, 0xca] => Format::Fat { endian: Endian::Little, is_64: true },
            _ => Format::Thin,
        };

        let Format::Fat { endian, is_64 } = format else {
            let image = MachO::parse(bytes)?;

            return Ok(Self {
                format,
                slices: vec![Slice {
                    cpu_type: image.cpu_type,
                    cpu_subtype: image.cpu_subtype,
                    alignment_exponent: 0,
                    offset: 0,
                    image,
                }],
            });
        };

        let count = endian.u32(bytes, 4)? as usize;
        let stride: usize = if is_64 { 32 } else { 20 };
        let table_end = count.checked_mul(stride).and_then(|v| v.checked_add(8)).ok_or(Error::Overflow("fat table"))?;

        if count == 0 || table_end > bytes.len() {
            return Err(malformed("empty or truncated fat table"));
        }

        let mut slices = Vec::with_capacity(count);
        let mut ranges = Vec::with_capacity(count);
        let mut architectures = std::collections::HashSet::with_capacity(count);

        for i in 0..count {
            let arch_offset = 8 + i * stride;
            let cpu_type = endian.u32(bytes, arch_offset)?;
            let cpu_subtype = endian.u32(bytes, arch_offset + 4)?;

            let (offset, size, alignment_exponent) = if is_64 {
                if endian.u32(bytes, arch_offset + 28)? != 0 {
                    return Err(malformed("nonzero fat_arch_64 reserved field"));
                }

                (
                    endian.u64(bytes, arch_offset + 8)?,
                    endian.u64(bytes, arch_offset + 16)?,
                    endian.u32(bytes, arch_offset + 24)?,
                )
            } else {
                (
                    u64::from(endian.u32(bytes, arch_offset + 8)?),
                    u64::from(endian.u32(bytes, arch_offset + 12)?),
                    endian.u32(bytes, arch_offset + 16)?,
                )
            };

            let alignment =
                1u64.checked_shl(alignment_exponent).ok_or_else(|| malformed("invalid fat alignment exponent"))?;

            if offset < table_end as u64 || !offset.is_multiple_of(alignment) || size == 0 {
                return Err(malformed("fat slice overlaps header, is misaligned, or is empty"));
            }

            let range = checked_range(offset, size, bytes.len())?;
            let image = MachO::parse(&bytes[range.clone()])?;

            if image.cpu_type != cpu_type || image.cpu_subtype != cpu_subtype {
                return Err(malformed("fat architecture disagrees with slice"));
            }

            if !architectures.insert((cpu_type, cpu_subtype)) {
                return Err(malformed("duplicate fat architecture"));
            }

            ranges.push(range.clone());
            slices.push(Slice { cpu_type, cpu_subtype, alignment_exponent, offset: range.start, image });
        }

        ranges.sort_unstable_by_key(|r| r.start);

        if ranges.windows(2).any(|r| r[0].end > r[1].start) {
            return Err(malformed("overlapping fat slices"));
        }

        Ok(Self { format, slices })
    }

    /// Rebuild with deterministic padding. Thin images stay thin.
    pub fn rebuild(&self, images: &[Vec<u8>], alignment_exponent: u32) -> Result<Vec<u8>> {
        if images.len() != self.slices.len() {
            return Err(malformed("replacement slice count differs"));
        }

        for (image, slice) in images.iter().zip(&self.slices) {
            let parsed = MachO::parse(image)?;

            if parsed.cpu_type != slice.cpu_type || parsed.cpu_subtype != slice.cpu_subtype {
                return Err(malformed("replacement architecture differs"));
            }
        }

        let Format::Fat { endian, is_64 } = self.format else {
            return images.first().cloned().ok_or_else(|| malformed("empty thin image"));
        };

        let alignment = 1usize.checked_shl(alignment_exponent).ok_or(Error::Overflow("fat alignment"))?;
        let stride = if is_64 { 32 } else { 20 };
        let mut total = 8 + stride * images.len();
        let mut offsets = Vec::with_capacity(images.len());

        for image in images {
            total = align_up(total, alignment)?;

            if !is_64 {
                to_u32(total, "fat offset")?;
                to_u32(image.len(), "fat size")?;
            }

            offsets.push(total);
            total = total.checked_add(image.len()).ok_or(Error::Overflow("fat file size"))?;
        }

        let mut output = Vec::new();
        output.try_reserve_exact(total).map_err(|e| Error::Allocation(e.to_string()))?;
        output.resize(total, 0);

        endian.put_u32(&mut output, 0, if is_64 { 0xcafe_babf } else { 0xcafe_babe })?;
        endian.put_u32(&mut output, 4, to_u32(images.len(), "fat count")?)?;

        for (i, ((image, slice), offset)) in images.iter().zip(&self.slices).zip(offsets).enumerate() {
            let arch_offset = 8 + i * stride;

            endian.put_u32(&mut output, arch_offset, slice.cpu_type)?;
            endian.put_u32(&mut output, arch_offset + 4, slice.cpu_subtype)?;

            if is_64 {
                endian.put_u64(&mut output, arch_offset + 8, offset as u64)?;
                endian.put_u64(&mut output, arch_offset + 16, image.len() as u64)?;
                endian.put_u32(&mut output, arch_offset + 24, alignment_exponent)?;
            } else {
                endian.put_u32(&mut output, arch_offset + 8, to_u32(offset, "fat offset")?)?;
                endian.put_u32(&mut output, arch_offset + 12, to_u32(image.len(), "fat size")?)?;
                endian.put_u32(&mut output, arch_offset + 16, alignment_exponent)?;
            }

            output[offset..offset + image.len()].copy_from_slice(image);
        }

        Ok(output)
    }
}

#[derive(Debug, Clone)]
pub struct LoadCommand<'a> {
    pub kind: u32,
    pub offset: usize,
    pub bytes: &'a [u8],
}

#[derive(Debug, Clone)]
pub struct Section {
    pub name: String,
    pub segment_name: String,
    pub address: u64,
    pub size: u64,
    pub offset: u32,
    pub flags: u32,
}

impl Section {
    pub fn is_zero_fill(&self) -> bool {
        matches!(self.flags & 0xff, 1 | 0xc | 0x12)
    }
}

#[derive(Debug, Clone)]
pub struct Segment {
    pub name: String,
    pub command_offset: usize,
    pub vm_address: u64,
    pub vm_size: u64,
    pub file_offset: u64,
    pub file_size: u64,
    pub sections: Vec<Section>,
}

#[derive(Debug, Clone)]
pub struct MachO<'a> {
    pub bytes: &'a [u8],
    pub endian: Endian,
    pub is_64: bool,
    pub cpu_type: u32,
    pub cpu_subtype: u32,
    pub file_type: u32,
    pub header_size: usize,
    pub commands_end: usize,
    pub commands: Vec<LoadCommand<'a>>,
    pub segments: Vec<Segment>,
    pub signature: Option<Range<usize>>,
    /// First occupied offset after the commands; limits header-padding insertion.
    pub content_start: usize,
    pub encrypted: bool,
    referenced_ranges: Vec<Range<usize>>,
}

fn name(bytes: &[u8]) -> Result<String> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());

    std::str::from_utf8(&bytes[..end]).map(str::to_owned).map_err(|_| malformed("non-UTF-8 segment or section name"))
}

impl<'a> MachO<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        let (endian, is_64) = match bytes.get(..4) {
            Some([0xce, 0xfa, 0xed, 0xfe]) => (Endian::Little, false),
            Some([0xcf, 0xfa, 0xed, 0xfe]) => (Endian::Little, true),
            Some([0xfe, 0xed, 0xfa, 0xce]) => (Endian::Big, false),
            Some([0xfe, 0xed, 0xfa, 0xcf]) => (Endian::Big, true),
            _ => return Err(malformed("unrecognized thin magic")),
        };

        let header_size: usize = if is_64 { 32 } else { 28 };

        if bytes.len() < header_size {
            return Err(malformed("truncated header"));
        }

        let count = endian.u32(bytes, 16)? as usize;
        let command_size = endian.u32(bytes, 20)? as usize;
        let commands_end = header_size.checked_add(command_size).ok_or(Error::Overflow("load commands"))?;

        if commands_end > bytes.len() || count > command_size / 8 {
            return Err(malformed("truncated commands or impossible count"));
        }

        let mut image = Self {
            bytes,
            endian,
            is_64,
            cpu_type: endian.u32(bytes, 4)?,
            cpu_subtype: endian.u32(bytes, 8)?,
            file_type: endian.u32(bytes, 12)?,
            header_size,
            commands_end,
            commands: Vec::with_capacity(count),
            segments: Vec::new(),
            signature: None,
            content_start: bytes.len(),
            encrypted: false,
            referenced_ranges: Vec::new(),
        };

        let mut offset = header_size;

        for _ in 0..count {
            let kind = endian.u32(bytes, offset)?;
            let size = endian.u32(bytes, offset + 4)? as usize;
            let end = offset.checked_add(size).ok_or(Error::Overflow("load command"))?;

            if size < 8 || !size.is_multiple_of(if is_64 { 8 } else { 4 }) || end > commands_end {
                return Err(malformed("invalid command length or alignment"));
            }

            let command = LoadCommand { kind, offset, bytes: &bytes[offset..end] };
            image.validate_command(&command)?;
            image.commands.push(command);
            offset = end;
        }

        if offset != commands_end {
            return Err(malformed("sizeofcmds differs from command lengths"));
        }

        if image.content_start < commands_end {
            return Err(malformed("file data overlaps load commands"));
        }

        Ok(image)
    }

    fn occupy(&mut self, offset: u64, size: u64) -> Result<()> {
        let range = checked_range(offset, size, self.bytes.len())?;

        if size != 0 {
            self.content_start = self.content_start.min(range.start);
            self.referenced_ranges.push(range);
        }

        Ok(())
    }

    fn validate_segment(&mut self, command: &LoadCommand<'_>) -> Result<()> {
        let bytes = command.bytes;
        let endian = self.endian;
        let is_64 = command.kind == LC_SEGMENT_64;

        if is_64 != self.is_64 {
            return Err(malformed("segment width differs from header"));
        }

        let header_size = if is_64 { 72usize } else { 56 };
        let section_size = if is_64 { 80usize } else { 68 };

        if bytes.len() < header_size {
            return Err(malformed("truncated segment command"));
        }

        let section_count = endian.u32(bytes, if is_64 { 64 } else { 48 })? as usize;

        if section_count.checked_mul(section_size).and_then(|size| size.checked_add(header_size)) != Some(bytes.len()) {
            return Err(malformed("invalid segment section count"));
        }

        let mut segment = Segment {
            name: name(&bytes[8..24])?,
            command_offset: command.offset,
            vm_address: if is_64 { endian.u64(bytes, 24)? } else { u64::from(endian.u32(bytes, 24)?) },
            vm_size: if is_64 { endian.u64(bytes, 32)? } else { u64::from(endian.u32(bytes, 28)?) },
            file_offset: if is_64 { endian.u64(bytes, 40)? } else { u64::from(endian.u32(bytes, 32)?) },
            file_size: if is_64 { endian.u64(bytes, 48)? } else { u64::from(endian.u32(bytes, 36)?) },
            sections: Vec::with_capacity(section_count),
        };

        checked_range(segment.file_offset, segment.file_size, self.bytes.len())?;

        if segment.file_offset != 0 && segment.file_size != 0 {
            self.content_start = self.content_start.min(segment.file_offset as usize);
        }

        for section_bytes in bytes[header_size..].chunks_exact(section_size) {
            let section = Section {
                name: name(&section_bytes[..16])?,
                segment_name: name(&section_bytes[16..32])?,
                address: if is_64 { endian.u64(section_bytes, 32)? } else { u64::from(endian.u32(section_bytes, 32)?) },
                size: if is_64 { endian.u64(section_bytes, 40)? } else { u64::from(endian.u32(section_bytes, 36)?) },
                offset: endian.u32(section_bytes, if is_64 { 48 } else { 40 })?,
                flags: endian.u32(section_bytes, if is_64 { 64 } else { 56 })?,
            };

            if !section.is_zero_fill() && section.size != 0 {
                let section_end =
                    u64::from(section.offset).checked_add(section.size).ok_or(Error::Overflow("section"))?;
                let segment_end =
                    segment.file_offset.checked_add(segment.file_size).ok_or(Error::Overflow("segment"))?;

                if u64::from(section.offset) < segment.file_offset || section_end > segment_end {
                    return Err(malformed("section lies outside file segment"));
                }

                self.occupy(u64::from(section.offset), section.size)?;
            }

            let relocation_offset = endian.u32(section_bytes, if is_64 { 56 } else { 48 })?;
            let relocation_count = endian.u32(section_bytes, if is_64 { 60 } else { 52 })?;

            self.occupy(u64::from(relocation_offset), u64::from(relocation_count) * 8)?;

            segment.sections.push(section);
        }

        self.segments.push(segment);

        Ok(())
    }

    fn validate_command(&mut self, command: &LoadCommand<'_>) -> Result<()> {
        let bytes = command.bytes;
        let endian = self.endian;

        match command.kind {
            LC_SEGMENT | LC_SEGMENT_64 => self.validate_segment(command)?,

            LC_CODE_SIGNATURE
            | LC_DYLIB_CODE_SIGN_DRS
            | 0x1e
            | 0x26
            | 0x29
            | 0x2e
            | 0x36
            | 0x37
            | 0x38
            | 0x3a
            | 0x8000_0033
            | 0x8000_0034 => {
                if bytes.len() != 16 {
                    return Err(malformed("invalid linkedit command size"));
                }

                let offset = u64::from(endian.u32(bytes, 8)?);
                let size = u64::from(endian.u32(bytes, 12)?);

                if command.kind == LC_CODE_SIGNATURE {
                    if self.signature.is_some() || size == 0 || offset < self.commands_end as u64 {
                        return Err(malformed("duplicate, empty, or overlapping signature"));
                    }

                    self.signature = Some(checked_range(offset, size, self.bytes.len())?);
                    self.content_start = self.content_start.min(offset as usize);
                } else {
                    self.occupy(offset, size)?;
                }
            }

            LC_ENCRYPTION_INFO | LC_ENCRYPTION_INFO_64 => {
                if bytes.len() != if command.kind == LC_ENCRYPTION_INFO_64 { 24 } else { 20 } {
                    return Err(malformed("invalid encryption command size"));
                }

                self.occupy(u64::from(endian.u32(bytes, 8)?), u64::from(endian.u32(bytes, 12)?))?;
                self.encrypted |= endian.u32(bytes, 16)? != 0;
            }

            LC_SYMTAB => {
                if bytes.len() != 24 {
                    return Err(malformed("invalid symbol table command size"));
                }

                self.occupy(
                    u64::from(endian.u32(bytes, 8)?),
                    u64::from(endian.u32(bytes, 12)?) * if self.is_64 { 16 } else { 12 },
                )?;
                self.occupy(u64::from(endian.u32(bytes, 16)?), u64::from(endian.u32(bytes, 20)?))?;
            }

            0xb => {
                if bytes.len() != 80 {
                    return Err(malformed("invalid dynamic symbol table command size"));
                }

                for (offset, stride) in
                    [(32, 8), (40, if self.is_64 { 56 } else { 52 }), (48, 4), (56, 4), (64, 8), (72, 8)]
                {
                    self.occupy(
                        u64::from(endian.u32(bytes, offset)?),
                        u64::from(endian.u32(bytes, offset + 4)?) * stride,
                    )?;
                }
            }

            0x16 => {
                if bytes.len() != 16 {
                    return Err(malformed("invalid two-level hints command size"));
                }

                self.occupy(u64::from(endian.u32(bytes, 8)?), u64::from(endian.u32(bytes, 12)?) * 4)?;
            }

            0x31 => {
                if bytes.len() != 40 {
                    return Err(malformed("invalid note command size"));
                }

                self.occupy(endian.u64(bytes, 24)?, endian.u64(bytes, 32)?)?;
            }

            0x8000_0028 => {
                if bytes.len() != 24 {
                    return Err(malformed("invalid entry point command size"));
                }

                self.occupy(endian.u64(bytes, 8)?, 1)?;
            }

            0x22 | 0x8000_0022 => {
                if bytes.len() != 48 {
                    return Err(malformed("invalid dyld info command size"));
                }

                for field_offset in (8..48).step_by(8) {
                    let data_offset = u64::from(endian.u32(bytes, field_offset)?);
                    let data_size = u64::from(endian.u32(bytes, field_offset + 4)?);

                    self.occupy(data_offset, data_size)?;
                }
            }

            LC_LOAD_DYLIB | LC_LOAD_WEAK_DYLIB | LC_ID_DYLIB | 0x8000_001f | 0x8000_0023 | 0x20 => {
                if bytes.len() < 24 {
                    return Err(malformed("truncated dylib command"));
                }

                let name_offset = endian.u32(bytes, 8)? as usize;

                if name_offset < 24 || name_offset >= bytes.len() || !bytes[name_offset..].contains(&0) {
                    return Err(malformed("invalid dylib name offset or terminator"));
                }
            }

            LC_RPATH => {
                if bytes.len() < 12 {
                    return Err(malformed("truncated rpath command"));
                }

                let path_offset = endian.u32(bytes, 8)? as usize;

                if path_offset < 12 || path_offset >= bytes.len() || !bytes[path_offset..].contains(&0) {
                    return Err(malformed("invalid rpath offset or terminator"));
                }
            }

            _ => {}
        }

        Ok(())
    }

    /// Validate that discarding a terminal signature does not truncate another object.
    pub fn unsigned_len(&self) -> Result<usize> {
        let Some(range) = &self.signature else {
            return Ok(self.bytes.len());
        };

        if self.bytes[range.end..].iter().any(|&byte| byte != 0) {
            return Err(malformed("file data follows the signature"));
        }

        if self.referenced_ranges.iter().any(|referenced| referenced.end > range.start) {
            return Err(malformed("signature removal would truncate referenced file data"));
        }

        for segment in &self.segments {
            if segment.name != "__LINKEDIT" && segment.file_offset + segment.file_size > range.start as u64 {
                return Err(malformed("signature overlaps a non-linkedit segment"));
            }

            for section in &segment.sections {
                if !section.is_zero_fill() && u64::from(section.offset) + section.size > range.start as u64 {
                    return Err(malformed("signature overlaps a section"));
                }
            }
        }

        Ok(range.start)
    }

    pub fn rewrite_commands(&self, commands: &[Vec<u8>]) -> Result<Vec<u8>> {
        let commands_size = commands
            .iter()
            .try_fold(0usize, |size, command| size.checked_add(command.len()).ok_or(Error::Overflow("commands")))?;

        let commands_end = self.header_size.checked_add(commands_size).ok_or(Error::Overflow("header"))?;

        if commands_end > self.content_start
            || (commands_end > self.commands_end
                && self.bytes[self.commands_end..commands_end].iter().any(|&byte| byte != 0))
        {
            return Err(Error::HeaderSpace { needed: commands_size, available: self.content_start - self.header_size });
        }

        let mut output = self.bytes.to_vec();
        output[self.header_size..self.commands_end.max(commands_end)].fill(0);

        self.endian.put_u32(&mut output, 16, to_u32(commands.len(), "command count")?)?;
        self.endian.put_u32(&mut output, 20, to_u32(commands_size, "sizeofcmds")?)?;

        let mut offset = self.header_size;

        for command in commands {
            if command.len() < 8
                || !command.len().is_multiple_of(if self.is_64 { 8 } else { 4 })
                || self.endian.u32(command, 4)? as usize != command.len()
            {
                return Err(malformed("invalid replacement command"));
            }

            output[offset..offset + command.len()].copy_from_slice(command);
            offset += command.len();
        }

        Ok(output)
    }

    /// Add a supplied library; exact existing paths are idempotent.
    pub fn add_dylib(&self, path: &str, weak: bool) -> Result<Vec<u8>> {
        if path.is_empty() || path.as_bytes().contains(&0) {
            return Err(malformed("empty or NUL-containing dylib path"));
        }

        for command in &self.commands {
            if matches!(command.kind, LC_LOAD_DYLIB | LC_LOAD_WEAK_DYLIB) {
                let name_offset = self.endian.u32(command.bytes, 8)? as usize;

                if name(&command.bytes[name_offset..])? == path {
                    return Ok(self.bytes.to_vec());
                }
            }
        }

        let unaligned_size = 24usize
            .checked_add(path.len())
            .and_then(|size| size.checked_add(1))
            .ok_or(Error::Overflow("dylib path"))?;
        let size = align_up(unaligned_size, if self.is_64 { 8 } else { 4 })?;

        let mut command = vec![0; size];
        self.endian.put_u32(&mut command, 0, if weak { LC_LOAD_WEAK_DYLIB } else { LC_LOAD_DYLIB })?;
        self.endian.put_u32(&mut command, 4, to_u32(size, "dylib command size")?)?;
        self.endian.put_u32(&mut command, 8, 24)?;
        command[24..24 + path.len()].copy_from_slice(path.as_bytes());

        let mut commands: Vec<_> = self.commands.iter().map(|command| command.bytes.to_vec()).collect();
        commands.push(command);

        self.rewrite_commands(&commands)
    }

    /// Update LC_CODE_SIGNATURE and __LINKEDIT without moving existing file data.
    /// A zero size removes the signature command and truncates its terminal blob.
    pub fn prepare_signature(&self, offset: usize, size: usize) -> Result<Vec<u8>> {
        let unsigned_len = self.unsigned_len()?;

        if size != 0 && offset < unsigned_len {
            return Err(malformed("new signature would overwrite file data"));
        }

        let file_end =
            if size == 0 { unsigned_len } else { offset.checked_add(size).ok_or(Error::Overflow("signature end"))? };

        let mut commands = Vec::with_capacity(self.commands.len() + 1);

        for command in &self.commands {
            if command.kind == LC_CODE_SIGNATURE {
                continue;
            }

            let mut bytes = command.bytes.to_vec();

            if matches!(command.kind, LC_SEGMENT | LC_SEGMENT_64) && name(&bytes[8..24])? == "__LINKEDIT" {
                let is_64 = command.kind == LC_SEGMENT_64;
                let file_offset =
                    if is_64 { self.endian.u64(&bytes, 40)? } else { u64::from(self.endian.u32(&bytes, 32)?) };

                let file_size = (file_end as u64)
                    .checked_sub(file_offset)
                    .ok_or_else(|| malformed("linkedit starts past file end"))?;
                let vm_size =
                    align_up(usize::try_from(file_size).map_err(|_| Error::Overflow("linkedit size"))?, 16384)?;

                if is_64 {
                    self.endian.put_u64(&mut bytes, 48, file_size)?;
                    self.endian.put_u64(&mut bytes, 32, vm_size as u64)?;
                } else {
                    self.endian.put_u32(&mut bytes, 36, to_u32(file_size as usize, "linkedit size")?)?;
                    self.endian.put_u32(&mut bytes, 28, to_u32(vm_size, "linkedit VM size")?)?;
                }
            }

            commands.push(bytes);
        }

        if size != 0 {
            let mut signature_command = vec![0; 16];
            self.endian.put_u32(&mut signature_command, 0, LC_CODE_SIGNATURE)?;
            self.endian.put_u32(&mut signature_command, 4, 16)?;
            self.endian.put_u32(&mut signature_command, 8, to_u32(offset, "signature offset")?)?;
            self.endian.put_u32(&mut signature_command, 12, to_u32(size, "signature size")?)?;

            commands.push(signature_command);
        }

        let mut output = self.rewrite_commands(&commands)?;
        output.truncate(unsigned_len);

        if file_end > output.len() {
            output.try_reserve_exact(file_end - output.len()).map_err(|error| Error::Allocation(error.to_string()))?;
            output.resize(file_end, 0);
        }

        Ok(output)
    }
}
