use crate::error::io;
use crate::files;
use crate::{BundleArchive, Control, Error, Result, read_dictionary};
use flate2::{Compression, read::DeflateDecoder, write::ZlibEncoder};
use image::imageops::FilterType;
use image::{ImageFormat, ImageReader, Limits, RgbaImage};
use plist::{Dictionary, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const MAX_DIMENSION: u32 = 4096;
const MAX_SCANLINE_BYTES: u64 = 80 * 1024 * 1024;

/// Size used when an existing icon cannot be measured, matching the recovered 0x80 default.
const DEFAULT_ICON_SIZE: u32 = 128;

/// Which icon files a custom-icon pass replaced, with the pixel size each was resized to.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IconReport {
    /// App-relative icon paths and their new square dimension, sorted by path.
    pub replaced: Vec<(PathBuf, u32)>,
}

impl BundleArchive {
    /// Replace the app's loose PNG icons with resized copies of a user PNG, reconstructed from
    /// the Go `ipa.ReplaceAppIcon` (`decompiled/go/sideloadly_ipa.c`).
    ///
    /// Each declared icon file is overwritten with the user image resized to that file's own
    /// pixel size; ancillary chunks and CgBI encoding in the user PNG are dropped. The recovered
    /// code does not touch Info.plist (the existing `CFBundleIcons`/`CFBundleIconFiles` entries
    /// already name these files) and refuses Assets-catalog icons; Sideport does exactly that.
    pub fn replace_icon(&mut self, png: &[u8], control: Control<'_>) -> Result<IconReport> {
        control.check()?;

        let root = self.bundle_path();
        let info = read_dictionary(&files::read_inside(&root, Path::new("Info.plist"))?)?;
        let bases = icon_bases(&info)?;

        let mut targets = matching_icon_files(&root, &bases)?;
        targets.sort_unstable();

        if targets.is_empty() {
            if io(&root.join("Assets.car"), root.join("Assets.car").try_exists())? {
                return Err(Error::Bundle("this app packs its icons in Assets.car, which cannot be replaced".into()));
            }

            return Err(Error::Bundle("no loose icon files matched the declared icon names".into()));
        }

        let source = prepare_source(png)?;
        let mut resized: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
        let mut replaced = Vec::with_capacity(targets.len());

        for relative in targets {
            control.check()?;

            let path = files::read_inside(&root, &relative)?;
            let size = measured_size(&path);

            let encoded = match resized.get(&size) {
                Some(encoded) => encoded,
                None => {
                    let encoded = encode_resized(&source, size)?;

                    resized.entry(size).or_insert(encoded)
                }
            };

            files::atomic_write(&path, encoded)?;
            replaced.push((relative, size));
        }

        Ok(IconReport { replaced })
    }
}

/// Gather declared icon base names from Info.plist, matching the recovered `findIconBases`:
/// `CFBundleIcons~ipad`/`CFBundleIcons` → `CFBundlePrimaryIcon` → `CFBundleIconFiles`, plus a
/// top-level `CFBundleIconFiles`. Assets-catalog references are refused up front.
fn icon_bases(info: &Dictionary) -> Result<Vec<String>> {
    let mut bases = Vec::new();

    for key in ["CFBundleIcons~ipad", "CFBundleIcons"] {
        let primary = info
            .get(key)
            .and_then(Value::as_dictionary)
            .and_then(|icons| icons.get("CFBundlePrimaryIcon"))
            .and_then(Value::as_dictionary)
            .and_then(|primary| primary.get("CFBundleIconFiles"));

        collect_strings(primary, &mut bases);
    }

    collect_strings(info.get("CFBundleIconFiles"), &mut bases);
    collect_strings(info.get("CFBundleIconFile"), &mut bases);

    bases.sort_unstable();
    bases.dedup();

    if bases.is_empty() {
        return Err(Error::Bundle("Info.plist declares no icon files".into()));
    }

    if bases.iter().any(|base| base.starts_with("Assets.car/")) {
        return Err(Error::Bundle("this app packs its icons in Assets.car, which cannot be replaced".into()));
    }

    Ok(bases)
}

fn collect_strings(value: Option<&Value>, names: &mut Vec<String>) {
    match value {
        Some(Value::String(name)) if !name.is_empty() => names.push(name.clone()),
        Some(Value::Array(values)) => {
            for value in values {
                collect_strings(Some(value), names);
            }
        }
        _ => {}
    }
}

/// Root-level PNGs whose name matches a declared base, optionally with an `@2x`/`~ipad` suffix.
fn matching_icon_files(root: &Path, bases: &[String]) -> Result<Vec<PathBuf>> {
    let mut matches = Vec::new();

    for entry in io(root, fs::read_dir(root))? {
        let entry = io(root, entry)?;

        if !io(&entry.path(), entry.file_type())?.is_file() {
            continue;
        }

        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".png").or_else(|| name.strip_suffix(".PNG")) else {
            continue;
        };

        let matched = bases.iter().any(|base| {
            let base = base.strip_suffix(".png").unwrap_or(base);

            stem == base || stem.strip_prefix(base).is_some_and(|suffix| suffix.starts_with(['@', '~']))
        });

        if matched {
            matches.push(PathBuf::from(name));
        }
    }

    Ok(matches)
}

/// Decode a user PNG into straight RGBA, stripping ancillary chunks and normalizing CgBI. The
/// re-decoded pixels carry no `iCCP`/`gAMA` metadata into the resized output.
fn prepare_source(png: &[u8]) -> Result<RgbaImage> {
    let (normalized, _area) = normalize(png)?;

    image::load_from_memory_with_format(&normalized, ImageFormat::Png)
        .map_err(|error| Error::Bundle(format!("cannot load new icon: {error}")))
        .map(|image| image.to_rgba8())
}

