//! Reads the in-camera colour profile (film simulation, picture style, ...) from vendor MakerNotes.
//!
//! Tag numbers and value tables follow ExifTool's Panasonic, Canon, FujiFilm, Nikon and Sony modules.

use exif::{Exif, In, Tag, Value};

struct Ifd<'a> {
    buf: &'a [u8],
    le: bool,
    /// Offsets inside the IFD are relative to this position in `buf`.
    base: usize,
    start: usize,
}

impl<'a> Ifd<'a> {
    fn u16(&self, at: usize) -> Option<u16> {
        let b = self.buf.get(at..at + 2)?;
        Some(if self.le { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) })
    }

    fn u32(&self, at: usize) -> Option<u32> {
        let b: [u8; 4] = self.buf.get(at..at + 4)?.try_into().ok()?;
        Some(if self.le { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
    }

    /// Raw bytes of the value for `tag`, if present.
    fn get(&self, tag: u16) -> Option<&'a [u8]> {
        let count = self.u16(self.start)? as usize;
        for i in 0..count.min(512) {
            let e = self.start + 2 + i * 12;
            if self.u16(e)? != tag {
                continue;
            }
            let size = match self.u16(e + 2)? {
                1 | 2 | 6 | 7 => 1,
                3 | 8 => 2,
                4 | 9 | 11 => 4,
                5 | 10 | 12 => 8,
                _ => return None,
            } * self.u32(e + 4)? as usize;
            let at = if size <= 4 { e + 8 } else { self.base + self.u32(e + 8)? as usize };
            return self.buf.get(at..at + size);
        }
        None
    }

    fn get_u16(&self, tag: u16, index: usize) -> Option<u16> {
        let v = self.get(tag)?;
        let b = v.get(index * 2..index * 2 + 2)?;
        Some(if self.le { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) })
    }
}

fn text(bytes: &[u8]) -> Option<String> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let s = String::from_utf8_lossy(&bytes[..end]).trim().to_owned();
    (!s.is_empty()).then_some(s)
}

