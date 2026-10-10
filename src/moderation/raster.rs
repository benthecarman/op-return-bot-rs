//! Check that an image holds one picture and nothing else.
//!
//! The decision service sees only the picture a decoder draws, but the
//! whole file is written on chain. Bytes after the end marker, extra
//! frames, thumbnails, and metadata chunks would all be stored without
//! being seen, so an image that has any of them is refused. The check
//! walks the container structure. It does not decode the pixel data.

/// Chunks outside the picture data, such as a colour profile, may total
/// this many bytes.
const MAX_EXTRA_BYTES: usize = 4096;
/// A PNG chunk that no decoder needs, such as the one-byte orientation
/// chunk `ImageMagick` writes, may be this long.
const MAX_SPARE_PNG_CHUNK: usize = 16;

/// Why an image was refused, for the log.
pub(super) type Refusal = &'static str;

pub(super) fn check(mime: &str, bytes: &[u8]) -> Result<(), Refusal> {
    match mime {
        "image/png" => png(bytes),
        "image/jpeg" => jpeg(bytes),
        "image/gif" => gif(bytes),
        "image/webp" => webp(bytes),
        _ => Err("image type is not supported"),
    }
}

const CUT_SHORT: Refusal = "image is cut short";

fn take(bytes: &[u8], start: usize, len: usize) -> Result<&[u8], Refusal> {
    start
        .checked_add(len)
        .and_then(|end| bytes.get(start..end))
        .ok_or(CUT_SHORT)
}

fn byte(bytes: &[u8], at: usize) -> Result<u8, Refusal> {
    bytes.get(at).copied().ok_or(CUT_SHORT)
}

fn be_u16(bytes: &[u8], at: usize) -> Result<usize, Refusal> {
    let raw = take(bytes, at, 2)?;
    Ok(usize::from(u16::from_be_bytes([raw[0], raw[1]])))
}

fn u32_len(raw: &[u8], big_endian: bool) -> Result<usize, Refusal> {
    let array = [raw[0], raw[1], raw[2], raw[3]];
    let value = if big_endian {
        u32::from_be_bytes(array)
    } else {
        u32::from_le_bytes(array)
    };
    usize::try_from(value).map_err(|_| CUT_SHORT)
}

fn add_extra(total: &mut usize, len: usize) -> Result<(), Refusal> {
    *total = total.saturating_add(len);
    if *total > MAX_EXTRA_BYTES {
        return Err("image has too much data outside the picture");
    }
    Ok(())
}

fn png(bytes: &[u8]) -> Result<(), Refusal> {
    let mut at = 8;
    let mut extra = 0;
    loop {
        let len = u32_len(take(bytes, at, 4)?, true)?;
        let kind = take(bytes, at + 4, 4)?;
        let end = len
            .checked_add(12)
            .and_then(|chunk| at.checked_add(chunk))
            .ok_or(CUT_SHORT)?;
        if end > bytes.len() {
            return Err(CUT_SHORT);
        }
        match kind {
            b"IEND" if end == bytes.len() => return Ok(()),
            b"IEND" => return Err("png has data after its end"),
            b"IHDR" | b"PLTE" | b"IDAT" => {}
            b"acTL" | b"fcTL" | b"fdAT" => return Err("png is animated"),
            b"tRNS" | b"gAMA" | b"cHRM" | b"sRGB" | b"sBIT" | b"bKGD" | b"pHYs" | b"iCCP"
            | b"cICP" => add_extra(&mut extra, len)?,
            // A lowercase first letter marks a chunk decoders may skip.
            _ if kind[0].is_ascii_lowercase() && len <= MAX_SPARE_PNG_CHUNK => {
                add_extra(&mut extra, len)?;
            }
            _ => return Err("png has a metadata or unknown chunk"),
        }
        at = end;
    }
}

fn jpeg(bytes: &[u8]) -> Result<(), Refusal> {
    let mut at = 2;
    let mut extra = 0;
    loop {
        if byte(bytes, at)? != 0xff {
            return Err("jpeg has bytes outside a segment");
        }
        // Any number of 0xff bytes may pad a marker.
        let mut marker = byte(bytes, at + 1)?;
        at += 2;
        while marker == 0xff {
            marker = byte(bytes, at)?;
            at += 1;
        }
        match marker {
            0xd9 if at == bytes.len() => return Ok(()),
            0xd9 => return Err("jpeg has data after its end"),
            0x01 | 0xd0..=0xd7 => continue,
            _ => {}
        }
        let len = be_u16(bytes, at)?;
        if len < 2 {
            return Err("jpeg segment has a bad length");
        }
        let body = take(bytes, at + 2, len - 2)?;
        match marker {
            // Frame, table, and restart segments.
            0xc0..=0xc7 | 0xc9..=0xcf | 0xdb | 0xdc | 0xdd => {}
            0xda => {
                at = skip_scan(bytes, at + len)?;
                continue;
            }
            // JFIF header without a thumbnail.
            0xe0 if body.starts_with(b"JFIF\0") && body.get(12..14) == Some(&[0, 0]) => {}
            0xe2 if body.starts_with(b"ICC_PROFILE\0") => add_extra(&mut extra, len)?,
            0xee if body.starts_with(b"Adobe") => add_extra(&mut extra, len)?,
            _ => return Err("jpeg has a metadata, thumbnail, or unknown segment"),
        }
        at += len;
    }
}