fn encode_resized(source: &RgbaImage, size: u32) -> Result<Vec<u8>> {
    let resized = image::imageops::resize(source, size, size, FilterType::Lanczos3);
    let mut output = Cursor::new(Vec::new());

    resized.write_to(&mut output, ImageFormat::Png).map_err(|error| Error::Bundle(error.to_string()))?;

    Ok(output.into_inner())
}

/// The square size an existing icon should keep, or the recovered default when it cannot decode.
fn measured_size(path: &Path) -> u32 {
    let dimensions = fs::read(path).ok().and_then(|bytes| png_dimensions(&bytes).ok());

    match dimensions {
        Some((width, height)) => width.max(height).clamp(1, MAX_DIMENSION),
        None => DEFAULT_ICON_SIZE,
    }
}

fn png_dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
    let (ordinary, _cgbi) = remove_cgbi(bytes)?;
    let mut reader = ImageReader::with_format(Cursor::new(ordinary), ImageFormat::Png);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    reader.limits(limits);

    reader.into_dimensions().map_err(|error| Error::Bundle(format!("cannot measure icon: {error}")))
}

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

    fn solid_png(size: u32, color: [u8; 4]) -> Vec<u8> {
        let image = RgbaImage::from_pixel(size, size, image::Rgba(color));
        let mut output = Cursor::new(Vec::new());
        image.write_to(&mut output, ImageFormat::Png).expect("png");

        output.into_inner()
    }

    fn app_with(root: &Path, icons: Value, files: &[(&str, u32)]) {
        let mut info = Dictionary::new();
        info.insert("CFBundleIdentifier".into(), "com.example.app".into());
        info.insert("CFBundleExecutable".into(), "App".into());
        info.insert("CFBundlePackageType".into(), "APPL".into());
        info.insert("CFBundleIcons".into(), icons);

        fs::create_dir_all(root).expect("app dir");
        Value::Dictionary(info).to_file_xml(root.join("Info.plist")).expect("info");
        fs::write(root.join("App"), b"executable").expect("executable");

        for (name, size) in files {
            fs::write(root.join(name), solid_png(*size, [10, 20, 30, 255])).expect("icon");
        }
    }

    fn primary_icon(files: &[&str]) -> Value {
        let mut primary = Dictionary::new();
        primary.insert("CFBundleIconFiles".into(), Value::Array(files.iter().map(|name| Value::from(*name)).collect()));

        let mut icons = Dictionary::new();
        icons.insert("CFBundlePrimaryIcon".into(), Value::Dictionary(primary));

        Value::Dictionary(icons)
    }

    fn dimensions(path: &Path) -> (u32, u32) {
        image::load_from_memory(&fs::read(path).expect("read")).expect("decode").into_rgba8().dimensions()
    }

    #[test]
    fn replaces_declared_icons_with_resized_copies_at_their_original_sizes() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let root = temporary.path().join("App.app");

        app_with(
            &root,
            primary_icon(&["AppIcon60x60", "AppIcon76x76"]),
            &[("AppIcon60x60@2x.png", 120), ("AppIcon76x76~ipad.png", 152), ("Unrelated.png", 64)],
        );

        let mut archive =
            crate::BundleArchive::unpack(&root, crate::ArchiveLimits::default(), Control::default()).expect("unpack");
        let report = archive.replace_icon(&solid_png(300, [200, 100, 50, 255]), Control::default()).expect("replace");

        assert_eq!(
            report.replaced,
            vec![(PathBuf::from("AppIcon60x60@2x.png"), 120), (PathBuf::from("AppIcon76x76~ipad.png"), 152)]
        );

        let staged = archive.bundle_path();
        assert_eq!(dimensions(&staged.join("AppIcon60x60@2x.png")), (120, 120));
        assert_eq!(dimensions(&staged.join("AppIcon76x76~ipad.png")), (152, 152));
        assert_eq!(dimensions(&staged.join("Unrelated.png")), (64, 64), "non-icon PNG is left alone");

        let pixel = image::load_from_memory(&fs::read(staged.join("AppIcon60x60@2x.png")).expect("value"))
            .expect("value")
            .to_rgba8()
            .get_pixel(60, 60)
            .0;
        assert_eq!(pixel, [200, 100, 50, 255], "resized from the user icon");
    }

    #[test]
    fn refuses_assets_car_packed_icons_and_undeclared_icons() {
        let temporary = tempfile::tempdir().expect("tempdir");

        let asset_app = temporary.path().join("Asset.app");
        app_with(&asset_app, primary_icon(&["AppIcon"]), &[]);
        fs::write(asset_app.join("Assets.car"), b"catalog").expect("assets");

        let mut archive =
            crate::BundleArchive::unpack(&asset_app, crate::ArchiveLimits::default(), Control::default()).expect("a");
        let error =
            archive.replace_icon(&solid_png(120, [1, 2, 3, 255]), Control::default()).expect_err("expected error");
        assert!(matches!(error, Error::Bundle(message) if message.contains("Assets.car")));

        let bare_app = temporary.path().join("Bare.app");
        app_with(&bare_app, Value::Dictionary(Dictionary::new()), &[]);
        let mut bare =
            crate::BundleArchive::unpack(&bare_app, crate::ArchiveLimits::default(), Control::default()).expect("b");
        assert!(bare.replace_icon(&solid_png(120, [1, 2, 3, 255]), Control::default()).is_err());
    }
}
