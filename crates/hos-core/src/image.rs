//! Header-only image inspection: format and pixel size read from the
//! container / frame headers without decoding, so a tiny file that would
//! decode to gigabytes (a "decompression bomb") is caught before any decoder
//! sees it. Everything is bounds-checked and allocation-free.
//!
//! Sources of the header layouts:
//! * PNG: `IHDR` is the first chunk, width / height big-endian u32.
//! * JPEG (ITU-T T.81 B.2.2, libjpeg `get_sof`): the frame header of any
//!   `SOFn` marker (C0–C3, C5–C7, C9–CB, CD–CF) is `Lf`(2) `P`(1) `Y`(2)
//!   `X`(2), big-endian; markers without a length are SOI, EOI, RSTn, TEM.
//! * GIF89a: logical screen width / height little-endian u16 at bytes 6–9;
//!   every image descriptor (`0x2C`) carries its own frame size.
//! * WebP (RFC 9649): `RIFF` size `WEBP`, then the first chunk: `VP8X`
//!   (canvas width / height minus one, 24-bit little-endian at 24 / 27),
//!   `VP8L` (signature `0x2f`, then 14-bit width − 1 and height − 1, LSB
//!   first) or `VP8 ` (frame tag, start code `9d 01 2a`, 14-bit width /
//!   height little-endian, RFC 6386 §9.1).
//! * ICO: `ICONDIR` + 16-byte entries (width / height byte, 0 = 256); an
//!   entry holding a PNG is measured by that PNG's `IHDR`.

/// Largest width or height accepted for an uploaded / rendered image.
pub const MAX_SIDE: u32 = 4096;

/// Raster formats recognised by [`dimensions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    /// `image/png`.
    Png,
    /// `image/jpeg`.
    Jpeg,
    /// `image/gif`.
    Gif,
    /// `image/webp`.
    Webp,
    /// `image/x-icon` (favicons only).
    Ico,
}

impl ImageFormat {
    /// MIME type.
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
            Self::Ico => "image/x-icon",
        }
    }
}

fn be16(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from(u16::from_be_bytes([
        *d.get(at)?,
        *d.get(at + 1)?,
    ])))
}

fn le16(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from(u16::from_le_bytes([
        *d.get(at)?,
        *d.get(at + 1)?,
    ])))
}

fn be32(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(d.get(at..at + 4)?.try_into().ok()?))
}

fn le24(d: &[u8], at: usize) -> Option<u32> {
    let b = d.get(at..at + 3)?;
    Some(u32::from(b[0]) | (u32::from(b[1]) << 8) | (u32::from(b[2]) << 16))
}

fn png(d: &[u8]) -> Option<(u32, u32)> {
    if !d.starts_with(b"\x89PNG\r\n\x1a\n") || d.get(12..16)? != b"IHDR" {
        return None;
    }
    Some((be32(d, 16)?, be32(d, 20)?))
}

fn jpeg(d: &[u8]) -> Option<(u32, u32)> {
    if !d.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut i = 2;
    loop {
        // Fill bytes: any number of 0xFF before the marker code.
        if *d.get(i)? != 0xFF {
            return None;
        }
        while *d.get(i)? == 0xFF {
            i += 1;
        }
        let marker = *d.get(i)?;
        i += 1;
        match marker {
            0x01 | 0xD0..=0xD8 => continue,
            0xD9 | 0xDA => return None, // end of image / start of scan before a frame header
            _ => {}
        }
        let len = usize::try_from(be16(d, i)?).ok()?;
        if len < 2 {
            return None;
        }
        if matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) {
            // Lf(2) P(1) Y(2) X(2)
            let h = be16(d, i + 3)?;
            let w = be16(d, i + 5)?;
            return Some((w, h));
        }
        i = i.checked_add(len)?;
    }
}

