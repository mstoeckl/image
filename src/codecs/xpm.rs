//! Decoding of XPM Images
//!
//! XPM (X PixMap) Format is a plain text image format, sometimes used to store
//! cursor and icon data. XPM images are valid C code.
//!
//! (This format is obsolete and nobody should make new images in it. If you need to
//! include an image in a C program, use `xxd -i` or #embed.)
//!
//! Note: color values for the X11 color name table were _changed_ for the X11R4 release
//! in Dec 1989; since then there have only been additions.
//!
//! This overlaps with XPM version development: XPMv1 in Feb 1989, XPMv2 in Feb-August 1990,
//! and XPMv3 in April 1991. Therefore, if you _do_ see an ancient XPMv1 or XPMv2 file
//! somewhere, it may be using different color name values.
//!
//! This decoder uses the X11 color name table as of X11R6 (May 1994); the only additions since
//! then, in 2014 to add some CSS color names, are _not_ included, to preserve compatibility
//! with other XPM parsers.
//!
//! This is a somewhat strict decoder and will reject many broken image files, including:
//! - those using the XPM2 header or `static char ** name = {` array string
//! - those missing a trailing "," on lines, or which use ";" instead of ","
//! - those with color data lines that are too long
//! - those which have content after the final semicolon which is not a C comment
//!
//! # Related Links
//! * <https://www.x.org/docs/XPM/xpm.pdf> - XPM Manual version 3.4i, which specifies the format
//! * <https://en.wikipedia.org/wiki/X_PixMap> - The XPM format on wikipedia
//! * <https://web.archive.org/web/20110513234507/https://www.w3.org/People/danield/xpm_story.html> - XPM format history
//! * <https://gitlab.freedesktop.org/xorg/app/rgb/raw/master/rgb.txt> - X color names
//! * <https://www.x.org/wiki/X11R4/#index10h4> - Introduction of modern X11 color name table
#![forbid(unsafe_code)]

// Use external crate for the color names, because the color name table is X11 licensed (= MIT with an
// additional condition) and cannot be included as-is without issues. The only change to the X11
// color DB since X11R6 is the addition of some CSS color names in 2014; these additions, like changes
// to XParseColor, should not be taken into account, because older parsers have not and will never be
// updated to include them. XPM is a dead standard for a reason: keep it that way!
extern crate x11r6colornames;

use std::cmp::Ordering;
use std::fmt;
use std::io::{self, BufRead, Bytes};

use crate::color::ColorType;
use crate::error::{DecodingError, ImageError, ImageResult, LimitError, LimitErrorKind};
use crate::image::{ImageDecoder, ImageFormat};
use crate::Limits;

/// Maximum length of an X11/CSS/etc. color name is 20; and of an RGB color is 13
const MAX_COLOR_NAME_LEN: usize = 32;

/// Location of a byte in the input stream.
///
/// Includes byte offset (for format debugging with hex editor) and
/// line:column offset (for format debugging with text editor)
#[derive(Clone, Copy, Debug)]
struct TextLocation {
    byte: u64,
    line: u64,
    column: u64,
}

/// A peekable reader which tracks location information
struct TextReader<R: BufRead> {
    inner: Bytes<R>,

    current: Option<u8>,

    location: TextLocation,
}

impl<R> TextReader<R>
where
    R: BufRead,
{
    /// Initialize a TextReader, returning an error if the first byte could not be acquired
    fn new(r: R) -> Result<TextReader<R>, io::Error> {
        let mut b = r.bytes();
        let current = match b.next() {
            None => None,
            Some(Ok(v)) => Some(v),
            Some(Err(x)) => {
                return Err(x);
            }
        };
        Ok(TextReader {
            inner: b,
            current,
            location: TextLocation {
                byte: 0,
                line: 1,
                column: 0,
            },
        })
    }

    /// Consume the next byte. On EOF, will return None
    fn next(&mut self) -> Result<Option<u8>, io::Error> {
        if self.current.is_none() {
            return Ok(None);
        }

        let mut current = match self.inner.next() {
            None => None,
            Some(Ok(v)) => Some(v),
            Some(Err(x)) => {
                return Err(x);
            }
        };

        std::mem::swap(&mut self.current, &mut current);

        self.location.byte += 1;
        self.location.column += 1;
        if let Some(b'\n') = current {
            self.location.line += 1;
            self.location.column = 0;
        }
        Ok(current)
    }
    /// Peek at the next byte. On EOF, will return None
    fn peek(&self) -> Option<u8> {
        self.current
    }
    /// The location of the last byte returned by [Self::next]
    fn loc(&self) -> TextLocation {
        self.location
    }
}

