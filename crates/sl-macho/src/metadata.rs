use crate::{Endian, Error, LC_ENCRYPTION_INFO, LC_ENCRYPTION_INFO_64, Result, malformed};
use std::{collections::BTreeSet, io::Read};

const MAX_ARCHITECTURES: usize = 128;
const MAX_COMMAND_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchitectureMetadata {
    pub cpu_type: u32,
    pub cpu_subtype: u32,
    pub encrypted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryMetadata {
    pub architectures: Vec<ArchitectureMetadata>,
}

impl BinaryMetadata {
    /// Read architecture tables and load commands from a forward-only stream.
    /// This does not validate sections, signatures, or bytes beyond the inspected headers.
    pub fn read(reader: &mut impl Read, length: u64) -> Result<Self> {
        let mut magic = [0; 4];
        reader.read_exact(&mut magic)?;

        let format = match magic {
            [0xca, 0xfe, 0xba, 0xbe] => Some((Endian::Big, false)),
            [0xbe, 0xba, 0xfe, 0xca] => Some((Endian::Little, false)),
            [0xca, 0xfe, 0xba, 0xbf] => Some((Endian::Big, true)),
            [0xbf, 0xba, 0xfe, 0xca] => Some((Endian::Little, true)),
            _ => None,
        };

        let Some((endian, fat64)) = format else {
            let (architecture, _) = read_thin(reader, magic, length)?;

            return Ok(Self { architectures: vec![architecture] });
        };

        let mut count_bytes = [0; 4];
        reader.read_exact(&mut count_bytes)?;
        let count = endian.u32(&count_bytes, 0)? as usize;
        let stride = if fat64 { 32 } else { 20 };

        if count == 0 || count > MAX_ARCHITECTURES {
            return Err(malformed("empty, oversized, or truncated fat metadata table"));
        }

        let table_end = 8 + count * stride;

        if table_end as u64 > length {
            return Err(malformed("truncated fat metadata table"));
        }

        let mut table = vec![0; count * stride];
        reader.read_exact(&mut table)?;
        let mut slices = Vec::with_capacity(count);
        let mut cpu_pairs = BTreeSet::new();

        for record in table.chunks_exact(stride) {
            let cpu = (endian.u32(record, 0)?, endian.u32(record, 4)?);
            let (offset, size, alignment) = if fat64 {
                if endian.u32(record, 28)? != 0 {
                    return Err(malformed("nonzero fat_arch_64 reserved field"));
                }

                (endian.u64(record, 8)?, endian.u64(record, 16)?, endian.u32(record, 24)?)
            } else {
                (u64::from(endian.u32(record, 8)?), u64::from(endian.u32(record, 12)?), endian.u32(record, 16)?)
            };
            let boundary = 1u64.checked_shl(alignment).ok_or_else(|| malformed("fat alignment exponent"))?;
            let end = offset.checked_add(size).ok_or(Error::Overflow("fat slice extent"))?;

            if size < 28 || offset < table_end as u64 || !offset.is_multiple_of(boundary) || end > length {
                return Err(malformed("invalid fat metadata slice extent or alignment"));
            }

            if !cpu_pairs.insert(cpu) {
                return Err(malformed("duplicate fat architecture"));
            }

            slices.push((offset, size, cpu));
        }

        slices.sort_unstable_by_key(|slice| slice.0);

        for adjacent in slices.windows(2) {
            if adjacent[0].0 + adjacent[0].1 > adjacent[1].0 {
                return Err(malformed("overlapping fat slices"));
            }
        }

        let mut position = table_end as u64;
        let mut architectures = Vec::with_capacity(count);
        let mut buffer = vec![0; 128 * 1024];

        for (offset, size, expected_cpu) in slices {
            while position < offset {
                let amount = (offset - position).min(buffer.len() as u64) as usize;
                reader.read_exact(&mut buffer[..amount])?;
                position += amount as u64;
            }

            reader.read_exact(&mut magic)?;
            let (architecture, header_size) = read_thin(reader, magic, size)?;

            if (architecture.cpu_type, architecture.cpu_subtype) != expected_cpu {
                return Err(malformed("fat architecture disagrees with slice"));
            }

            position += header_size;
            architectures.push(architecture);
        }

        Ok(Self { architectures })
    }

    pub fn encrypted(&self) -> bool {
        self.architectures.iter().any(|architecture| architecture.encrypted)
    }
}

fn read_thin(reader: &mut impl Read, magic: [u8; 4], length: u64) -> Result<(ArchitectureMetadata, u64)> {
    let (endian, is_64) = match magic {
        [0xce, 0xfa, 0xed, 0xfe] => (Endian::Little, false),
        [0xcf, 0xfa, 0xed, 0xfe] => (Endian::Little, true),
        [0xfe, 0xed, 0xfa, 0xce] => (Endian::Big, false),
        [0xfe, 0xed, 0xfa, 0xcf] => (Endian::Big, true),
        _ => return Err(malformed("unrecognized Mach-O metadata magic")),
    };
    let header_size = if is_64 { 32 } else { 28 };

    if length < header_size as u64 {
        return Err(malformed("truncated Mach-O header"));
    }

    let mut header = vec![0; header_size];
    header[..4].copy_from_slice(&magic);
    reader.read_exact(&mut header[4..])?;

    let count = endian.u32(&header, 16)? as usize;
    let command_bytes = endian.u32(&header, 20)? as usize;
    let extent = header_size as u64 + command_bytes as u64;

    if command_bytes > MAX_COMMAND_BYTES || count > command_bytes / 8 || extent > length {
        return Err(malformed("oversized or truncated Mach-O load commands"));
    }

    let mut commands = vec![0; command_bytes];
    reader.read_exact(&mut commands)?;
    let mut position = 0;
    let mut encrypted = false;

    for _ in 0..count {
        let command = endian.u32(&commands, position)?;
        let size = endian.u32(&commands, position + 4)? as usize;
        let end = position.checked_add(size).ok_or(Error::Overflow("load command extent"))?;

        if size < 8 || !size.is_multiple_of(if is_64 { 8 } else { 4 }) || end > commands.len() {
            return Err(malformed("invalid metadata load command extent"));
        }

        if matches!(command, LC_ENCRYPTION_INFO | LC_ENCRYPTION_INFO_64) {
            let expected = if command == LC_ENCRYPTION_INFO_64 { 24 } else { 20 };

            if size != expected {
                return Err(malformed("invalid encryption command size"));
            }

            let offset = u64::from(endian.u32(&commands, position + 8)?);
            let size = u64::from(endian.u32(&commands, position + 12)?);

            if offset + size > length {
                return Err(malformed("encryption range exceeds slice"));
            }

            encrypted |= endian.u32(&commands, position + 16)? != 0;
        }

        position = end;
    }

    if position != commands.len() {
        return Err(malformed("load command count does not cover command bytes"));
    }

    let metadata =
        ArchitectureMetadata { cpu_type: endian.u32(&header, 4)?, cpu_subtype: endian.u32(&header, 8)?, encrypted };

    Ok((metadata, extent))
}