fn gif(d: &[u8]) -> Option<(u32, u32)> {
    if !(d.starts_with(b"GIF87a") || d.starts_with(b"GIF89a")) {
        return None;
    }
    let (mut w, mut h) = (le16(d, 6)?, le16(d, 8)?);
    let flags = *d.get(10)?;
    let mut i = 13;
    if flags & 0x80 != 0 {
        i += 3 << ((flags & 0x07) + 1);
    }
    let skip_blocks = |mut i: usize| -> Option<usize> {
        loop {
            let n = usize::from(*d.get(i)?);
            i += 1;
            if n == 0 {
                return Some(i);
            }
            i += n;
        }
    };
    loop {
        match *d.get(i)? {
            0x3B => return Some((w, h)),
            0x21 => i = skip_blocks(i + 2)?,
            0x2C => {
                w = w.max(le16(d, i + 5)?);
                h = h.max(le16(d, i + 7)?);
                let f = *d.get(i + 9)?;
                i += 10;
                if f & 0x80 != 0 {
                    i += 3 << ((f & 0x07) + 1);
                }
                i = skip_blocks(i + 1)?; // LZW minimum code size, then data
            }
            _ => return None,
        }
    }
}

fn webp(d: &[u8]) -> Option<(u32, u32)> {
    if !(d.get(..4)? == b"RIFF" && d.get(8..12)? == b"WEBP") {
        return None;
    }
    match d.get(12..16)? {
        b"VP8X" => Some((le24(d, 24)? + 1, le24(d, 27)? + 1)),
        b"VP8L" => {
            if *d.get(20)? != 0x2F {
                return None;
            }
            let bits = u32::from_le_bytes(d.get(21..25)?.try_into().ok()?);
            Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1))
        }
        b"VP8 " => {
            if d.get(23..26)? != [0x9D, 0x01, 0x2A] {
                return None;
            }
            Some((le16(d, 26)? & 0x3FFF, le16(d, 28)? & 0x3FFF))
        }
        _ => None,
    }
}

fn ico(d: &[u8]) -> Option<(u32, u32)> {
    if !d.starts_with(&[0, 0, 1, 0]) {
        return None;
    }
    let count = usize::try_from(le16(d, 4)?).ok()?;
    if count == 0 {
        return None;
    }
    let (mut w, mut h) = (0, 0);
    for n in 0..count {
        let e = 6 + n * 16;
        let side = |b: u8| if b == 0 { 256 } else { u32::from(b) };
        let (mut ew, mut eh) = (side(*d.get(e)?), side(*d.get(e + 1)?));
        let offset =
            usize::try_from(u32::from_le_bytes(d.get(e + 12..e + 16)?.try_into().ok()?)).ok()?;
        let body = d.get(offset..)?;
        if body.starts_with(b"\x89PNG") {
            (ew, eh) = png(body)?;
        }
        w = w.max(ew);
        h = h.max(eh);
    }
    Some((w, h))
}

/// Format and `(width, height)` in pixels; `None` when the format is not
/// one of [`ImageFormat`] or the size cannot be read from the headers.
pub fn dimensions(data: &[u8]) -> Option<(ImageFormat, u32, u32)> {
    let (format, (w, h)) = if let Some(s) = png(data) {
        (ImageFormat::Png, s)
    } else if let Some(s) = jpeg(data) {
        (ImageFormat::Jpeg, s)
    } else if let Some(s) = gif(data) {
        (ImageFormat::Gif, s)
    } else if let Some(s) = webp(data) {
        (ImageFormat::Webp, s)
    } else {
        (ImageFormat::Ico, ico(data)?)
    };
    Some((format, w, h))
}

