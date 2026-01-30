#![no_main]
use libfuzzer_sys::fuzz_target;

use image::codecs::bmp::BmpDecoder;
use image::{ImageDecoder, ImageError, Limits};
use std::cell::Cell;
use std::io::{BufRead, Read, Seek, SeekFrom};

/// A reader that simulates partial data availability for testing resumable decoding.
/// It wraps a byte slice and limits how many bytes can be read before returning UnexpectedEof.
struct PartialReader<'a> {
    data: &'a [u8],
    position: u64,
    available_bytes: &'a Cell<usize>,
}

impl<'a> PartialReader<'a> {
    fn new(data: &'a [u8], available_bytes: &'a Cell<usize>) -> Self {
        Self {
            data,
            position: 0,
            available_bytes,
        }
    }
}

impl<'a> Read for PartialReader<'a> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let data_view = &self.data[..self.available_bytes.get()];

        if self.position as usize >= data_view.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "simulated partial data",
            ));
        }

        let available = data_view.len() - self.position as usize;
        let to_read = buf.len().min(available);
        let start = self.position as usize;
        buf[..to_read].copy_from_slice(&data_view[start..start + to_read]);
        self.position += to_read as u64;
        Ok(to_read)
    }
}

impl<'a> BufRead for PartialReader<'a> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        let data_view = &self.data[..self.available_bytes.get()];

        if self.position as usize >= data_view.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "simulated partial data",
            ));
        }

        let start = self.position as usize;
        Ok(&data_view[start..])
    }

    fn consume(&mut self, amt: usize) {
        self.position += amt as u64;
    }
}

impl<'a> Seek for PartialReader<'a> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let Some(new_pos) = (match pos {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::End(offset) => (self.data.len() as u64).checked_add_signed(offset),
            SeekFrom::Current(offset) => self.position.checked_add_signed(offset),
        }) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek to invalid position",
            ));
        };
        self.position = new_pos;
        Ok(self.position)
    }
}

// Helper to check if an error is UnexpectedEof
fn is_unexpected_eof(err: &ImageError) -> bool {
    matches!(err, ImageError::IoError(e) if e.kind() == std::io::ErrorKind::UnexpectedEof)
}

// Test resumable decoding by simulating a slowly arriving stream of data
fuzz_target!(|data: &[u8]| {
    let available_bytes = Cell::new(0);
    let reader = PartialReader::new(data, &available_bytes);
    let mut decoder = BmpDecoder::new_resumable(reader);

    // Phase 1: Stream bytes until metadata succeeds
    loop {
        match decoder.read_metadata() {
            Ok(()) => break, // Done!
            Err(e) => {
                if !is_unexpected_eof(&e) {
                    return;
                }
            }
        }

        if available_bytes.get() >= data.len() {
            // End of `data`
            return;
        }
        available_bytes.set(available_bytes.get() + 1);
    }

    if decoder.total_bytes() > Limits::default().max_alloc.unwrap_or(u64::MAX) {
        return;
    }
    let mut buf = vec![0u8; decoder.total_bytes().try_into().unwrap()];

    // Phase 2: Stream bytes until image data succeeds
    loop {
        match decoder.read_image_data(&mut buf) {
            Ok(()) => break, // Done!
            Err(e) => {
                if !is_unexpected_eof(&e) {
                    return;
                }
            }
        }

        if available_bytes.get() >= data.len() {
            // End of `data`
            return;
        }
        available_bytes.set(available_bytes.get() + 1);
    }
});