/// Skip entropy-coded scan data. Returns the offset of the next marker.
fn skip_scan(bytes: &[u8], mut at: usize) -> Result<usize, Refusal> {
    loop {
        if byte(bytes, at)? != 0xff {
            at += 1;
            continue;
        }
        match byte(bytes, at + 1)? {
            // A stuffed 0xff byte or a restart marker stays in the scan.
            0x00 | 0xd0..=0xd7 => at += 2,
            // Fill before a marker.
            0xff => at += 1,
            _ => return Ok(at),
        }
    }
}

fn gif(bytes: &[u8]) -> Result<(), Refusal> {
    let packed = byte(bytes, 10)?;
    let mut at = 13 + color_table(packed);
    let mut frames = 0;
    loop {
        match byte(bytes, at)? {
            0x2c => {
                frames += 1;
                if frames > 1 {
                    return Err("gif has more than one frame");
                }
                let packed = byte(bytes, at + 9)?;
                // Descriptor, local colour table, and LZW code size.
                at = skip_sub_blocks(bytes, at + 10 + color_table(packed) + 1)?;
            }
            0x21 if byte(bytes, at + 1)? == 0xf9 => at = skip_sub_blocks(bytes, at + 2)?,
            0x21 => return Err("gif has a comment, text, or application block"),
            0x3b if at + 1 == bytes.len() && frames == 1 => return Ok(()),
            0x3b if frames == 0 => return Err("gif has no frame"),
            0x3b => return Err("gif has data after its end"),
            _ => return Err("gif has an unknown block"),
        }
    }
}

const fn color_table(packed: u8) -> usize {
    if packed & 0x80 == 0 {
        0
    } else {
        3 << ((packed & 0x07) + 1)
    }
}

fn skip_sub_blocks(bytes: &[u8], mut at: usize) -> Result<usize, Refusal> {
    loop {
        let size = usize::from(byte(bytes, at)?);
        at += 1;
        if size == 0 {
            return Ok(at);
        }
        take(bytes, at, size)?;
        at += size;
    }
}

