use crate::error::io;
use crate::{Error, Result};
use plist::{Dictionary, Value};
use std::{
    fs::File,
    io::{Cursor, Read},
    path::Path,
};

const MAX_PLIST_BYTES: u64 = 16 * 1024 * 1024;
const MAX_DEPTH: usize = 64;

/// Binary/XML plists and OpenStep dictionaries, including localized .strings.
pub fn read_dictionary(path: &Path) -> Result<Dictionary> {
    let mut bytes = Vec::new();
    let file = io(path, File::open(path))?;
    io(path, file.take(MAX_PLIST_BYTES + 1).read_to_end(&mut bytes))?;

    parse_dictionary(&bytes)
}

/// Decode an already bounded property-list payload without creating a temporary file.
pub fn parse_dictionary(bytes: &[u8]) -> Result<Dictionary> {
    if bytes.len() as u64 > MAX_PLIST_BYTES {
        return Err(Error::Limit("plist bytes"));
    }

    if let Ok(value) = Value::from_reader(Cursor::new(bytes)) {
        return dictionary(value);
    }

    if bytes.starts_with(b"bplist") {
        return dictionary(Value::from_reader(Cursor::new(bytes))?);
    }

    let mut text = decode_text(bytes)?;

    if text.trim_start().starts_with("<?xml") || text.trim_start().starts_with("<plist") {
        if let Some(end) = text.find("?>") {
            let declaration = text[..end]
                .replace("UTF-16", "UTF-8")
                .replace("utf-16", "UTF-8")
                .replace("UTF-32", "UTF-8")
                .replace("utf-32", "UTF-8");

            text.replace_range(..end, &declaration);
        }

        return dictionary(Value::from_reader(Cursor::new(text.as_bytes()))?);
    }

    let mut parser = Parser { text: &text, position: 0 };
    parser.space()?;
    let braced = parser.remaining().starts_with('{');
    let fields = parser.fields(0, braced)?;

    parser.space()?;

    if !parser.remaining().is_empty() {
        return Err(parser.error("trailing property-list text"));
    }

    Ok(fields)
}

fn dictionary(value: Value) -> Result<Dictionary> {
    match value {
        Value::Dictionary(fields) => Ok(fields),
        _ => Err(Error::Bundle("property list must be a dictionary".into())),
    }
}

fn decode_text(bytes: &[u8]) -> Result<String> {
    if bytes.starts_with(&[0xff, 0xfe, 0, 0]) || bytes.starts_with(&[0, 0, 0xfe, 0xff]) {
        let little = bytes[0] == 0xff;
        let mut text = String::new();

        if !(bytes.len() - 4).is_multiple_of(4) {
            return Err(Error::Bundle("truncated UTF-32 property list".into()));
        }

        for unit in bytes[4..].as_chunks::<4>().0 {
            let word = [unit[0], unit[1], unit[2], unit[3]];
            let value = if little { u32::from_le_bytes(word) } else { u32::from_be_bytes(word) };
            let character = char::from_u32(value).ok_or_else(|| Error::Bundle("invalid UTF-32 scalar".into()))?;
            text.push(character);
        }

        return Ok(text);
    }

    if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        let little = bytes[0] == 0xff;

        if !(bytes.len() - 2).is_multiple_of(2) {
            return Err(Error::Bundle("truncated UTF-16 property list".into()));
        }

        let words = bytes[2..].as_chunks::<2>().0.iter().map(|unit| {
            if little { u16::from_le_bytes([unit[0], unit[1]]) } else { u16::from_be_bytes([unit[0], unit[1]]) }
        });

        return char::decode_utf16(words)
            .collect::<std::result::Result<String, _>>()
            .map_err(|error| Error::Bundle(format!("invalid UTF-16 property list: {error}")));
    }

    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);

    String::from_utf8(bytes.to_vec()).map_err(|error| Error::Bundle(format!("invalid property-list text: {error}")))
}

#[derive(Debug)]
struct Parser<'a> {
    text: &'a str,
    position: usize,
}

