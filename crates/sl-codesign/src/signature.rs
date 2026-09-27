//! Signature blob construction and Mach-O rewriting.

use crate::{CodeKind, Error, Result, SignOptions, Signer, blob, cms, entitlements, requirements};
use rayon::prelude::*;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use sl_macho::{Binary, Endian, LC_DYLIB_CODE_SIGN_DRS, MachO, align_up, to_u32};
use std::collections::BTreeMap;

const PAGE_SIZE: usize = 4096;
const CD_HEADER: usize = 92;
const CD_VERSION: u32 = 0x20400;

const INFO_SLOT: usize = 1;
const REQUIREMENTS_SLOT: usize = 2;
const RESOURCES_SLOT: usize = 3;
const ENTITLEMENTS_SLOT: usize = 5;
const DER_ENTITLEMENTS_SLOT: usize = 7;

const PRIMARY_DIRECTORY_SLOT: u32 = 0;
const ALTERNATE_DIRECTORY_SLOT: u32 = 0x1000;
const CMS_SIGNATURE_SLOT: u32 = 0x10000;

#[derive(Debug)]
struct PageHash {
    sha1: [u8; 20],
    sha256: [u8; 32],
}

#[derive(Debug)]
struct Components {
    blobs: BTreeMap<u32, Vec<u8>>,
    special_slots: BTreeMap<usize, Vec<u8>>,
}

#[derive(Debug, Clone, Copy)]
enum HashAlgorithm {
    Sha1,
    Sha256,
}

impl HashAlgorithm {
    fn hash_size(self) -> usize {
        match self {
            Self::Sha1 => 20,
            Self::Sha256 => 32,
        }
    }

    fn hash_type(self) -> u8 {
        match self {
            Self::Sha1 => 1,
            Self::Sha256 => 2,
        }
    }

    fn digest(self, bytes: &[u8]) -> Vec<u8> {
        match self {
            Self::Sha1 => Sha1::digest(bytes).to_vec(),
            Self::Sha256 => Sha256::digest(bytes).to_vec(),
        }
    }

    fn page_digest(self, hashes: &PageHash) -> &[u8] {
        match self {
            Self::Sha1 => &hashes.sha1,
            Self::Sha256 => &hashes.sha256,
        }
    }
}

pub(crate) fn sign_file(input: &[u8], signer: &Signer, opts: &SignOptions<'_>) -> Result<Vec<u8>> {
    if opts.identifier.is_empty() || opts.identifier.as_bytes().contains(&0) {
        return Err(Error::Other("code identifier is empty or contains NUL".into()));
    }

    let binary = Binary::parse(input)?;
    let slices =
        binary.slices.par_iter().map(|slice| sign_slice(&slice.image, signer, opts)).collect::<Result<Vec<_>>>()?;

    Ok(binary.rebuild(&slices, 14)?)
}

