//! Shooting metadata read from a JPEG's EXIF, formatted for display.

use exif::{Exif, In, Tag, Value};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Meta {
    /// (label, value) rows, in display order. Missing fields are left out.
    pub rows: Vec<(&'static str, String)>,
}

fn ascii(exif: &Exif, tag: Tag) -> Option<String> {
    match &exif.get_field(tag, In::PRIMARY)?.value {
        Value::Ascii(parts) => {
            let s = String::from_utf8_lossy(parts.first()?).trim().to_owned();
            (!s.is_empty()).then_some(s)
        }
        _ => None,
    }
}

fn float(exif: &Exif, tag: Tag) -> Option<f64> {
    let v = match &exif.get_field(tag, In::PRIMARY)?.value {
        Value::Rational(r) => r.first()?.to_f64(),
        Value::SRational(r) => r.first()?.to_f64(),
        other => other.get_uint(0)? as f64,
    };
    v.is_finite().then_some(v)
}

pub fn shutter(seconds: f64) -> String {
    if seconds >= 0.3 {
        format!("{} s", trim(seconds, 1))
    } else {
        format!("1/{} s", (1.0 / seconds).round())
    }
}

pub fn ev(bias: f64) -> String {
    if bias.abs() < 0.05 { "0 EV".to_owned() } else { format!("{:+.1} EV", bias) }
}

/// EXIF writes dates as `2026:09:14 17:02:05`; show them as `2026-09-14 17:02:05`.
pub fn date(v: &str) -> String {
    match v.split_once(' ') {
        Some((d, t)) => format!("{} {t}", d.replace(':', "-")),
        None => v.to_owned(),
    }
}

/// Format with at most `decimals` places, dropping a trailing ".0".
fn trim(v: f64, decimals: usize) -> String {
    let s = format!("{v:.decimals$}");
    s.strip_suffix(".0").map(str::to_owned).unwrap_or(s)
}

impl Meta {
    pub fn from_exif(exif: Option<&Exif>, width: u32, height: u32) -> Meta {
        let mut rows = Vec::new();
        if let Some(exif) = exif {
            let make = ascii(exif, Tag::Make);
            let model = ascii(exif, Tag::Model);
            let camera = match (make, model) {
                // Many cameras repeat the make in the model ("Canon EOS R5").
                (Some(make), Some(model)) if model.to_lowercase().starts_with(&make.to_lowercase()) => Some(model),
                (Some(make), Some(model)) => Some(format!("{make} {model}")),
                (make, model) => make.or(model),
            };
            rows.extend(camera.map(|v| ("Camera", v)));
            rows.extend(ascii(exif, Tag::LensModel).map(|v| ("Lens", v)));
            rows.extend(crate::makernote::color_profile(exif).map(|v| ("Color profile", v)));
            if let Some(f) = float(exif, Tag::FocalLength) {
                let mut s = format!("{} mm", trim(f, 1));
                if let Some(f35) = float(exif, Tag::FocalLengthIn35mmFilm)
                    && (f35 - f).abs() >= 1.0
                {
                    s.push_str(&format!(" ({} mm eq.)", trim(f35, 0)));
                }
                rows.push(("Focal length", s));
            }
            rows.extend(float(exif, Tag::FNumber).map(|v| ("Aperture", format!("f/{}", trim(v, 1)))));
            rows.extend(float(exif, Tag::ExposureTime).filter(|&v| v > 0.0).map(|v| ("Shutter", shutter(v))));
            rows.extend(float(exif, Tag::PhotographicSensitivity).map(|v| ("ISO", trim(v, 0))));
            rows.extend(float(exif, Tag::ExposureBiasValue).map(|v| ("Exposure comp.", ev(v))));
            rows.extend(ascii(exif, Tag::DateTimeOriginal).map(|v| ("Taken", date(&v))));
        }
        rows.push(("Size", format!("{width} × {height}")));
        Meta { rows }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(shutter(1.0 / 250.0), "1/250 s");
        assert_eq!(shutter(0.5), "0.5 s");
        assert_eq!(shutter(2.0), "2 s");
        assert_eq!(ev(0.0), "0 EV");
        assert_eq!(ev(-0.7), "-0.7 EV");
        assert_eq!(ev(1.0), "+1.0 EV");
        assert_eq!(trim(2.8, 1), "2.8");
        assert_eq!(trim(8.0, 1), "8");
        assert_eq!(date("2026:09:14 17:02:05"), "2026-09-14 17:02:05");
    }

    #[test]
    fn without_exif_shows_size() {
        let m = Meta::from_exif(None, 6000, 4000);
        assert_eq!(m.rows, [("Size", "6000 × 4000".to_owned())]);
    }
}
