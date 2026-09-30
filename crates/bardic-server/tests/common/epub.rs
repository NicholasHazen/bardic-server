//! Builds small synthetic EPUBs for tests. All text here is original.
use std::io::{Cursor, Write};
use zip::{write::SimpleFileOptions, ZipWriter};

pub fn png(r: u8, g: u8, b: u8) -> Vec<u8> {
    let img = image::RgbImage::from_pixel(60, 90, image::Rgb([r, g, b]));
    let mut out = Vec::new();
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
        .unwrap();
    out
}

pub struct Epub {
    pub title: &'static str,
    pub author: &'static str,
    pub cover: Option<Vec<u8>>,
    pub drm: bool,
    pub chapters: Vec<(&'static str, &'static str)>,
}

impl Default for Epub {
    fn default() -> Self {
        Epub {
            title: "The Test Ferry",
            author: "A. Writer",
            cover: Some(png(200, 40, 40)),
            drm: false,
            chapters: vec![
                (
                    "The Crossing",
                    "The ferry left at dusk. \"Wait,\" she said.",
                ),
                ("The Far Shore", "Nobody was there. The lantern swung."),
            ],
        }
    }
}

impl Epub {
    pub fn build(&self) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut z = ZipWriter::new(Cursor::new(&mut out));
            let o = SimpleFileOptions::default();
            z.start_file("mimetype", o).unwrap();
            z.write_all(b"application/epub+zip").unwrap();
            z.start_file("META-INF/container.xml", o).unwrap();
            z.write_all(br#"<?xml version="1.0"?><container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#).unwrap();
            if self.drm {
                z.start_file("META-INF/encryption.xml", o).unwrap();
                z.write_all(br#"<encryption xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><EncryptedData xmlns="http://www.w3.org/2001/04/xmlenc#"><EncryptionMethod Algorithm="http://www.w3.org/2001/04/xmlenc#aes128-cbc"/></EncryptedData></encryption>"#).unwrap();
            }
            let items: String = (0..self.chapters.len())
                .map(|i| {
                    format!(
                        r#"<item id="c{i}" href="c{i}.xhtml" media-type="application/xhtml+xml"/>"#
                    )
                })
                .collect();
            let refs: String = (0..self.chapters.len())
                .map(|i| format!(r#"<itemref idref="c{i}"/>"#))
                .collect();
            let cover_meta = if self.cover.is_some() {
                r#"<meta name="cover" content="cov"/>"#
            } else {
                ""
            };
            let cover_item = if self.cover.is_some() {
                r#"<item id="cov" href="cover.png" media-type="image/png"/>"#
            } else {
                ""
            };
            z.start_file("OEBPS/content.opf", o).unwrap();
            z.write_all(format!(r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>{}</dc:title><dc:creator>{}</dc:creator>{cover_meta}</metadata><manifest>{items}{cover_item}</manifest><spine>{refs}</spine></package>"#, self.title, self.author).as_bytes()).unwrap();
            for (i, (t, body)) in self.chapters.iter().enumerate() {
                z.start_file(format!("OEBPS/c{i}.xhtml"), o).unwrap();
                z.write_all(
                    format!("<html><body><h1>{t}</h1><p>{body}</p></body></html>").as_bytes(),
                )
                .unwrap();
            }
            if let Some(c) = &self.cover {
                z.start_file("OEBPS/cover.png", o).unwrap();
                z.write_all(c).unwrap();
            }
            z.finish().unwrap();
        }
        out
    }
}
