//! Cover thumbnails and the colour sample clients theme from.

use image::{imageops::FilterType, DynamicImage, GenericImageView, ImageFormat};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::Cursor;

const MAX_W: u32 = 240;
const MAX_H: u32 = 360;
/// Version of the sampling algorithm; bump when it changes.
pub const SAMPLE_VERSION: i64 = 1;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ColorSample {
    pub hex: String,
    pub hue: f64,
    pub saturation: f64,
    pub lightness: f64,
    pub vivid: bool,
    pub version: i64,
}

pub struct Thumbnail {
    pub jpeg: Vec<u8>,
    pub sha256: String,
    pub width: u32,
    pub height: u32,
    pub sample: ColorSample,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Decode an image of any supported type into a bounded JPEG thumbnail with a colour sample.
pub fn thumbnail(bytes: &[u8]) -> Result<Thumbnail, String> {
    // The size is read from the header first: a small file can declare a huge picture.
    let (w, h) = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| format!("cannot read image: {e}"))?
        .into_dimensions()
        .map_err(|e| format!("cannot read image: {e}"))?;
    if w == 0 || h == 0 || w.saturating_mul(h) > 64_000_000 {
        return Err("image has unreasonable dimensions".into());
    }
    let img = image::load_from_memory(bytes).map_err(|e| format!("cannot read image: {e}"))?;
    finish(&img)
}

/// Bound, encode and measure a decoded picture. Real and generated covers both end here, so the
/// stored colour sample is always what this pipeline measures.
fn finish(img: &DynamicImage) -> Result<Thumbnail, String> {
    let thumb = img.resize(MAX_W, MAX_H, FilterType::Lanczos3);
    let rgb = DynamicImage::ImageRgb8(thumb.to_rgb8());
    let mut jpeg = Vec::new();
    rgb.write_to(&mut Cursor::new(&mut jpeg), ImageFormat::Jpeg)
        .map_err(|e| format!("cannot encode: {e}"))?;
    let (tw, th) = rgb.dimensions();
    let sample = colour_sample(&rgb);
    Ok(Thumbnail {
        sha256: sha256_hex(&jpeg),
        jpeg,
        width: tw,
        height: th,
        sample,
    })
}

