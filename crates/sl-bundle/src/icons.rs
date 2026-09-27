use crate::{Error, Result};
use flate2::{Compression, read::DeflateDecoder, write::ZlibEncoder};
use image::{ImageFormat, ImageReader, Limits};
use std::io::{Cursor, Read, Write};

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const MAX_DIMENSION: u32 = 4096;
const MAX_SCANLINE_BYTES: u64 = 80 * 1024 * 1024;

pub(crate) fn normalize(bytes: &[u8]) -> Result<(Vec<u8>, u64)> {
    let (ordinary_png, cgbi) = remove_cgbi(bytes)?;
    let mut reader = ImageReader::with_format(Cursor::new(&ordinary_png), ImageFormat::Png);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);

    let mut pixels = reader.decode().map_err(|error| Error::Bundle(format!("cannot decode PNG: {error}")))?.to_rgba8();

    if cgbi {
        for pixel in pixels.pixels_mut() {
            pixel.0.swap(0, 2);
            let alpha = u32::from(pixel[3]);

            for component in &mut pixel.0[..3] {
                let numerator = u32::from(*component) * 255 + alpha / 2;
                let straight = numerator.checked_div(alpha).unwrap_or(0);
                *component = straight.min(255) as u8;
            }
        }
    }

    let area = u64::from(pixels.width()) * u64::from(pixels.height());
    let mut output = Cursor::new(Vec::new());
    pixels.write_to(&mut output, ImageFormat::Png).map_err(|error| Error::Bundle(error.to_string()))?;

    Ok((output.into_inner(), area))
}

fn remove_cgbi(bytes: &[u8]) -> Result<(Vec<u8>, bool)> {
    if !bytes.starts_with(PNG_SIGNATURE) {
        return Err(Error::Bundle("icon is not a PNG".into()));
    }

    let mut chunks = Vec::new();
    let mut position = 8;
    let mut cgbi = false;
    let mut seen_end = false;

    while position < bytes.len() {
        let header = bytes.get(position..position + 8).ok_or_else(|| Error::Bundle("truncated PNG chunk".into()))?;
        let length =
            u32::from_be_bytes(header[..4].try_into().map_err(|_| Error::Bundle("PNG length".into()))?) as usize;
        let end = position.checked_add(12).and_then(|end| end.checked_add(length)).ok_or(Error::Limit("PNG chunk"))?;
        let chunk = bytes.get(position..end).ok_or_else(|| Error::Bundle("truncated PNG payload".into()))?;
        let kind = &chunk[4..8];
        let payload = &chunk[8..8 + length];
        let expected_crc =
            u32::from_be_bytes(chunk[8 + length..].try_into().map_err(|_| Error::Bundle("PNG CRC".into()))?);

        if crc32fast::hash(&chunk[4..8 + length]) != expected_crc {
            return Err(Error::Bundle("PNG chunk CRC mismatch".into()));
        }

        cgbi |= kind == b"CgBI";
        chunks.push((kind, payload));
        position = end;

        if kind == b"IEND" {
            seen_end = true;
            break;
        }
    }

    if !seen_end || position != bytes.len() {
        return Err(Error::Bundle("missing PNG end or trailing bytes".into()));
    }

    if !cgbi {
        return Ok((bytes.to_vec(), false));
    }

    let header = chunks
        .iter()
        .find(|(kind, _)| *kind == b"IHDR")
        .map(|(_, payload)| *payload)
        .ok_or_else(|| Error::Bundle("CgBI icon has no image header".into()))?;

    if header.len() != 13 || header[8] != 8 || !matches!(header[9], 2 | 6) {
        return Err(Error::Bundle("CgBI requires 8-bit RGB or RGBA".into()));
    }

    let width = u32::from_be_bytes(header[..4].try_into().map_err(|_| Error::Bundle("PNG width".into()))?);
    let height = u32::from_be_bytes(header[4..8].try_into().map_err(|_| Error::Bundle("PNG height".into()))?);

    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(Error::Limit("icon dimensions"));
    }

    let compressed: Vec<u8> =
        chunks.iter().filter(|(kind, _)| *kind == b"IDAT").flat_map(|(_, payload)| payload.iter().copied()).collect();
    let mut decoder = DeflateDecoder::new(&compressed[..]);
    let mut scanlines = Vec::new();
    decoder
        .by_ref()
        .take(MAX_SCANLINE_BYTES + 1)
        .read_to_end(&mut scanlines)
        .map_err(|error| Error::Bundle(format!("CgBI decompression: {error}")))?;

    if scanlines.len() as u64 > MAX_SCANLINE_BYTES || decoder.total_in() != compressed.len() as u64 {
        return Err(Error::Limit("CgBI scanlines or trailing compressed bytes"));
    }

    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&scanlines).map_err(|error| Error::Bundle(error.to_string()))?;
    let encoded = encoder.finish().map_err(|error| Error::Bundle(error.to_string()))?;
    let mut output = PNG_SIGNATURE.to_vec();
    let mut wrote_data = false;

    for (kind, payload) in chunks {
        if kind == b"CgBI" {
            continue;
        }

        if kind == b"IDAT" {
            if !wrote_data {
                append_chunk(&mut output, b"IDAT", &encoded)?;
                wrote_data = true;
            }
        } else {
            append_chunk(&mut output, kind, payload)?;
        }
    }

    Ok((output, true))
}