/// "MY FILM LOOK" -> "My Film Look" (Nikon stores Picture Control names in upper case).
fn title_case(s: &str) -> String {
    s.split(' ')
        .map(|w| {
            let mut c = w.chars();
            c.next().map_or(String::new(), |f| f.to_uppercase().chain(c.flat_map(char::to_lowercase)).collect())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The camera's colour profile, e.g. "Classic Chrome", "Nostalgic", "User Def. 3 (PC 3)".
pub fn color_profile(exif: &Exif) -> Option<String> {
    let field = exif.get_field(Tag::MakerNote, In::PRIMARY)?;
    let Value::Undefined(note, offset) = &field.value else { return None };
    let buf = exif.buf();
    let mn = *offset as usize;
    let tiff_ifd = |start: usize| Ifd { buf, le: exif.little_endian(), base: 0, start };

    if note.starts_with(b"Panasonic\0") {
        return panasonic(&tiff_ifd(mn + 12));
    }
    if note.starts_with(b"FUJIFILM") {
        let start = u32::from_le_bytes(note.get(8..12)?.try_into().ok()?) as usize;
        return fujifilm(&Ifd { buf, le: true, base: mn, start: mn + start });
    }
    if note.starts_with(b"Nikon\0\x02") {
        // An embedded TIFF header follows the 10-byte signature; offsets are relative to it.
        let base = mn + 10;
        let le = note.get(10..12)? == b"II";
        let ifd = Ifd { buf, le, base, start: 0 };
        let start = base + ifd.u32(base + 4)? as usize;
        return nikon(&Ifd { start, ..ifd });
    }
    if note.starts_with(b"SONY DSC ") || note.starts_with(b"SONY CAM ") {
        return sony(&tiff_ifd(mn + 12));
    }
    let make = exif.get_field(Tag::Make, In::PRIMARY).map(|f| f.display_value().to_string()).unwrap_or_default();
    let make = make.trim_matches('"').to_lowercase();
    if make.starts_with("canon") {
        return canon(&tiff_ifd(mn));
    }
    if make.starts_with("sony") {
        return sony(&tiff_ifd(mn));
    }
    None
}

fn panasonic(ifd: &Ifd) -> Option<String> {
    // Older bodies (e.g. LX3, GH1) use FilmMode; newer ones use PhotoStyle.
    let film = match ifd.get_u16(0x42, 0) {
        Some(1) => Some("Standard (color)"),
        Some(2) => Some("Dynamic (color)"),
        Some(3) => Some("Nature (color)"),
        Some(4) => Some("Smooth (color)"),
        Some(5) => Some("Standard (B&W)"),
        Some(6) => Some("Dynamic (B&W)"),
        Some(7) => Some("Smooth (B&W)"),
        Some(10) => Some("Nostalgic"),
        Some(11) => Some("Vibrant"),
        _ => None,
    };
    let style = || match ifd.get_u16(0x89, 0)? {
        0 => Some("Auto"),
        1 => Some("Standard or Custom"),
        2 => Some("Vivid"),
        3 => Some("Natural"),
        4 => Some("Monochrome"),
        5 => Some("Scenery"),
        6 => Some("Portrait"),
        8 => Some("Cinelike D"),
        9 => Some("Cinelike V"),
        11 => Some("L. Monochrome"),
        12 => Some("Like709"),
        15 => Some("L. Monochrome D"),
        17 => Some("V-Log"),
        18 => Some("Cinelike D2"),
        _ => None,
    };
    film.or_else(style).map(str::to_owned)
}

fn canon_style(code: u16) -> Option<&'static str> {
    Some(match code {
        0x01 => "Standard",
        0x02 => "Portrait",
        0x03 => "High Saturation",
        0x04 => "Adobe RGB",
        0x05 => "Low Saturation",
        0x06 => "CM Set 1",
        0x07 => "CM Set 2",
        0x21 => "User Def. 1",
        0x22 => "User Def. 2",
        0x23 => "User Def. 3",
        0x41 => "PC 1",
        0x42 => "PC 2",
        0x43 => "PC 3",
        0x81 => "Standard",
        0x82 => "Portrait",
        0x83 => "Landscape",
        0x84 => "Neutral",
        0x85 => "Faithful",
        0x86 => "Monochrome",
        0x87 => "Auto",
        0x88 => "Fine Detail",
        _ => return None,
    })
}

fn canon(ifd: &Ifd) -> Option<String> {
    // ProcessingInfo (0xa0) is an int16 array; PictureStyle is entry 10.
    let code = ifd.get_u16(0xa0, 10)?;
    let name = canon_style(code)?;
    // For user-defined styles, PictureStyleUserDef (0x4008) holds the style each is based on.
    if (0x21..=0x23).contains(&code)
        && let Some(base) = ifd.get_u16(0x4008, (code - 0x21) as usize).and_then(canon_style)
    {
        return Some(format!("{name} ({base})"));
    }
    Some(name.to_owned())
}

fn fujifilm(ifd: &Ifd) -> Option<String> {
    // Monochrome simulations are recorded in the Saturation tag rather than FilmMode.
    let mono = match ifd.get_u16(0x1003, 0) {
        Some(0x300) => Some("Monochrome"),
        Some(0x301) => Some("Monochrome + R Filter"),
        Some(0x302) => Some("Monochrome + Ye Filter"),
        Some(0x303) => Some("Monochrome + G Filter"),
        Some(0x310) => Some("Sepia"),
        Some(0x500) => Some("Acros"),
        Some(0x501) => Some("Acros + R Filter"),
        Some(0x502) => Some("Acros + Ye Filter"),
        Some(0x503) => Some("Acros + G Filter"),
        _ => None,
    };
    if let Some(m) = mono {
        return Some(m.to_owned());
    }
    let film = match ifd.get_u16(0x1401, 0)? {
        0x000 => "Provia / Standard",
        0x100 => "Studio Portrait",
        0x110 => "Studio Portrait Enhanced Saturation",
        0x120 => "Astia / Soft",
        0x130 => "Studio Portrait Increased Sharpness",
        0x200 => "Velvia / Vivid",
        0x300 => "Studio Portrait Ex",
        0x400 => "Velvia",
        0x500 => "Pro Neg. Std",
        0x501 => "Pro Neg. Hi",
        0x600 => "Classic Chrome",
        0x700 => "Eterna",
        0x800 => "Classic Negative",
        0x900 => "Eterna Bleach Bypass",
        0xa00 => "Nostalgic Neg.",
        0xb00 => "Reala Ace",
        _ => return None,
    };
    Some(film.to_owned())
}

fn nikon(ifd: &Ifd) -> Option<String> {
    let data = ifd.get(0x0023)?;
    let (name_at, base_at) = if data.starts_with(b"03") { (8, 28) } else { (4, 24) };
    let name = title_case(&text(data.get(name_at..name_at + 20)?)?);
    match data.get(base_at..base_at + 20).and_then(text).map(|b| title_case(&b)) {
        Some(base) if base != name => Some(format!("{name} ({base})")),
        _ => Some(name),
    }
}

fn sony(ifd: &Ifd) -> Option<String> {
    text(ifd.get(0xb020)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a little-endian IFD with the given (tag, type, count, value bytes) entries.
    fn ifd_bytes(entries: &[(u16, u16, u32, Vec<u8>)]) -> Vec<u8> {
        let mut out = (entries.len() as u16).to_le_bytes().to_vec();
        let data_start = 2 + entries.len() * 12 + 4;
        let mut data: Vec<u8> = Vec::new();
        for (tag, typ, count, value) in entries {
            out.extend(tag.to_le_bytes());
            out.extend(typ.to_le_bytes());
            out.extend(count.to_le_bytes());
            if value.len() <= 4 {
                let mut v = value.clone();
                v.resize(4, 0);
                out.extend(v);
            } else {
                out.extend(((data_start + data.len()) as u32).to_le_bytes());
                data.extend(value);
            }
        }
        out.extend([0; 4]);
        out.extend(data);
        out
    }

    fn u16s(v: &[u16]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    #[test]
    fn panasonic_film_mode_then_photo_style() {
        let buf = ifd_bytes(&[(0x42, 3, 1, u16s(&[10]))]);
        assert_eq!(panasonic(&Ifd { buf: &buf, le: true, base: 0, start: 0 }).as_deref(), Some("Nostalgic"));
        let buf = ifd_bytes(&[(0x42, 3, 1, u16s(&[0])), (0x89, 3, 1, u16s(&[8]))]);
        assert_eq!(panasonic(&Ifd { buf: &buf, le: true, base: 0, start: 0 }).as_deref(), Some("Cinelike D"));
    }

    #[test]
    fn canon_user_def_shows_base() {
        let mut processing = vec![0u16; 12];
        processing[10] = 0x23;
        let buf = ifd_bytes(&[(0xa0, 3, 12, u16s(&processing)), (0x4008, 3, 3, u16s(&[0x81, 0x84, 0x43]))]);
        assert_eq!(canon(&Ifd { buf: &buf, le: true, base: 0, start: 0 }).as_deref(), Some("User Def. 3 (PC 3)"));
    }

    #[test]
    fn fujifilm_film_and_mono() {
        let buf = ifd_bytes(&[(0x1401, 3, 1, u16s(&[0x600]))]);
        assert_eq!(fujifilm(&Ifd { buf: &buf, le: true, base: 0, start: 0 }).as_deref(), Some("Classic Chrome"));
        let buf = ifd_bytes(&[(0x1003, 3, 1, u16s(&[0x501])), (0x1401, 3, 1, u16s(&[0]))]);
        assert_eq!(fujifilm(&Ifd { buf: &buf, le: true, base: 0, start: 0 }).as_deref(), Some("Acros + R Filter"));
    }

    #[test]
    fn nikon_picture_control() {
        let mut data = b"0310".to_vec();
        data.extend([0; 4]);
        let mut name = b"MY FILM".to_vec();
        name.resize(20, 0);
        let mut base = b"STANDARD".to_vec();
        base.resize(20, 0);
        data.extend(name);
        data.extend(base);
        let buf = ifd_bytes(&[(0x23, 7, data.len() as u32, data)]);
        assert_eq!(nikon(&Ifd { buf: &buf, le: true, base: 0, start: 0 }).as_deref(), Some("My Film (Standard)"));
    }
}

/// Checks against real camera files in `test-images/` (not committed); skipped when absent.
#[cfg(test)]
mod sample_tests {
    #[test]
    fn sample_images() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("test-images");
        for (file, want) in [("P1620583.JPG", "Nostalgic"), ("IMG_1959.JPG", "User Def. 3 (PC 3)")] {
            let Ok(bytes) = std::fs::read(dir.join(file)) else { continue };
            let exif = exif::Reader::new().read_from_container(&mut std::io::Cursor::new(&bytes)).unwrap();
            assert_eq!(super::color_profile(&exif).as_deref(), Some(want), "{file}");
        }
    }
}