/// XPM decoder
pub struct XpmDecoder<R: BufRead> {
    r: TextReader<R>,
    width: u32,
    height: u32,
    ncolors: u32,
    /// characters per pixel
    cpp: u32,
}

/// Pixel code and value read from the Colors section of an XPM file
struct XpmColorCodeEntry {
    code: u64,
    /// channel order: R,G,B,A
    value: [u16; 4],
}

#[derive(Debug, Clone, Copy)]
enum XpmPart {
    Header,
    ArrayStart,
    FirstLine,
    Palette,
    Body,
    Trailing,
    AfterEnd,
}

#[derive(Debug)]
enum XpmDecodeError {
    Io(io::Error),
    Parse(XpmPart, TextLocation),
    ZeroColors,
    BadCharsPerColor(u32),
    // A color with the given name is not available. Name length should be at most [MAX_COLOR_NAME_LEN] and be alphanumeric
    UnknownColor(String),
    // Palette entry is missing 'c'-type color specification
    NoColorModeColorSpecified,
    BadHexColor,
    DuplicateCode,
    UnknownCode,
    TwoKeysInARow,
    MissingEntry,
    MissingColorAfterKey,
    MissingKeyBeforeColor,
    InvalidColorName,
    ColorNameTooLong,
}

/// Types of visuals for which a color should be used
#[derive(Debug)]
enum XpmVisual {
    Mono,
    Symbolic,
    Grayscale4,
    Grayscale,
    Color,
}

impl fmt::Display for TextLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "byte={},line={}:col={}",
            self.byte, self.line, self.column
        ))
    }
}

impl fmt::Display for XpmPart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Header => f.write_str("header"),
            Self::ArrayStart => f.write_str("array definition"),
            Self::FirstLine => f.write_str("<Values> section"),
            Self::Palette => f.write_str("<Colors> section"),
            Self::Body => f.write_str("<Pixels> section"),
            Self::Trailing => f.write_str("array end"),
            Self::AfterEnd => f.write_str("after final semicolon"),
        }
    }
}

impl fmt::Display for XpmDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(x) => x.fmt(f),
            Self::Parse(part, loc) => f.write_fmt(format_args!("Failed to parse {}, at {}", part, loc)),
            Self::ZeroColors => f.write_str("Invalid (zero) number of colors"),
            Self::BadCharsPerColor(c) => f.write_fmt(format_args!(
                "Invalid number of characters per color: {} is not in [1,8]",
                c
            )),
            Self::UnknownColor(s) => {
                assert!(s.chars().all(|x| x.is_ascii_alphanumeric()));
                f.write_fmt(format_args!("Unknown color name \"{}\"; is not an X11R6 color.", s))
            }
            Self::NoColorModeColorSpecified => {
                f.write_str("Color entry has no specified value for color visual")
            }
            Self::BadHexColor => f.write_str("Invalid hex RGB color"),
            Self::DuplicateCode => f.write_str("Duplicate color code"),
            Self::UnknownCode => f.write_str("Unknown color code"),

            Self::ColorNameTooLong => f.write_str("Invalid color name, too long"),
            Self::TwoKeysInARow => f.write_str("Invalid color specification, two keys in a row"),
            Self::MissingEntry => f.write_str("Invalid color specification, must contain at least one key-color pair"),
            Self::MissingColorAfterKey => f.write_str("Invalid color specification, no color name after key"),
            Self::MissingKeyBeforeColor => f.write_str("Invalid color specification, no key before color name or could not parse value as key (m|s|g4|g|c)"),
            Self::InvalidColorName => f.write_str("Invalid color name, contains non-alphanumeric or non-whitespace characters"),
        }
    }
}

impl std::error::Error for XpmDecodeError {}

impl From<XpmDecodeError> for ImageError {
    fn from(e: XpmDecodeError) -> ImageError {
        ImageError::Decoding(DecodingError::new(ImageFormat::Xpm.into(), e))
    }
}

impl From<io::Error> for XpmDecodeError {
    fn from(e: io::Error) -> XpmDecodeError {
        XpmDecodeError::Io(e)
    }
}

