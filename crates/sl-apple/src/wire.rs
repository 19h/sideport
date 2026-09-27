//! Bounded XML property lists with redacted, zeroizing ownership.

use crate::{Error, Result};
use plist::{Dictionary, Value};
use quick_xml::events::Event;
use std::fmt;
use std::io::Cursor;
use zeroize::{Zeroize, Zeroizing};

pub(crate) const MAX_BODY_BYTES: usize = 1024 * 1024;
const APPLE_DTD: &[u8] =
    b"plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\"";

pub(crate) struct SecretValue(pub(crate) Value);

impl fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretValue([redacted])")
    }
}

impl Drop for SecretValue {
    fn drop(&mut self) {
        wipe(&mut self.0);
    }
}

pub(crate) fn wipe(value: &mut Value) {
    match value {
        Value::String(value) => value.zeroize(),
        Value::Data(value) => value.zeroize(),
        Value::Array(values) => values.iter_mut().for_each(wipe),
        Value::Dictionary(values) => values.iter_mut().for_each(|(_, value)| wipe(value)),
        _ => {}
    }
}

pub(crate) fn dictionary(fields: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Dictionary(fields.into_iter().map(|(key, value)| (key.to_owned(), value)).collect())
}

pub(crate) fn encode(value: &Value) -> Result<Zeroizing<Vec<u8>>> {
    let mut bytes = Zeroizing::new(Vec::new());
    value.to_writer_xml(&mut *bytes).map_err(|_| Error::Invalid("request plist"))?;

    if bytes.len() > MAX_BODY_BYTES {
        return Err(Error::Invalid("request plist length"));
    }

    Ok(bytes)
}

pub(crate) fn decode(bytes: &[u8], fragment: bool) -> Result<SecretValue> {
    if bytes.len() > MAX_BODY_BYTES {
        return Err(Error::Invalid("response plist length"));
    }

    let mut wrapped = Zeroizing::new(Vec::new());
    let xml = if fragment {
        wrapped.extend_from_slice(b"<plist>");
        wrapped.extend_from_slice(bytes);
        wrapped.extend_from_slice(b"</plist>");

        wrapped.as_slice()
    } else {
        bytes
    };

    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut depth = 0_usize;

    loop {
        match reader.read_event().map_err(|_| Error::Invalid("response XML"))? {
            Event::Start(_) => {
                depth += 1;

                if depth > 32 {
                    return Err(Error::Invalid("response XML nesting"));
                }
            }
            Event::End(_) => depth = depth.checked_sub(1).ok_or(Error::Invalid("response XML nesting"))?,
            Event::DocType(declaration) if fragment || declaration.as_ref() != APPLE_DTD => {
                return Err(Error::Invalid("response XML declaration"));
            }
            Event::Decl(_) if fragment => return Err(Error::Invalid("response XML declaration")),
            Event::Eof => break,
            _ => {}
        }
    }

    let value = Value::from_reader_xml(Cursor::new(xml)).map_err(|_| Error::Invalid("response plist"))?;

    Ok(SecretValue(value))
}

pub(crate) fn dict(value: &Value) -> Result<&Dictionary> {
    value.as_dictionary().ok_or(Error::Invalid("response dictionary"))
}

pub(crate) fn string<'a>(dictionary: &'a Dictionary, key: &'static str) -> Result<&'a str> {
    dictionary
        .get(key)
        .and_then(Value::as_string)
        .filter(|value| !value.is_empty() && !value.contains('\0'))
        .ok_or(Error::Invalid(key))
}

pub(crate) fn data<'a>(dictionary: &'a Dictionary, key: &'static str) -> Result<&'a [u8]> {
    dictionary.get(key).and_then(Value::as_data).ok_or(Error::Invalid(key))
}

pub(crate) fn integer(dictionary: &Dictionary, key: &'static str) -> Result<i64> {
    let value = dictionary.get(key).ok_or(Error::Invalid(key))?;

    value.as_signed_integer().or_else(|| value.as_string()?.parse().ok()).ok_or(Error::Invalid(key))
}
