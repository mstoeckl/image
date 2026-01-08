#![no_main]
#[macro_use]
extern crate libfuzzer_sys;
extern crate image;

use image::codecs::bmp::BmpDecoder;
use image::error::{ImageError, LimitError, LimitErrorKind};
use image::ImageDecoder;
use image::ImageResult;
use std::io::Cursor;
use std::io::{BufRead, Read, Seek, SeekFrom};

fn decode_non_streaming<R: BufRead + Seek>(reader: &mut R) -> ImageResult<Vec<u8>> {
    let decoder = BmpDecoder::new(reader)?;
    if decoder.total_bytes() > 100_000_000 {
        return Err(ImageError::Limits(LimitError::from_kind(
            LimitErrorKind::InsufficientMemory,
        )));
    }
    // TODO important: this should also return all the metadata
    // (ICC profile, image dimensions, color type); the raw bytes aren't everything.
    let mut buf = vec![0_u8; decoder.total_bytes() as usize];
    decoder.read_image(&mut buf)?;
    Ok(buf)
}

fn decode_streaming<R: BufRead + Seek>(reader: &mut R) -> ImageResult<Vec<u8>> {
    let mut decoder = BmpDecoder::new_streaming(reader)?;
    decoder.try_read_metadata()?;
    if decoder.total_bytes() > 100_000_000 {
        return Err(ImageError::Limits(LimitError::from_kind(
            LimitErrorKind::InsufficientMemory,
        )));
    }
    let mut buf = vec![0_u8; decoder.total_bytes() as usize];
    decoder.read_image(&mut buf)?;
    Ok(buf)
}

// A modified Cursor which errors when seeking out of bounds,
// instead of -- as the Seek trait permits -- silently accepting.
struct StrictCursor<'a>(Cursor<&'a [u8]>);

impl<'a> Read for StrictCursor<'a> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}
impl<'a> BufRead for StrictCursor<'a> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        self.0.fill_buf()
    }
    fn consume(&mut self, amount: usize) {
        self.0.consume(amount)
    }
}
impl<'a> Seek for StrictCursor<'a> {
    fn seek(&mut self, style: SeekFrom) -> std::io::Result<u64> {
        self.0.seek(style)?;
        if self.0.position() > self.0.get_ref().len() as u64 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                *Box::new(""),
            ));
        }
        Ok(self.0.position())
    }
}

fuzz_target!(|data: &[u8]| {
    let mut c1 = StrictCursor(Cursor::new(data));
    let mut c2 = StrictCursor(Cursor::new(data));

    let r1 = decode_streaming(&mut c1);
    let r2 = decode_non_streaming(&mut c2);
    match (&r1, &r2) {
        (Ok(im1), Ok(im2)) => {
            assert!(im1 == im2);
        }
        (Err(_), Err(_)) => (),
        _ => {
            panic!(
                "Positions S {:?} N {:?} Results S {:?} N {:?}",
                c1.0.position(),
                c2.0.position(),
                r1,
                r2
            );
        }
    }
});