/// Is x a valid character to use in a word of a color name
fn valid_name_char(x: u8) -> bool {
    matches!(x, b'#' | b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z')
}
/// Replace upper case by lower case ASCII letters
fn fold_to_lower(x: u8) -> u8 {
    match x {
        b'A'..=b'Z' => (x - b'A') + b'a',
        _ => x,
    }
}

/// Read precisely the string `s` from `r`, or error.
fn read_fixed_string<R: BufRead>(
    r: &mut TextReader<R>,
    s: &[u8],
    part: XpmPart,
) -> Result<(), XpmDecodeError> {
    for c in s {
        if let Some(b) = r.next()? {
            if b != *c {
                return Err(XpmDecodeError::Parse(part, r.loc()));
            }
        } else {
            return Err(XpmDecodeError::Parse(part, r.loc()));
        };
    }
    Ok(())
}
// Read a single byte
fn read_byte<R: BufRead>(r: &mut TextReader<R>, part: XpmPart) -> Result<u8, XpmDecodeError> {
    match r.next()? {
        None => Err(XpmDecodeError::Parse(part, r.loc())),
        Some(b) => Ok(b),
    }
}

/// Read a mixture of ' ' and '\t'. At least one character must be read.
// Other whitespace characters are not permitted.
fn read_whitespace_gap<R: BufRead>(
    r: &mut TextReader<R>,
    part: XpmPart,
) -> Result<(), XpmDecodeError> {
    let b = read_byte(r, part)?;
    if !(b == b' ' || b == b'\t') {
        return Err(XpmDecodeError::Parse(part, r.loc()));
    }
    while let Some(b) = r.peek() {
        if b == b' ' || b == b'\t' {
            r.next()?;
            continue;
        } else {
            return Ok(());
        }
    }
    Ok(())
}

/// Read a mixture of ' ', '\t', '\n', and C-style /* comments */.
/// This will error if it sees a / without following *
fn skip_whitespace_and_comments<R: BufRead>(
    r: &mut TextReader<R>,
    part: XpmPart,
) -> Result<usize, XpmDecodeError> {
    let mut nbytes = 0;

    // `has_first_char`: If out of comment, has / ; if in comment, has *
    let mut has_first_char = false;
    let mut in_comment = false;

    while let Some(b) = r.peek() {
        if !in_comment {
            if has_first_char {
                if b != b'*' {
                    return Err(XpmDecodeError::Parse(part, r.loc()));
                } else {
                    in_comment = true;
                    has_first_char = false;
                }
            }
            if b == b'/' {
                has_first_char = true;
            }
        }
        if b == b' ' || b == b'\t' || b == b'\n' || b == b'/' || in_comment {
            if in_comment {
                if has_first_char && b == b'/' {
                    in_comment = false;
                }
                has_first_char = b == b'*';
            }
            nbytes += 1;
            r.next()?;
            continue;
        } else {
            break;
        }
    }
    if !in_comment && has_first_char {
        // Parsed up to a / but did not find *
        return Err(XpmDecodeError::Parse(part, r.loc()));
    }

    Ok(nbytes)
}

fn skip_spaces_and_tabs<R: BufRead>(r: &mut TextReader<R>) -> Result<usize, XpmDecodeError> {
    let mut nbytes = 0;
    while let Some(b) = r.peek() {
        if b == b' ' || b == b'\t' {
            nbytes += 1;
            r.next()?;
            continue;
        } else {
            break;
        }
    }
    Ok(nbytes)
}

/// Read a mixture of ' ' and '\t', until reading '\n'.
fn read_to_newline<R: BufRead>(r: &mut TextReader<R>, part: XpmPart) -> Result<(), XpmDecodeError> {
    while let Some(b) = r.peek() {
        if b == b' ' || b == b'\t' {
            r.next()?;
            continue;
        } else {
            break;
        }
    }
    if read_byte(r, part)? != b'\n' {
        Err(XpmDecodeError::Parse(part, r.loc()))
    } else {
        Ok(())
    }
}
/// Read token into the buffer until the buffer size is exceeded, or ' ' or '\t' or '"' is found
/// \ characters are forbidden. Returns the region of data read.
fn read_until_whitespace_or_eos<'a, R: BufRead>(
    r: &mut TextReader<R>,
    buf: &'a mut [u8],
    part: XpmPart,
) -> Result<&'a mut [u8], XpmDecodeError> {
    let mut len = 0;
    while let Some(b) = r.peek() {
        if b == b' ' || b == b'\t' || b == b'"' {
            return Ok(&mut buf[..len]);
        } else if b == b'\\' {
            r.next()?;
            return Err(XpmDecodeError::Parse(part, r.loc()));
        } else {
            if len >= buf.len() {
                // identifier is too long
                return Err(XpmDecodeError::Parse(part, r.loc()));
            }
            buf[len] = b;
            len += 1;
            r.next()?;
        }
    }
    Ok(&mut buf[..len])
}