fn webp(bytes: &[u8]) -> Result<(), Refusal> {
    let riff = u32_len(take(bytes, 4, 4)?, false)?;
    if riff.checked_add(8) != Some(bytes.len()) {
        return Err("webp size does not match the file");
    }
    let mut at = 12;
    let mut pictures = 0;
    let mut extra = 0;
    while at < bytes.len() {
        let kind = take(bytes, at, 4)?;
        let len = u32_len(take(bytes, at + 4, 4)?, false)?;
        let body = take(bytes, at + 8, len)?;
        match kind {
            b"VP8 " | b"VP8L" => pictures += 1,
            b"VP8X" if body.first().is_some_and(|flags| flags & 0x02 != 0) => {
                return Err("webp is animated");
            }
            b"VP8X" | b"ALPH" => {}
            b"ICCP" => add_extra(&mut extra, len)?,
            b"ANIM" | b"ANMF" => return Err("webp is animated"),
            _ => return Err("webp has a metadata or unknown chunk"),
        }
        // Chunks are padded to an even length.
        at += 8 + len + (len & 1);
    }
    if at != bytes.len() {
        return Err(CUT_SHORT);
    }
    if pictures != 1 {
        return Err("webp must hold exactly one picture");
    }
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use image::ImageEncoder;

    use super::*;

    /// A one-pixel GIF.
    pub(in crate::moderation) const GIF: &[u8] = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\
\xff\xff\xff\x00\x00\x00\
\x21\xf9\x04\x01\x00\x00\x00\x00\
\x2c\x00\x00\x00\x00\x01\x00\x01\x00\x00\
\x02\x02\x44\x01\x00\
\x3b";

    fn png_pixel() -> Vec<u8> {
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[0, 0, 0, 255], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        png
    }

    fn png_chunk(kind: [u8; 4], data: &[u8]) -> Vec<u8> {
        let mut chunk = u32::try_from(data.len()).unwrap().to_be_bytes().to_vec();
        chunk.extend_from_slice(&kind);
        chunk.extend_from_slice(data);
        chunk.extend_from_slice(&[0; 4]);
        chunk
    }

    /// Put `chunk` just before the IEND chunk.
    fn png_with(chunk: &[u8]) -> Vec<u8> {
        let mut png = png_pixel();
        let iend = png.len() - 12;
        png.splice(iend..iend, chunk.iter().copied());
        png
    }

    fn jpeg(segments: &[&[u8]]) -> Vec<u8> {
        let mut jpeg = vec![0xff, 0xd8];
        for segment in segments {
            jpeg.extend_from_slice(segment);
        }
        jpeg.extend_from_slice(&[0xff, 0xd9]);
        jpeg
    }

    const JFIF: &[u8] = b"\xff\xe0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00";
    const DQT: &[u8] = b"\xff\xdb\x00\x04\x00\x01";
    const SOF: &[u8] = b"\xff\xc0\x00\x05\x08\x00\x01";
    const SCAN: &[u8] = b"\xff\xda\x00\x03\x01\x12\xff\x00\x34\xff\xd0\x56";

    fn webp(chunks: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let mut body = b"WEBP".to_vec();
        for (kind, data) in chunks {
            body.extend_from_slice(*kind);
            body.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
            body.extend_from_slice(data);
            if data.len() % 2 == 1 {
                body.push(0);
            }
        }
        let mut file = b"RIFF".to_vec();
        file.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
        file.extend_from_slice(&body);
        file
    }

    #[test]
    fn accepts_a_plain_png() {
        check("image/png", &png_pixel()).unwrap();
        check("image/png", &png_with(&png_chunk(*b"gAMA", &[0; 4]))).unwrap();
        check("image/png", &png_with(&png_chunk(*b"orNT", &[1]))).unwrap();
    }

    #[test]
    fn refuses_a_png_that_hides_data() {
        let mut trailing = png_pixel();
        trailing.extend_from_slice(b"\xff\xd8\xff hidden");
        assert!(check("image/png", &trailing).is_err());
        let text = png_chunk(*b"tEXt", b"Comment\0a hidden note");
        assert!(check("image/png", &png_with(&text)).is_err());
        assert!(check("image/png", &png_with(&png_chunk(*b"acTL", &[0; 8]))).is_err());
        assert!(check("image/png", &png_with(&png_chunk(*b"PRIV", b"x"))).is_err());
        let profile = png_chunk(*b"iCCP", &vec![0; MAX_EXTRA_BYTES + 1]);
        assert!(check("image/png", &png_with(&profile)).is_err());
        let pixel = png_pixel();
        assert!(check("image/png", &pixel[..pixel.len() - 1]).is_err());
    }

    #[test]
    fn accepts_a_plain_jpeg() {
        check("image/jpeg", &jpeg(&[JFIF, DQT, SOF, SCAN])).unwrap();
    }

    #[test]
    fn refuses_a_jpeg_that_hides_data() {
        let mut trailing = jpeg(&[JFIF, DQT, SOF, SCAN]);
        trailing.extend_from_slice(b"\xff\xd8\xff hidden");
        assert!(check("image/jpeg", &trailing).is_err());
        let exif = b"\xff\xe1\x00\x08Exif\x00\x00";
        assert!(check("image/jpeg", &jpeg(&[exif, SOF, SCAN])).is_err());
        let comment = b"\xff\xfe\x00\x04hi";
        assert!(check("image/jpeg", &jpeg(&[comment, SOF, SCAN])).is_err());
        let thumbnail = b"\xff\xe0\x00\x13JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x01\x01\x00\x00\x00";
        assert!(check("image/jpeg", &jpeg(&[thumbnail, SOF, SCAN])).is_err());
        let mut short = jpeg(&[JFIF, SOF, SCAN]);
        short.truncate(short.len() - 2);
        assert!(check("image/jpeg", &short).is_err());
    }

    #[test]
    fn accepts_a_single_frame_gif() {
        check("image/gif", GIF).unwrap();
    }

    #[test]
    fn refuses_a_gif_that_hides_data() {
        let frame = &GIF[27..42];
        let mut animated = GIF[..GIF.len() - 1].to_vec();
        animated.extend_from_slice(frame);
        animated.push(0x3b);
        assert_eq!(
            check("image/gif", &animated),
            Err("gif has more than one frame")
        );
        let mut trailing = GIF.to_vec();
        trailing.extend_from_slice(b"hidden");
        assert!(check("image/gif", &trailing).is_err());
        let mut comment = GIF[..19].to_vec();
        comment.extend_from_slice(b"\x21\xfe\x02hi\x00");
        comment.extend_from_slice(&GIF[19..]);
        assert!(check("image/gif", &comment).is_err());
    }

    #[test]
    fn accepts_a_single_picture_webp() {
        check("image/webp", &webp(&[(b"VP8L", b"abcde")])).unwrap();
        check(
            "image/webp",
            &webp(&[(b"VP8X", &[0; 10]), (b"ALPH", b"a"), (b"VP8 ", b"ab")]),
        )
        .unwrap();
    }

    #[test]
    fn refuses_a_webp_that_hides_data() {
        let mut trailing = webp(&[(b"VP8L", b"abcde")]);
        trailing.extend_from_slice(b"hidden");
        assert!(check("image/webp", &trailing).is_err());
        let animated = webp(&[
            (b"VP8X", &[0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            (b"VP8L", b"a"),
        ]);
        assert!(check("image/webp", &animated).is_err());
        let exif = webp(&[(b"VP8L", b"abcde"), (b"EXIF", b"x")]);
        assert!(check("image/webp", &exif).is_err());
        let two = webp(&[(b"VP8L", b"a"), (b"VP8L", b"b")]);
        assert!(check("image/webp", &two).is_err());
    }
}