pub(crate) fn strip_file(input: &[u8]) -> Result<Vec<u8>> {
    let binary = Binary::parse(input)?;

    let slices = binary
        .slices
        .iter()
        .map(|slice| {
            if slice.image.signature.is_none() {
                Ok(slice.image.bytes.to_vec())
            } else {
                Ok(slice.image.prepare_signature(0, 0)?)
            }
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(binary.rebuild(&slices, 14)?)
}

pub(crate) fn is_arm64_macho(data: &[u8]) -> bool {
    Binary::parse(data).is_ok_and(|binary| binary.slices.iter().any(|slice| slice.cpu_type == sl_macho::CPU_TYPE_ARM64))
}

pub(crate) fn is_encrypted(data: &[u8]) -> Result<bool> {
    let binary = Binary::parse(data)?;

    Ok(binary.slices.iter().any(|slice| slice.image.encrypted))
}

fn components(image: &MachO<'_>, signer: &Signer, opts: &SignOptions<'_>) -> Result<Components> {
    let library_drs = image
        .commands
        .iter()
        .find(|command| command.kind == LC_DYLIB_CODE_SIGN_DRS)
        .map(|command| -> Result<&[u8]> {
            let start = image.endian.u32(command.bytes, 8)? as usize;
            let size = image.endian.u32(command.bytes, 12)? as usize;

            Ok(&image.bytes[start..start + size])
        })
        .transpose()?;

    let requirements = requirements::build(opts.identifier, signer, library_drs)?;
    let mut special_slots = BTreeMap::from([(REQUIREMENTS_SLOT, requirements.clone())]);
    let mut blobs = BTreeMap::from([(REQUIREMENTS_SLOT as u32, requirements)]);

    let embedded_info = image
        .segments
        .iter()
        .flat_map(|segment| &segment.sections)
        .find(|section| section.segment_name == "__TEXT" && section.name == "__info_plist" && !section.is_zero_fill())
        .map(|section| &image.bytes[section.offset as usize..section.offset as usize + section.size as usize]);

    if let Some(info) = opts.info_plist.or(embedded_info) {
        special_slots.insert(INFO_SLOT, info.to_vec());
    }

    if let Some(resources) = opts.code_resources {
        special_slots.insert(RESOURCES_SLOT, resources.to_vec());
    }

    if !signer.is_adhoc()
        && opts.kind != CodeKind::Dylib
        && let Some(ent) = opts.entitlements
    {
        let xml = blob::wrap(blob::ENTITLEMENTS, &entitlements::to_xml(ent)?)?;
        let der = blob::wrap(blob::DER_ENTITLEMENTS, &entitlements::to_der(ent)?)?;

        special_slots.insert(ENTITLEMENTS_SLOT, xml.clone());
        special_slots.insert(DER_ENTITLEMENTS_SLOT, der.clone());

        blobs.insert(ENTITLEMENTS_SLOT as u32, xml);
        blobs.insert(DER_ENTITLEMENTS_SLOT as u32, der);
    }

    Ok(Components { blobs, special_slots })
}

fn sign_slice(image: &MachO<'_>, signer: &Signer, opts: &SignOptions<'_>) -> Result<Vec<u8>> {
    let offset = align_up(image.unsigned_len()?, 16)?;
    to_u32(offset, "code limit")?;

    let components = components(image, signer, opts)?;

    // Fixed hash-array and RSA sizes let the placeholder determine the signature size
    // without hashing or signing the file twice.
    let reserved = {
        let primary = directory(image, signer, opts, offset, &components, None, HashAlgorithm::Sha1)?;
        let alternate = directory(image, signer, opts, offset, &components, None, HashAlgorithm::Sha256)?;
        let mut placeholders = components.blobs.clone();

        if let Signer::Identity(identity) = signer {
            let cms_length = cms::encoded_size(&primary, &alternate, identity)?;
            let placeholder_cms = blob::wrap(blob::BLOB_WRAPPER, &vec![0; cms_length])?;

            placeholders.insert(CMS_SIGNATURE_SLOT, placeholder_cms);
        }

        placeholders.insert(PRIMARY_DIRECTORY_SLOT, primary);
        placeholders.insert(ALTERNATE_DIRECTORY_SLOT, alternate);

        let signature = blob::superblob(blob::EMBEDDED_SIGNATURE, &placeholders)?;

        align_up(signature.len(), 16)?
    };

    let mut output = image.prepare_signature(offset, reserved)?;

    let hashes: Vec<_> = output[..offset]
        .par_chunks(PAGE_SIZE)
        .map(|page| PageHash { sha1: Sha1::digest(page).into(), sha256: Sha256::digest(page).into() })
        .collect();

    let primary = directory(image, signer, opts, offset, &components, Some(&hashes), HashAlgorithm::Sha1)?;
    let alternate = directory(image, signer, opts, offset, &components, Some(&hashes), HashAlgorithm::Sha256)?;

    let mut blobs = components.blobs;

    if let Signer::Identity(identity) = signer {
        let signed_cms = cms::sign(&primary, &alternate, identity)?;

        blobs.insert(CMS_SIGNATURE_SLOT, blob::wrap(blob::BLOB_WRAPPER, &signed_cms)?);
    }

    blobs.insert(PRIMARY_DIRECTORY_SLOT, primary);
    blobs.insert(ALTERNATE_DIRECTORY_SLOT, alternate);

    let signature = blob::superblob(blob::EMBEDDED_SIGNATURE, &blobs)?;

    if signature.len() > reserved {
        return Err(Error::Other("CMS size changed after sizing".into()));
    }

    output[offset..offset + signature.len()].copy_from_slice(&signature);

    Ok(output)
}

fn directory(
    image: &MachO<'_>,
    signer: &Signer,
    opts: &SignOptions<'_>,
    code_limit: usize,
    components: &Components,
    pages: Option<&[PageHash]>,
    algorithm: HashAlgorithm,
) -> Result<Vec<u8>> {
    let hash_size = algorithm.hash_size();
    let special_count = components.special_slots.keys().next_back().copied().unwrap_or(0);
    let page_count = code_limit.div_ceil(PAGE_SIZE);

    let mut output = vec![0; CD_HEADER];
    output.extend_from_slice(opts.identifier.as_bytes());
    output.push(0);

    let team_offset = if signer.team_id().is_empty() {
        0
    } else {
        let offset = output.len();

        output.extend_from_slice(signer.team_id().as_bytes());
        output.push(0);

        offset
    };

    let hash_offset = output
        .len()
        .checked_add(special_count * hash_size)
        .ok_or_else(|| Error::Other("hash offset overflow".into()))?;
    let length = hash_offset
        .checked_add(page_count * hash_size)
        .ok_or_else(|| Error::Other("CodeDirectory size overflow".into()))?;

    output.resize(length, 0);

    let fields = [
        (0, blob::CODE_DIRECTORY),
        (4, to_u32(length, "CodeDirectory length")?),
        (8, CD_VERSION),
        (12, if signer.is_adhoc() { 2 } else { 0 }),
        (16, to_u32(hash_offset, "hash offset")?),
        (20, CD_HEADER as u32),
        (24, to_u32(special_count, "special slot count")?),
        (28, to_u32(page_count, "page count")?),
        (32, to_u32(code_limit, "code limit")?),
        (48, to_u32(team_offset, "team offset")?),
    ];

    for (offset, value) in fields {
        Endian::Big.put_u32(&mut output, offset, value)?;
    }

    output[36] = hash_size as u8;
    output[37] = algorithm.hash_type();
    output[39] = 12;

    if let Some(text) = image.segments.iter().find(|segment| segment.name == "__TEXT") {
        Endian::Big.put_u64(&mut output, 64, text.file_offset)?;
        Endian::Big.put_u64(&mut output, 72, text.file_size)?;
    }

    let main = matches!(opts.kind, CodeKind::MainExecutable | CodeKind::AppExtension);
    let debug = !signer.is_adhoc()
        && main
        && opts.entitlements.is_some_and(|entitlements| {
            entitlements.get("get-task-allow").and_then(plist::Value::as_boolean) == Some(true)
        });

    let exec_flags = u64::from(main) | if debug { 0x10 } else { 0 };

    Endian::Big.put_u64(&mut output, 80, exec_flags)?;

    for (slot, bytes) in &components.special_slots {
        let offset = hash_offset - slot * hash_size;
        let digest = algorithm.digest(bytes);

        output[offset..offset + hash_size].copy_from_slice(&digest);
    }

    if let Some(pages) = pages {
        if pages.len() != page_count {
            return Err(Error::Other("page count changed during signing".into()));
        }

        for (i, hashes) in pages.iter().enumerate() {
            let offset = hash_offset + i * hash_size;

            output[offset..offset + hash_size].copy_from_slice(algorithm.page_digest(hashes));
        }
    }

    Ok(output)
}