/// Whether `data` is a recognised raster image whose sides are all between
/// 1 and `max_side` pixels.
pub fn fits(data: &[u8], max_side: u32) -> bool {
    dimensions(data)
        .is_some_and(|(_, w, h)| (1..=max_side).contains(&w) && (1..=max_side).contains(&h))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_of(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&[8, 6, 0, 0, 0]);
        v
    }

    fn webp_lossless(w: u32, h: u32) -> Vec<u8> {
        let bits = (w - 1) | ((h - 1) << 14);
        let mut v = b"RIFF\x1a\0\0\0WEBPVP8L\x0d\0\0\0\x2f".to_vec();
        v.extend_from_slice(&bits.to_le_bytes());
        v.extend_from_slice(&[0; 8]);
        v
    }

    #[test]
    fn png_and_jpeg() {
        assert_eq!(
            dimensions(&png_of(64, 32)),
            Some((ImageFormat::Png, 64, 32))
        );
        // SOI, APP0 (length 16), SOF0 for 300 × 200, SOS.
        let mut jpg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        jpg.extend_from_slice(b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
        jpg.extend_from_slice(&[
            0xFF, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x00, 0xC8, 0x01, 0x2C, 0x03,
        ]);
        jpg.extend_from_slice(&[0; 9]);
        jpg.extend_from_slice(&[0xFF, 0xDA]);
        assert_eq!(dimensions(&jpg), Some((ImageFormat::Jpeg, 300, 200)));
        // Progressive (SOF2) 20000 × 10.
        let big = [
            0xFF, 0xD8, 0xFF, 0xC2, 0x00, 0x11, 0x08, 0x00, 0x0A, 0x4E, 0x20, 0x03,
        ];
        assert_eq!(dimensions(&big), Some((ImageFormat::Jpeg, 20000, 10)));
        assert!(!fits(&big, MAX_SIDE));
        // No frame header before the scan: unknown size.
        assert_eq!(dimensions(&[0xFF, 0xD8, 0xFF, 0xDA, 0, 2]), None);
        assert_eq!(dimensions(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]), None);
    }

    #[test]
    fn webp_headers() {
        let bomb = webp_lossless(16383, 16383);
        assert_eq!(dimensions(&bomb), Some((ImageFormat::Webp, 16383, 16383)));
        assert!(!fits(&bomb, MAX_SIDE));
        assert!(fits(&webp_lossless(480, 300), MAX_SIDE));
        // VP8X canvas 5000 × 1.
        let mut x = b"RIFF\x1a\0\0\0WEBPVP8X\x0a\0\0\0\x10\0\0\0".to_vec();
        x.extend_from_slice(&[0x87, 0x13, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(dimensions(&x), Some((ImageFormat::Webp, 5000, 1)));
        // VP8 key frame 640 × 480 (scale bits ignored).
        let mut l = b"RIFF\x1a\0\0\0WEBPVP8 \x0a\0\0\0\x9d\x01\x2a".to_vec();
        l.splice(20..20, [0x10, 0x02, 0x00]);
        l.extend_from_slice(&(640u16 | 0x4000).to_le_bytes());
        l.extend_from_slice(&480u16.to_le_bytes());
        assert_eq!(dimensions(&l), Some((ImageFormat::Webp, 640, 480)));
        // Container only: no size.
        assert_eq!(dimensions(b"RIFF\x1a\0\0\0WEBP"), None);
    }

    #[test]
    fn gif_frames_and_ico() {
        // 16 × 8 screen, one 4000 × 9000 frame descriptor.
        let mut g = b"GIF89a\x10\0\x08\0\0\0\0".to_vec();
        g.extend_from_slice(&[0x21, 0xF9, 0x04, 0, 0, 0, 0, 0]);
        g.extend_from_slice(&[
            0x2C, 0, 0, 0, 0, 0xA0, 0x0F, 0x28, 0x23, 0x00, 0x02, 0x01, 0x00, 0x00,
        ]);
        g.push(0x3B);
        assert_eq!(dimensions(&g), Some((ImageFormat::Gif, 4000, 9000)));
        assert_eq!(
            dimensions(b"GIF89a\x10\0\x08\0\0\0\0\x3B"),
            Some((ImageFormat::Gif, 16, 8))
        );
        // ICO: one 16 × 16 entry, one PNG entry of 300 × 300.
        let p = png_of(300, 300);
        let mut ico = vec![0, 0, 1, 0, 2, 0];
        ico.extend_from_slice(&[16, 16, 0, 0, 1, 0, 32, 0, 4, 0, 0, 0, 38, 0, 0, 0]);
        ico.extend_from_slice(&[0, 0, 0, 0, 1, 0, 32, 0]);
        ico.extend_from_slice(&u32::try_from(p.len()).unwrap().to_le_bytes());
        ico.extend_from_slice(&42u32.to_le_bytes());
        ico.extend_from_slice(&[0; 4]);
        ico.extend_from_slice(&p);
        assert_eq!(dimensions(&ico), Some((ImageFormat::Ico, 300, 300)));
        assert_eq!(
            dimensions(b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>"),
            None
        );
    }
}