/// Read fixed length token into the buffer. Errors if file ends, or " or \ is found.
fn read_all_except_eos<R: BufRead>(
    r: &mut TextReader<R>,
    buf: &mut [u8],
    part: XpmPart,
) -> Result<(), XpmDecodeError> {
    let mut len = 0;
    while let Some(b) = r.peek() {
        if b == b'"' || b == b'\\' {
            r.next()?;
            return Err(XpmDecodeError::Parse(part, r.loc()));
        } else {
            buf[len] = b;
            len += 1;
            r.next()?;
            if len >= buf.len() {
                return Ok(());
            }
        }
    }
    Err(XpmDecodeError::Parse(part, r.loc()))
}

/// Read the name portion of the file (but do not validate it, because some old files
/// may put invalid characters here (like "." and "-") or use 8-bit character sets instead
/// of Unicode.)
fn read_name<R: BufRead>(r: &mut TextReader<R>, part: XpmPart) -> Result<(), XpmDecodeError> {
    let mut empty = true;
    while let Some(b) = r.peek() {
        match b {
            b'/' | b' ' | b'\t' | b'\n' | b'[' => {
                break;
            }
            _ => (),
        }
        r.next()?;
        empty = false;
    }
    if empty {
        return Err(XpmDecodeError::Parse(part, r.loc()));
    }

    Ok(())
}

/// Parse string into integer, rejecting leading + and leading zeros
fn parse_i32(data: &[u8]) -> Option<i32> {
    if data.starts_with(b"-") {
        (-(parse_u32(&data[1..])? as i64)).try_into().ok()
    } else {
        parse_u32(data)?.try_into().ok()
    }
}

/// Parse string into unsigned integer, rejecting leading + and leading zeros
fn parse_u32(data: &[u8]) -> Option<u32> {
    let Some(c1) = data.first() else {
        // Reject empty string
        return None;
    };
    if *c1 == b'0' && data.len() > 1 {
        // Reject leading zeros unless value is exactly zero
        return None;
    }
    let mut x: u32 = 0;
    for c in data {
        if b'0' <= *c && *c <= b'9' {
            x = x.checked_mul(10)?.checked_add((*c - b'0') as u32)?;
        } else {
            return None;
        }
    }
    Some(x)
}
fn parse_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'A'..=b'F' => Some(b - b'A' + 10),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}
fn parse_hex1(x1: u8) -> Option<u16> {
    let x = parse_hex(x1)? as u16;
    Some(x | (x << 4) | (x << 8) | (x << 12))
}
fn parse_hex2(x2: u8, x1: u8) -> Option<u16> {
    let x = ((parse_hex(x2)? as u16) << 4) | (parse_hex(x1)? as u16);
    Some(x | (x << 8))
}
fn parse_hex3(x3: u8, x2: u8, x1: u8) -> Option<u16> {
    let x =
        ((parse_hex(x3)? as u16) << 8) | ((parse_hex(x2)? as u16) << 4) | (parse_hex(x1)? as u16);
    // There are four reasonable approaches to converting 12-bit to 16-bit,
    // round down, round nearest, round up, and round fast
    // (x*65535)/4095, (x*65535+2047)/4095, (x*65535+4094)/4095, and (x<<4)|(x>>8).
    Some((((x as u32) * 65535 + 2047) / 4095) as u16)
}
fn parse_hex4(x4: u8, x3: u8, x2: u8, x1: u8) -> Option<u16> {
    Some(
        (parse_hex(x1)? as u16)
            | ((parse_hex(x2)? as u16) << 4)
            | ((parse_hex(x3)? as u16) << 8)
            | ((parse_hex(x4)? as u16) << 12),
    )
}
fn scale_u8_to_u16(x: u8) -> u16 {
    (x as u16) << 8 | (x as u16)
}