impl Parser<'_> {
    fn remaining(&self) -> &str {
        &self.text[self.position..]
    }

    fn error(&self, message: &str) -> Error {
        Error::Bundle(format!("{message} at property-list byte {}", self.position))
    }

    fn next(&mut self) -> Option<char> {
        let character = self.remaining().chars().next()?;
        self.position += character.len_utf8();

        Some(character)
    }

    fn space(&mut self) -> Result<()> {
        loop {
            while self.remaining().chars().next().is_some_and(char::is_whitespace) {
                self.next();
            }

            if self.remaining().starts_with("//") || self.remaining().starts_with('#') {
                self.position += self.remaining().find('\n').unwrap_or(self.remaining().len());

                continue;
            }

            if self.remaining().starts_with("/*") {
                let end = self.remaining()[2..].find("*/").ok_or_else(|| self.error("unterminated comment"))?;
                self.position += end + 4;

                continue;
            }

            return Ok(());
        }
    }

    fn consume(&mut self, token: char) -> Result<bool> {
        self.space()?;

        if self.remaining().starts_with(token) {
            self.next();

            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn expect(&mut self, token: char) -> Result<()> {
        if !self.consume(token)? {
            return Err(self.error(&format!("expected '{token}'")));
        }

        Ok(())
    }

    fn fields(&mut self, depth: usize, braced: bool) -> Result<Dictionary> {
        if depth > MAX_DEPTH {
            return Err(self.error("property-list nesting exceeds 64 levels"));
        }

        if braced {
            self.expect('{')?;
        }

        let mut fields = Dictionary::new();

        loop {
            self.space()?;

            if braced && self.consume('}')? {
                break;
            }

            if !braced && self.remaining().is_empty() {
                break;
            }

            let key = self.string()?;
            self.expect('=')?;

            let value = self.value(depth + 1)?;
            self.expect(';')?;

            if fields.insert(key, value).is_some() {
                return Err(self.error("duplicate dictionary key"));
            }
        }

        Ok(fields)
    }

    fn value(&mut self, depth: usize) -> Result<Value> {
        if depth > MAX_DEPTH {
            return Err(self.error("property-list nesting exceeds 64 levels"));
        }

        self.space()?;

        match self.remaining().chars().next() {
            Some('{') => Ok(Value::Dictionary(self.fields(depth, true)?)),
            Some('(') => {
                self.expect('(')?;
                let mut values = Vec::new();

                while !self.consume(')')? {
                    values.push(self.value(depth + 1)?);

                    if self.consume(')')? {
                        break;
                    }

                    self.expect(',')?;
                }

                Ok(Value::Array(values))
            }

            Some('<') => {
                self.expect('<')?;
                let mut digits = String::new();

                loop {
                    match self.next() {
                        Some('>') => break,
                        Some(character) if character.is_ascii_hexdigit() => digits.push(character),
                        Some(character) if character.is_whitespace() => {}
                        _ => return Err(self.error("invalid hexadecimal data")),
                    }
                }

                if !digits.len().is_multiple_of(2) {
                    return Err(self.error("odd hexadecimal data length"));
                }

                let mut bytes = Vec::with_capacity(digits.len() / 2);

                for pair in digits.as_bytes().as_chunks::<2>().0 {
                    let high = (pair[0] as char).to_digit(16).ok_or_else(|| self.error("hex digit"))?;
                    let low = (pair[1] as char).to_digit(16).ok_or_else(|| self.error("hex digit"))?;
                    bytes.push((high * 16 + low) as u8);
                }

                Ok(Value::Data(bytes))
            }

            Some(_) => Ok(Value::String(self.string()?)),
            None => Err(self.error("missing value")),
        }
    }

    fn string(&mut self) -> Result<String> {
        self.space()?;

        if !self.consume('"')? {
            let start = self.position;

            while self.remaining().chars().next().is_some_and(|character| {
                !character.is_whitespace() && !matches!(character, '=' | ';' | ',' | ')' | '}' | '(' | '{')
            }) {
                self.next();
            }

            if start == self.position {
                return Err(self.error("missing string"));
            }

            return Ok(self.text[start..self.position].to_owned());
        }

        let mut text = String::new();

        loop {
            match self.next() {
                Some('"') => break,
                Some('\\') => match self.next() {
                    Some('n') => text.push('\n'),
                    Some('r') => text.push('\r'),
                    Some('t') => text.push('\t'),
                    Some('U' | 'u') => {
                        let high = self.hex_word()?;
                        let mut words = vec![high];

                        if (0xd800..=0xdbff).contains(&high) {
                            if self.next() != Some('\\') || !matches!(self.next(), Some('U' | 'u')) {
                                return Err(self.error("missing low surrogate"));
                            }

                            words.push(self.hex_word()?);
                        }

                        let decoded = char::decode_utf16(words)
                            .collect::<std::result::Result<String, _>>()
                            .map_err(|_| self.error("invalid Unicode escape"))?;
                        text.push_str(&decoded);
                    }

                    Some(first @ '0'..='7') => {
                        let mut value = first as u32 - '0' as u32;

                        for _ in 0..2 {
                            let Some(next @ '0'..='7') = self.remaining().chars().next() else {
                                break;
                            };

                            self.next();
                            value = value * 8 + next as u32 - '0' as u32;
                        }

                        text.push(char::from_u32(value).ok_or_else(|| self.error("invalid octal escape"))?);
                    }

                    Some(character) => text.push(character),
                    None => return Err(self.error("truncated escape")),
                },

                Some(character) => text.push(character),
                None => return Err(self.error("unterminated string")),
            }
        }

        Ok(text)
    }

    fn hex_word(&mut self) -> Result<u16> {
        let mut value = 0;

        for _ in 0..4 {
            let digit = self
                .next()
                .and_then(|character| character.to_digit(16))
                .ok_or_else(|| self.error("invalid Unicode escape"))?;
            value = value * 16 + digit as u16;
        }

        Ok(value)
    }
}
