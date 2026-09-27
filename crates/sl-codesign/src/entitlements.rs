//! Entitlements encodings.

use crate::{Error, Result};

const MAX_NESTING: usize = 64;

const BOOLEAN: u8 = 0x01;
const INTEGER: u8 = 0x02;
const OCTET_STRING: u8 = 0x04;
const UTF8_STRING: u8 = 0x0c;
const SEQUENCE: u8 = 0x30;
const DICTIONARY_WRAPPER: u8 = 0x70;
const DICTIONARY: u8 = 0xb0;

/// XML plist encoding used for the 0xfade7171 blob.
pub fn to_xml(entitlements: &plist::Dictionary) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    plist::Value::Dictionary(entitlements.clone()).to_writer_xml(&mut output)?;

    Ok(output)
}

/// DER encoding used for the 0xfade7172 blob.
pub fn to_der(entitlements: &plist::Dictionary) -> Result<Vec<u8>> {
    encode_dict(entitlements, 0)
}

// CoreEntitlements v1: [APPLICATION 16] { INTEGER 1, [16] dictionary }.
// Entries are SEQUENCE { UTF8String key, ANY value }, ordered by UTF-8 key bytes.
// Bounds on recursion make malformed/deep plist input fail before exhausting the stack.
fn encode_dict(dict: &plist::Dictionary, depth: usize) -> Result<Vec<u8>> {
    if depth > MAX_NESTING {
        return Err(Error::Entitlements("nesting exceeds 64 levels".into()));
    }

    let mut entries: Vec<_> = dict.iter().collect();
    entries.sort_unstable_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));

    let mut contents = Vec::new();

    for (key, value) in entries {
        let mut pair = tlv(UTF8_STRING, key.as_bytes());
        pair.extend(encode_value(value, depth + 1)?);

        contents.extend(tlv(SEQUENCE, &pair));
    }

    let mut wrapper = vec![INTEGER, 0x01, 0x01];
    wrapper.extend(tlv(DICTIONARY, &contents));

    Ok(tlv(DICTIONARY_WRAPPER, &wrapper))
}

fn encode_value(value: &plist::Value, depth: usize) -> Result<Vec<u8>> {
    if depth > MAX_NESTING {
        return Err(Error::Entitlements("nesting exceeds 64 levels".into()));
    }

    match value {
        plist::Value::Dictionary(dict) => encode_dict(dict, depth),
        plist::Value::Array(values) => encode_array(values, depth),
        plist::Value::Integer(integer) => encode_integer(integer),

        plist::Value::String(text) => Ok(tlv(UTF8_STRING, text.as_bytes())),
        plist::Value::Data(data) => Ok(tlv(OCTET_STRING, data)),
        plist::Value::Boolean(value) => Ok(vec![BOOLEAN, 0x01, if *value { 0xff } else { 0 }]),

        _ => Err(Error::Entitlements("DER entitlements cannot contain real, date, or UID values".into())),
    }
}

fn encode_array(values: &[plist::Value], depth: usize) -> Result<Vec<u8>> {
    let mut contents = Vec::new();

    for value in values {
        contents.extend(encode_value(value, depth + 1)?);
    }

    Ok(tlv(SEQUENCE, &contents))
}

fn encode_integer(integer: &plist::Integer) -> Result<Vec<u8>> {
    let value = if let Some(signed) = integer.as_signed() {
        i128::from(signed)
    } else {
        let unsigned = integer.as_unsigned().ok_or_else(|| Error::Entitlements("integer out of range".into()))?;

        i128::from(unsigned)
    };

    let bytes = value.to_be_bytes();
    let mut start = 0;

    while start < bytes.len() - 1
        && ((bytes[start] == 0 && bytes[start + 1] & 0x80 == 0)
            || (bytes[start] == 0xff && bytes[start + 1] & 0x80 != 0))
    {
        start += 1;
    }

    Ok(tlv(INTEGER, &bytes[start..]))
}

pub(crate) fn tlv(tag: u8, contents: &[u8]) -> Vec<u8> {
    let mut output = vec![tag];

    if contents.len() < 128 {
        output.push(contents.len() as u8);
    } else {
        let bytes = contents.len().to_be_bytes();
        let first = bytes.iter().position(|&byte| byte != 0).unwrap_or(bytes.len() - 1);

        output.push(0x80 | (bytes.len() - first) as u8);
        output.extend_from_slice(&bytes[first..]);
    }

    output.extend_from_slice(contents);

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hand_computed_v1_dictionary() {
        let mut dict = plist::Dictionary::new();
        dict.insert("a".into(), true.into());

        assert_eq!(to_der(&dict).expect("DER"), [0x70, 0x0d, 2, 1, 1, 0xb0, 8, 0x30, 6, 0x0c, 1, b'a', 1, 1, 0xff]);
    }

    #[test]
    fn stable_order_and_integer_boundaries() {
        let mut first = plist::Dictionary::new();
        first.insert("z".into(), 128i64.into());
        first.insert("a".into(), (-129i64).into());

        let mut reordered = plist::Dictionary::new();
        reordered.insert("a".into(), (-129i64).into());
        reordered.insert("z".into(), 128i64.into());

        let encoded = to_der(&first).expect("DER");

        assert_eq!(encoded, to_der(&reordered).expect("DER"));
        assert!(encoded.windows(4).any(|bytes| bytes == [2, 2, 0, 0x80]));
        assert!(encoded.windows(4).any(|bytes| bytes == [2, 2, 0xff, 0x7f]));
    }

    #[test]
    fn rejects_unsupported_and_excessive_nesting() {
        let mut dict = plist::Dictionary::new();
        dict.insert("float".into(), 1.0.into());

        assert!(to_der(&dict).is_err());

        let mut value = plist::Value::Boolean(true);

        for _ in 0..70 {
            value = plist::Value::Array(vec![value]);
        }

        dict.clear();
        dict.insert("deep".into(), value);

        assert!(to_der(&dict).is_err());
    }
}