/// Parse an #RGB-style color.
/// Note: this deviates from XParseColor in order to sensibly interpret #aabbcc as #aaaabbbbcccc
/// instead of #aa00bb00cc00.
fn parse_hex_color(data: &[u8]) -> Option<[u16; 4]> {
    Some(match data {
        [r, g, b] => [parse_hex1(*r)?, parse_hex1(*g)?, parse_hex1(*b)?, 0xffff],
        [r2, r1, g2, g1, b2, b1] => [
            parse_hex2(*r2, *r1)?,
            parse_hex2(*g2, *g1)?,
            parse_hex2(*b2, *b1)?,
            0xffff,
        ],
        [r3, r2, r1, g3, g2, g1, b3, b2, b1] => [
            parse_hex3(*r3, *r2, *r1)?,
            parse_hex3(*g3, *g2, *g1)?,
            parse_hex3(*b3, *b2, *b1)?,
            0xffff,
        ],
        [r4, r3, r2, r1, g4, g3, g2, g1, b4, b3, b2, b1] => [
            parse_hex4(*r4, *r3, *r2, *r1)?,
            parse_hex4(*g4, *g3, *g2, *g1)?,
            parse_hex4(*b4, *b3, *b2, *b1)?,
            0xffff,
        ],
        _ => {
            return None;
        }
    })
}

fn parse_color(data: &[u8]) -> Result<[u16; 4], XpmDecodeError> {
    if data.starts_with(b"#") {
        parse_hex_color(&data[1..]).ok_or(XpmDecodeError::BadHexColor)
    } else {
        if data == b"none" {
            return Ok([0, 0, 0, 0]);
        }

        if let Ok(idx) =
            x11r6colornames::COLORS.binary_search_by(|entry| entry.0.as_bytes().cmp(data))
        {
            let entry = x11r6colornames::COLORS[idx];
            Ok([
                scale_u8_to_u16(entry.1),
                scale_u8_to_u16(entry.2),
                scale_u8_to_u16(entry.3),
                0xffff,
            ])
        } else {
            // At this point, `data` has been validated as alphanumeric ASCII
            let t = std::str::from_utf8(data).ok().unwrap_or("");
            Err(XpmDecodeError::UnknownColor(t.into()))
        }
    }
}