fn rgb_to_hsl(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < 1e-9 {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if (max - r).abs() < 1e-9 {
        ((g - b) / d + if g < b { 6.0 } else { 0.0 }) * 60.0
    } else if (max - g).abs() < 1e-9 {
        ((b - r) / d + 2.0) * 60.0
    } else {
        ((r - g) / d + 4.0) * 60.0
    };
    (h, s, l)
}

fn hsl_to_rgb(h: f64, s: f64, l: f64) -> (f64, f64, f64) {
    let h = h.rem_euclid(360.0);
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = (h % 360.0) / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    (r1 + m, g1 + m, b1 + m)
}

fn hsl_to_hex(h: f64, s: f64, l: f64) -> String {
    let (r, g, b) = hsl_to_rgb(h, s, l);
    let to = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", to(r), to(g), to(b))
}

/// A title or author reduced to its letters and digits, lowercase: "The  Lantern-Keeper" and
/// "the lantern keeper" are the same book. Used to find duplicates and to colour generated covers.
pub fn normalise(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn identity_hash(title: &str, author: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(normalise(title).as_bytes());
    h.update(b"\n");
    h.update(normalise(author).as_bytes());
    h.finalize().into()
}

/// The hue (degrees) a generated cover for this title and author is built around.
pub fn generated_hue(title: &str, author: &str) -> f64 {
    let d = identity_hash(title, author);
    f64::from(u16::from_be_bytes([d[0], d[1]])) / 65536.0 * 360.0
}

/// A cover for a book that has none: a vertical gradient with two soft blobs, no text (the client
/// puts the title on it). Deterministic: the same normalised title and author always give the
/// same bytes, so every device shows the same colour. It goes through the same pipeline as a real
/// cover, so the stored colour sample is what the server measures; the saturation and lightness
/// are chosen so that it measures `vivid`.
pub fn generated(title: &str, author: &str) -> Thumbnail {
    let d = identity_hash(title, author);
    let hue = generated_hue(title, author);
    let unit = |i: usize| f64::from(d[i]) / 255.0;
    let (w, h) = (MAX_W, MAX_H);
    let top = hsl_to_rgb(hue, 0.70, 0.54);
    let bottom = hsl_to_rgb(hue + (unit(2) - 0.5) * 50.0, 0.66, 0.24);
    // (centre x, centre y, radius, colour, strength), as fractions of the width and height.
    let blobs = [
        (
            0.2 + 0.6 * unit(3),
            0.15 + 0.3 * unit(4),
            0.55 + 0.2 * unit(5),
            hsl_to_rgb(hue + 25.0 + 20.0 * unit(6), 0.80, 0.66),
            0.55,
        ),
        (
            0.2 + 0.6 * unit(7),
            0.6 + 0.3 * unit(8),
            0.6 + 0.2 * unit(9),
            hsl_to_rgb(hue - 30.0 - 20.0 * unit(10), 0.75, 0.34),
            0.5,
        ),
    ];
    let mut img = image::RgbImage::new(w, h);
    for (x, y, px) in img.enumerate_pixels_mut() {
        let (fx, fy) = (
            (f64::from(x) + 0.5) / f64::from(w),
            (f64::from(y) + 0.5) / f64::from(h),
        );
        let mut c = [
            top.0 + (bottom.0 - top.0) * fy,
            top.1 + (bottom.1 - top.1) * fy,
            top.2 + (bottom.2 - top.2) * fy,
        ];
        for (cx, cy, r, col, strength) in &blobs {
            // Distance in units of the width, so a blob is round on a 2:3 picture.
            let dist = ((fx - cx).powi(2) + ((fy - cy) * 1.5).powi(2)).sqrt() / r;
            if dist < 1.0 {
                let falloff = 1.0 - dist;
                let a = strength * falloff * falloff * (3.0 - 2.0 * falloff);
                c[0] += (col.0 - c[0]) * a;
                c[1] += (col.1 - c[1]) * a;
                c[2] += (col.2 - c[2]) * a;
            }
        }
        *px = image::Rgb(c.map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8));
    }
    finish(&DynamicImage::ImageRgb8(img)).expect("a plain RGB picture always encodes")
}

/// Dominant chromatic colour, measured deterministically from the thumbnail.
///
/// 1. Downsample to at most 32 x 48 and read HSL.
/// 2. Drop pixels with lightness under 0.12 or over 0.92, or saturation under 0.18.
/// 3. Weight the rest into 24 hue bins by `saturation * (1 - |lightness - 0.5|)`,
///    counting pixels near the edges half.
/// 4. The winning bin gives hue, saturation and lightness as weighted means.
/// 5. `vivid` is true when kept pixels are at least 5% of all pixels.
pub fn colour_sample(img: &DynamicImage) -> ColorSample {
    let small = img.resize(32, 48, FilterType::Triangle).to_rgb8();
    let (w, h) = small.dimensions();
    let total = (w * h).max(1) as f64;
    let mut bins = [(0.0f64, 0.0f64, 0.0f64, 0.0f64, 0u32); 24]; // weight, sum_sin, sum_cos, sum_s*w, sum_l*w ; count
    let mut sums_s = [0.0f64; 24];
    let mut sums_l = [0.0f64; 24];
    let mut kept = 0u32;
    for (x, y, p) in small.enumerate_pixels() {
        let (hh, s, l) = rgb_to_hsl(
            p[0] as f64 / 255.0,
            p[1] as f64 / 255.0,
            p[2] as f64 / 255.0,
        );
        if !(0.12..=0.92).contains(&l) || s < 0.18 {
            continue;
        }
        kept += 1;
        let edge = x < w / 6 || x >= w - w / 6 || y < h / 6 || y >= h - h / 6;
        let weight = s * (1.0 - (l - 0.5).abs()) * if edge { 0.5 } else { 1.0 };
        let bin = ((hh / 15.0) as usize).min(23);
        let rad = hh.to_radians();
        bins[bin].0 += weight;
        bins[bin].1 += weight * rad.sin();
        bins[bin].2 += weight * rad.cos();
        bins[bin].4 += 1;
        sums_s[bin] += weight * s;
        sums_l[bin] += weight * l;
    }
    let vivid = (kept as f64) / total >= 0.05;
    let best = (0..24)
        .max_by(|&a, &b| {
            bins[a]
                .0
                .partial_cmp(&bins[b].0)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(0);
    let (weight, sin, cos, ..) = bins[best];
    if weight <= 0.0 {
        // Nothing chromatic at all: report the mean colour, not vivid.
        let mean = small.pixels().fold((0.0, 0.0, 0.0), |a, p| {
            (a.0 + p[0] as f64, a.1 + p[1] as f64, a.2 + p[2] as f64)
        });
        let n = total * 255.0;
        let (hh, s, l) = rgb_to_hsl(mean.0 / n, mean.1 / n, mean.2 / n);
        return ColorSample {
            hex: hsl_to_hex(hh, s, l),
            hue: hh,
            saturation: s,
            lightness: l,
            vivid: false,
            version: SAMPLE_VERSION,
        };
    }
    let hue = (sin.atan2(cos).to_degrees() + 360.0) % 360.0;
    let (s, l) = (sums_s[best] / weight, sums_l[best] / weight);
    ColorSample {
        hex: hsl_to_hex(hue, s, l),
        hue,
        saturation: s,
        lightness: l,
        vivid,
        version: SAMPLE_VERSION,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};

    fn solid(r: u8, g: u8, b: u8) -> Vec<u8> {
        let img = RgbImage::from_pixel(120, 180, Rgb([r, g, b]));
        let mut out = Vec::new();
        DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .unwrap();
        out
    }

    /// A PNG whose header claims 40000 x 40000 pixels and has no pixel data at all.
    fn huge_declared() -> Vec<u8> {
        fn crc(bytes: &[u8]) -> u32 {
            let mut c = !0u32;
            for b in bytes {
                c ^= *b as u32;
                for _ in 0..8 {
                    c = if c & 1 == 1 {
                        (c >> 1) ^ 0xEDB8_8320
                    } else {
                        c >> 1
                    };
                }
            }
            !c
        }
        let mut ihdr = b"IHDR".to_vec();
        ihdr.extend(40_000u32.to_be_bytes());
        ihdr.extend(40_000u32.to_be_bytes());
        ihdr.extend([8, 2, 0, 0, 0]);
        let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
        out.extend(&ihdr);
        out.extend(crc(&ihdr).to_be_bytes());
        // an empty compressed stream and the end marker, so the file is well formed up to its pixels
        for (kind, body) in [
            (&b"IDAT"[..], &[0x78, 0x9c, 0x03, 0, 0, 0, 0, 1][..]),
            (&b"IEND"[..], &[][..]),
        ] {
            out.extend((body.len() as u32).to_be_bytes());
            let mut chunk = kind.to_vec();
            chunk.extend(body);
            out.extend(&chunk);
            out.extend(crc(&chunk).to_be_bytes());
        }
        out
    }

    #[test]
    fn a_picture_that_declares_a_huge_size_is_refused_before_it_is_decoded() {
        let e = thumbnail(&huge_declared()).err().expect("refused");
        assert!(e.contains("unreasonable dimensions"), "{e}");
    }

    #[test]
    fn a_red_cover_is_red_and_vivid() {
        let t = thumbnail(&solid(200, 40, 40)).unwrap();
        assert!(t.sample.vivid);
        assert!(
            t.sample.hue < 10.0 || t.sample.hue > 350.0,
            "{}",
            t.sample.hue
        );
        assert!(t.sample.hex.starts_with('#') && t.sample.hex.len() == 7);
        assert_eq!(t.sample.version, SAMPLE_VERSION);
    }

    #[test]
    fn a_teal_cover_is_teal() {
        let t = thumbnail(&solid(30, 160, 160)).unwrap();
        assert!((160.0..200.0).contains(&t.sample.hue), "{}", t.sample.hue);
    }

    #[test]
    fn grey_and_near_white_are_not_vivid() {
        assert!(!thumbnail(&solid(128, 128, 128)).unwrap().sample.vivid);
        assert!(!thumbnail(&solid(250, 250, 250)).unwrap().sample.vivid);
        assert!(!thumbnail(&solid(10, 10, 10)).unwrap().sample.vivid);
    }

    #[test]
    fn thumbnails_are_bounded_and_deterministic() {
        let big = {
            let img = RgbImage::from_pixel(1000, 1500, Rgb([20, 100, 200]));
            let mut out = Vec::new();
            DynamicImage::ImageRgb8(img)
                .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
                .unwrap();
            out
        };
        let a = thumbnail(&big).unwrap();
        let b = thumbnail(&big).unwrap();
        assert!(a.width <= MAX_W && a.height <= MAX_H);
        assert_eq!(a.sha256, b.sha256);
        assert_eq!(a.sample, b.sample);
        assert_eq!(a.sha256, sha256_hex(&a.jpeg));
    }

    fn hue_gap(a: f64, b: f64) -> f64 {
        let d = (a - b).rem_euclid(360.0);
        d.min(360.0 - d)
    }

    #[test]
    fn a_generated_cover_is_deterministic_vivid_and_the_right_shape() {
        let a = generated("The Lantern Keeper", "A Bardic sample");
        let b = generated("  the LANTERN keeper!", "a bardic sample");
        assert_eq!(a.sha256, b.sha256, "normalised title and author decide");
        assert_eq!(a.jpeg, b.jpeg);
        assert_eq!((a.width, a.height), (MAX_W, MAX_H));
        assert_eq!(a.width * 3, a.height * 2, "2:3");
        assert!(a.sample.vivid);
        assert_eq!(a.sha256, sha256_hex(&a.jpeg));
        let decoded = image::load_from_memory(&a.jpeg).unwrap();
        assert_eq!(decoded.dimensions(), (MAX_W, MAX_H));
    }

    #[test]
    fn generated_covers_are_vivid_and_near_their_hue_for_many_titles() {
        let mut shas = std::collections::HashSet::new();
        for i in 0..60 {
            let title = format!("Synthetic title number {i}");
            let g = generated(&title, "Nobody");
            assert!(g.sample.vivid, "{title}: {:?}", g.sample);
            assert!(g.sample.saturation > 0.4, "{title}: {:?}", g.sample);
            let gap = hue_gap(g.sample.hue, generated_hue(&title, "Nobody"));
            assert!(
                gap < 60.0,
                "{title}: measured {} wanted {}",
                g.sample.hue,
                generated_hue(&title, "Nobody")
            );
            shas.insert(g.sha256);
        }
        assert_eq!(shas.len(), 60);
    }

    #[test]
    fn junk_is_not_an_image() {
        assert!(thumbnail(b"not an image").is_err());
    }
}
