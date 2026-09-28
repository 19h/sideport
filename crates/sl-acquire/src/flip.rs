//! XOR-0xAA "flipped" files (recovered `ipa.flippedFile`/`flippedReader`).
//!
//! The recovered client stores downloaded and temporary IPAs with every byte XORed with 0xAA;
//! `sl-bundle` reads such archives directly.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};

pub const KEY: u8 = 0xAA;

pub fn xor(bytes: &mut [u8]) {
    bytes.iter_mut().for_each(|byte| *byte ^= KEY);
}

/// A file that stores bytes flipped when `flipped` is set; reads and writes see plain bytes.
#[derive(Debug)]
pub struct FlippedFile {
    file: File,
    flipped: bool,
    scratch: Vec<u8>,
}

impl FlippedFile {
    pub fn new(file: File, flipped: bool) -> Self {
        Self { file, flipped, scratch: Vec::new() }
    }

    pub fn len(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    pub fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }

    pub fn truncate(&mut self) -> io::Result<()> {
        self.file.set_len(0)?;
        self.file.rewind()
    }

    pub fn seek_end(&mut self) -> io::Result<u64> {
        self.file.seek(SeekFrom::End(0))
    }
}

impl Read for FlippedFile {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.file.read(buffer)?;

        if self.flipped {
            xor(&mut buffer[..read]);
        }

        Ok(read)
    }
}

impl Write for FlippedFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if !self.flipped {
            return self.file.write(bytes);
        }

        self.scratch.clear();
        self.scratch.extend_from_slice(bytes);
        xor(&mut self.scratch);

        self.file.write_all(&self.scratch)?;

        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Seek for FlippedFile {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.file.seek(position)
    }
}