fn append_chunk(output: &mut Vec<u8>, kind: &[u8], payload: &[u8]) -> Result<()> {
    let length = u32::try_from(payload.len()).map_err(|_| Error::Limit("PNG output chunk"))?;
    output.extend_from_slice(&length.to_be_bytes());
    let start = output.len();
    output.extend_from_slice(kind);
    output.extend_from_slice(payload);
    let crc = crc32fast::hash(&output[start..]);
    output.extend_from_slice(&crc.to_be_bytes());

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::DeflateEncoder;

    fn cgbi(width: u32) -> Vec<u8> {
        cgbi_scanline(width, 6, &[0, 15, 10, 5, 128, 99, 88, 77, 0])
    }

    fn cgbi_scanline(width: u32, color_type: u8, scanline: &[u8]) -> Vec<u8> {
        let mut header = [0; 13];
        header[..4].copy_from_slice(&width.to_be_bytes());
        header[4..8].copy_from_slice(&1u32.to_be_bytes());
        header[8] = 8;
        header[9] = color_type;

        let mut compressed = DeflateEncoder::new(Vec::new(), Compression::default());
        compressed.write_all(scanline).expect("scanline");
        let compressed = compressed.finish().expect("DEFLATE");
        let mut png = PNG_SIGNATURE.to_vec();
        append_chunk(&mut png, b"CgBI", &[0x40, 0xa0, 0x60, 0x82]).expect("CgBI");
        append_chunk(&mut png, b"IHDR", &header).expect("header");
        append_chunk(&mut png, b"IDAT", &compressed).expect("data");
        append_chunk(&mut png, b"IEND", &[]).expect("end");

        png
    }

    #[test]
    fn cgbi_converts_raw_deflate_bgra_and_premultiplied_alpha_to_ordinary_png() {
        let (png, area) = normalize(&cgbi(2)).expect("normalize");
        let pixels = image::load_from_memory(&png).expect("ordinary PNG").to_rgba8();

        assert_eq!(area, 2);
        assert_eq!(pixels.get_pixel(0, 0).0, [10, 20, 30, 128]);
        assert_eq!(pixels.get_pixel(1, 0).0, [0, 0, 0, 0]);
        assert!(!png.windows(4).any(|window| window == b"CgBI"));

        let (png, area) = normalize(&cgbi_scanline(1, 2, &[0, 30, 20, 10])).expect("RGB normalize");
        let pixels = image::load_from_memory(&png).expect("RGB PNG").to_rgba8();

        assert_eq!(area, 1);
        assert_eq!(pixels.get_pixel(0, 0).0, [10, 20, 30, 255]);
    }

    #[test]
    fn rejects_crc_corruption_trailing_bytes_and_excessive_dimensions() {
        let mut corrupt = cgbi(2);
        corrupt[16] ^= 1;
        assert!(normalize(&corrupt).is_err());

        let mut trailing = cgbi(2);
        trailing.push(0);
        assert!(normalize(&trailing).is_err());
        assert!(normalize(&cgbi(MAX_DIMENSION + 1)).is_err());
    }
}