impl<R> XpmDecoder<R>
where
    R: BufRead,
{
    /// Create a new [XpmDecoder].
    pub fn new(reader: R) -> Result<XpmDecoder<R>, ImageError> {
        let mut r = TextReader::new(reader)?;

        // Note: XPM3 header is `/* XPM */`

        read_fixed_string(&mut r, b"/* XPM */", XpmPart::Header)?;
        read_to_newline(&mut r, XpmPart::Header)?;

        skip_whitespace_and_comments(&mut r, XpmPart::ArrayStart)?;
        read_fixed_string(&mut r, b"static", XpmPart::ArrayStart)?;
        if skip_whitespace_and_comments(&mut r, XpmPart::ArrayStart)? == 0 {
            /* need a space or other char between 'static' and 'char' */
            return Err(XpmDecodeError::Parse(XpmPart::ArrayStart, r.loc()).into());
        }
        read_fixed_string(&mut r, b"char", XpmPart::ArrayStart)?;
        skip_whitespace_and_comments(&mut r, XpmPart::ArrayStart)?;
        read_fixed_string(&mut r, b"*", XpmPart::ArrayStart)?;
        skip_whitespace_and_comments(&mut r, XpmPart::ArrayStart)?;
        read_name(&mut r, XpmPart::ArrayStart)?;
        skip_whitespace_and_comments(&mut r, XpmPart::ArrayStart)?;
        read_fixed_string(&mut r, b"[", XpmPart::ArrayStart)?;
        skip_whitespace_and_comments(&mut r, XpmPart::ArrayStart)?;
        read_fixed_string(&mut r, b"]", XpmPart::ArrayStart)?;
        skip_whitespace_and_comments(&mut r, XpmPart::ArrayStart)?;
        read_fixed_string(&mut r, b"=", XpmPart::ArrayStart)?;
        skip_whitespace_and_comments(&mut r, XpmPart::ArrayStart)?;
        read_fixed_string(&mut r, b"{", XpmPart::ArrayStart)?;
        skip_whitespace_and_comments(&mut r, XpmPart::ArrayStart)?;

        /* next: read \" */
        read_fixed_string(&mut r, b"\"", XpmPart::FirstLine)?;

        // Inside strings, only spaces are allowed for separators
        let mut int_buf = [0u8; 10]; // 2^32 fits in 10 bytes
        skip_spaces_and_tabs(&mut r)?; // words separated by space & tabulation chars -- so skip both?
        let int = read_until_whitespace_or_eos(&mut r, &mut int_buf, XpmPart::FirstLine)?;
        let width = parse_u32(int).ok_or(XpmDecodeError::Parse(XpmPart::FirstLine, r.loc()))?;

        read_whitespace_gap(&mut r, XpmPart::FirstLine)?;
        let int = read_until_whitespace_or_eos(&mut r, &mut int_buf, XpmPart::FirstLine)?;
        let height = parse_u32(int).ok_or(XpmDecodeError::Parse(XpmPart::FirstLine, r.loc()))?;
        read_whitespace_gap(&mut r, XpmPart::FirstLine)?;
        let int = read_until_whitespace_or_eos(&mut r, &mut int_buf, XpmPart::FirstLine)?;
        let ncolors = parse_u32(int).ok_or(XpmDecodeError::Parse(XpmPart::FirstLine, r.loc()))?;
        read_whitespace_gap(&mut r, XpmPart::FirstLine)?;
        let int = read_until_whitespace_or_eos(&mut r, &mut int_buf, XpmPart::FirstLine)?;
        let cpp = parse_u32(int).ok_or(XpmDecodeError::Parse(XpmPart::FirstLine, r.loc()))?;
        skip_spaces_and_tabs(&mut r)?;

        let _hotspot = if let Some(b'"') = r.peek() {
            // Done
            None
        } else {
            let int = read_until_whitespace_or_eos(&mut r, &mut int_buf, XpmPart::FirstLine)?;
            let hotspot_x =
                parse_i32(int).ok_or(XpmDecodeError::Parse(XpmPart::FirstLine, r.loc()))?;
            read_whitespace_gap(&mut r, XpmPart::FirstLine)?;
            let int = read_until_whitespace_or_eos(&mut r, &mut int_buf, XpmPart::FirstLine)?;
            let hotspot_y =
                parse_i32(int).ok_or(XpmDecodeError::Parse(XpmPart::FirstLine, r.loc()))?;
            skip_spaces_and_tabs(&mut r)?;

            // Parse hotspot now.
            Some((hotspot_x, hotspot_y))
        };
        // XPMEXT tags are not supported -- they were essentially never used in practice.

        read_fixed_string(&mut r, b"\"", XpmPart::FirstLine)?;
        skip_whitespace_and_comments(&mut r, XpmPart::FirstLine)?;
        read_fixed_string(&mut r, b",", XpmPart::FirstLine)?;
        skip_whitespace_and_comments(&mut r, XpmPart::FirstLine)?;

        if ncolors == 0 {
            return Err(XpmDecodeError::ZeroColors.into());
        }
        if cpp == 0 || cpp > 8 {
            /* cpp larger than 8 is pointless and would not be made by sane encoders:
             * with hex encoding, it would allow 2^32 distinct colors. */
            return Err(XpmDecodeError::BadCharsPerColor(cpp).into());
        }

        Ok(XpmDecoder {
            r,
            width,
            height,
            ncolors,
            cpp,
        })
    }
}

/// Parse color, returning it if the key is also XpmVisual::Color
fn handle_key_color(key: &XpmVisual, color: &[u8]) -> Result<Option<[u16; 4]>, XpmDecodeError> {
    if matches!(key, XpmVisual::Symbolic) {
        return Ok(None);
    }
    let color = parse_color(color)?;
    if matches!(key, XpmVisual::Color) {
        Ok(Some(color))
    } else {
        Ok(None)
    }
}

impl<R: BufRead> ImageDecoder for XpmDecoder<R> {
    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    fn color_type(&self) -> ColorType {
        // note: some images specify 16-bpc colors, and fully transparent pixels are possible,
        // so RGBA16 is needed to handle all possible cases
        ColorType::Rgba16
    }
    fn read_image(mut self, buf: &mut [u8]) -> ImageResult<()>
    where
        Self: Sized,
    {
        assert!(1 <= self.cpp && self.cpp <= 8);
        // Check that color table is sorted
        assert!(x11r6colornames::COLORS.windows(2).all(|p| p[0].0 < p[1].0));

        // Even though the file provides a value for `ncolors`, and memory limits are validated,
        // do NOT reserve the suggested memory in advance. Dynamically resizing the vector
        // is negligibly slower, but ensures that the amount of memory allocated is always
        // bounded by a multiple of the actual file size. Kernel virtual memory optimizations
        // may hide the performance cost of allocating a 100MB color table from the
        // application, but such allocations are still expensive even if mostly unused.
        let mut color_table: Vec<XpmColorCodeEntry> = Vec::new();

        for _col in 0..self.ncolors {
            read_fixed_string(&mut self.r, b"\"", XpmPart::Palette)?;

            let mut code = [0_u8; 8];
            read_all_except_eos(
                &mut self.r,
                &mut code[..self.cpp as usize],
                XpmPart::Palette,
            )?;
            read_whitespace_gap(&mut self.r, XpmPart::Palette)?;

            // Color parsing: XPM color specifications have the form {<key> <color>}+
            // This is tricky to parse correctly as color names may contain spaces.
            // Fortunately, the key values are "m", "s", "g4", "g", "c", which will
            // never be a word within a color name, so one can acquire the entire color
            // name by parsing until the next key appears or until '"' arrives.

            // Like the X server, this parser does a case-insensitive match on color names.
            // Unfortunately, there is no general way to handle spaces in names: the color
            // name database includes variants with spaces for multi-word names that do not
            // end in a number; e.g. "antiquewhite" has a split variation "antique white",
            // but "antiquewhite3" does not.

            let mut color_name_buf = [0_u8; MAX_COLOR_NAME_LEN];
            let mut color_name_len = 0;
            let mut next_buf = [0_u8; MAX_COLOR_NAME_LEN];

            let mut key: Option<XpmVisual> = None;

            let mut cvis_color = None;
            loop {
                if let Some(b'"') = self.r.peek() {
                    let Some(ref k) = key else {
                        // At end of line, must have read a key
                        return Err(XpmDecodeError::MissingEntry.into());
                    };
                    if color_name_len == 0 {
                        // At end of line, must also have read a color to process
                        return Err(XpmDecodeError::MissingColorAfterKey.into());
                    }

                    let color = handle_key_color(k, &color_name_buf[..color_name_len])?;
                    cvis_color = color.or(cvis_color);
                    break;
                }

                let next =
                    read_until_whitespace_or_eos(&mut self.r, &mut next_buf, XpmPart::Palette)?;
                skip_spaces_and_tabs(&mut self.r)?;

                let this_key = match &next[..] {
                    b"m" => Some(XpmVisual::Mono),
                    b"s" => Some(XpmVisual::Symbolic),
                    b"g4" => Some(XpmVisual::Grayscale4),
                    b"g" => Some(XpmVisual::Grayscale),
                    b"c" => Some(XpmVisual::Color),
                    _ => None,
                };

                let Some(ref k) = key else {
                    // No key has been set, is first key-color pair in the line
                    if this_key.is_none() {
                        // Error: processing non-key value with no preceding key
                        return Err(XpmDecodeError::MissingKeyBeforeColor.into());
                    };

                    key = this_key;
                    continue;
                };

                if this_key.is_some() {
                    // End of preceding segment
                    if color_name_len == 0 {
                        return Err(XpmDecodeError::TwoKeysInARow.into());
                    }

                    let color = handle_key_color(k, &color_name_buf[..color_name_len])?;
                    cvis_color = color.or(cvis_color);
                    color_name_len = 0;
                    key = this_key;
                    continue;
                }

                // Validate word, case fold it, and concatenate it with the preceding word,
                // adding a space betweeen words
                if color_name_len > 0 {
                    if color_name_len < MAX_COLOR_NAME_LEN {
                        color_name_buf[color_name_len] = b' ';
                        color_name_len += 1;
                    } else {
                        return Err(XpmDecodeError::ColorNameTooLong.into());
                    }
                }
                for c in next {
                    if !valid_name_char(*c) {
                        return Err(XpmDecodeError::InvalidColorName.into());
                    }
                    // Reduce to lowercase, matching the color name database, to
                    // make regular string comparisons be case-insensitive
                    if color_name_len < MAX_COLOR_NAME_LEN {
                        color_name_buf[color_name_len] = fold_to_lower(*c);
                        color_name_len += 1;
                    } else {
                        return Err(XpmDecodeError::ColorNameTooLong.into());
                    }
                }
            }

            let Some(color) = cvis_color else {
                return Err(XpmDecodeError::NoColorModeColorSpecified.into());
            };

            color_table.push(XpmColorCodeEntry {
                code: u64::from_le_bytes(code),
                value: color,
            });

            read_fixed_string(&mut self.r, b"\"", XpmPart::Palette)?;
            skip_whitespace_and_comments(&mut self.r, XpmPart::Palette)?;
            read_fixed_string(&mut self.r, b",", XpmPart::Palette)?;
            skip_whitespace_and_comments(&mut self.r, XpmPart::Palette)?;
        }

        // Sort table and check for duplicates
        color_table.sort_unstable_by(|x, y| x.code.cmp(&y.code));
        for w in color_table.windows(2) {
            if w[0].code.cmp(&w[1].code) != Ordering::Less {
                return Err(XpmDecodeError::DuplicateCode.into());
            }
        }

        // Read main image contents
        let stride = (self.width as usize) * 8;
        for (i, row) in buf.chunks_exact_mut(stride).enumerate() {
            read_fixed_string(&mut self.r, b"\"", XpmPart::Body)?;

            for chunk in row.chunks_mut(8) {
                let mut code = [0_u8; 8];
                read_all_except_eos(
                    &mut self.r,
                    &mut code[..self.cpp as usize],
                    XpmPart::Palette,
                )?;
                let code = u64::from_le_bytes(code);

                let Ok(index) = color_table.binary_search_by(|entry| entry.code.cmp(&code)) else {
                    return Err(XpmDecodeError::UnknownCode.into());
                };

                let color = color_table[index].value;
                // ColorType::Rgba16 is currently native endian, R,G,B,A channel order
                chunk[0..2].copy_from_slice(&color[0].to_ne_bytes());
                chunk[2..4].copy_from_slice(&color[1].to_ne_bytes());
                chunk[4..6].copy_from_slice(&color[2].to_ne_bytes());
                chunk[6..8].copy_from_slice(&color[3].to_ne_bytes());
            }

            read_fixed_string(&mut self.r, b"\"", XpmPart::Body)?;

            if i >= (self.height - 1) as usize {
                // No trailing comma
            } else {
                skip_whitespace_and_comments(&mut self.r, XpmPart::Body)?;
                read_fixed_string(&mut self.r, b",", XpmPart::Body)?;
                skip_whitespace_and_comments(&mut self.r, XpmPart::Body)?;
            }
        }

        // Read optional comma, followed by final };
        skip_whitespace_and_comments(&mut self.r, XpmPart::Trailing)?;
        let next = read_byte(&mut self.r, XpmPart::Trailing)?;
        if next == b',' {
            skip_whitespace_and_comments(&mut self.r, XpmPart::Trailing)?;
            read_fixed_string(&mut self.r, b"}", XpmPart::Trailing)?;
        } else if next != b'}' {
            return Err(XpmDecodeError::Parse(XpmPart::Trailing, self.r.loc()).into());
        }
        skip_whitespace_and_comments(&mut self.r, XpmPart::Trailing)?;
        read_fixed_string(&mut self.r, b";", XpmPart::Trailing)?;

        skip_whitespace_and_comments(&mut self.r, XpmPart::AfterEnd)?;
        if self.r.next()?.is_some() {
            // File has unexpected trailing contents.
            return Err(XpmDecodeError::Parse(XpmPart::AfterEnd, self.r.loc()).into());
        }
        Ok(())
    }
    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }

    fn set_limits(&mut self, limits: Limits) -> ImageResult<()> {
        limits.check_support(&crate::LimitSupport::default())?;
        let (width, height) = self.dimensions();
        limits.check_dimensions(width, height)?;

        let max_pixels = u64::from(self.width) * u64::from(self.height);
        let max_image_bytes =
            max_pixels
                .checked_mul(8)
                .ok_or(ImageError::Limits(LimitError::from_kind(
                    LimitErrorKind::DimensionError,
                )))?;

        let max_table_bytes = (self.ncolors as u64) * (size_of::<XpmColorCodeEntry>() as u64);
        let max_bytes = max_image_bytes
            .checked_add(max_table_bytes)
            .ok_or(ImageError::Limits(LimitError::from_kind(
                LimitErrorKind::InsufficientMemory,
            )))?;

        let max_alloc = limits.max_alloc.unwrap_or(u64::MAX);
        if max_alloc < max_bytes {
            return Err(ImageError::Limits(LimitError::from_kind(
                LimitErrorKind::InsufficientMemory,
            )));
        }
        Ok(())
    }
}
