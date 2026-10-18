//! The file panel's rendered mode: what a file looks like rather than what it
//! says. Markdown lays out as styled text ([`super::markdown`]); pictures,
//! SVG drawings and PDF pages become RGBA pixels the reader places on its
//! grid as one image.
//!
//! Everything here runs on the loader's threads. A picture always comes as a
//! [`RASTER`] square with the picture in its top left corner, so the grid can
//! keep one atlas allocation per panel and overwrite it for the next picture.

use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use super::document::{Document, Loaded, MAX_BYTES};
use super::markdown::{Laid, Markdown};

/// Side of the square every picture is drawn into, in pixels.
pub const RASTER: u32 = 1024;
/// Pictures and PDFs are read up to this many bytes.
pub const MAX_MEDIA_BYTES: u64 = 64 * 1024 * 1024;

/// A picture, ready for the grid.
pub struct Picture {
    /// [`RASTER`] × [`RASTER`] RGBA pixels, not premultiplied. The picture
    /// fills the top left `width` × `height` of them; the rest is clear.
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// The picture's own size: pixels for a raster image, points for a PDF
    /// page, user units for SVG.
    pub natural: (f32, f32),
    /// Vector pictures grow to fill the reader; raster ones stop at their
    /// natural size.
    pub scalable: bool,
    /// Tells pictures apart, so the grid uploads each one once.
    pub serial: u64,
}

/// A PDF file and the page the reader shows.
pub struct Pdf {
    pub bytes: Arc<Vec<u8>>,
    pub pages: usize,
    pub page: usize,
    /// The drawn page; it lags `page` while a new one renders.
    pub picture: Option<Picture>,
    /// The page a loader thread is rendering.
    pub rendering: Option<usize>,
}

pub enum Rendered {
    Markdown {
        markdown: Markdown,
        laid: Laid,
    },
    /// A raster image or an SVG drawing, with its format's name.
    Picture {
        picture: Picture,
        format: &'static str,
    },
    Pdf(Pdf),
}

/// What reading a file produced: its text, its rendered form, or both.
pub struct Opened {
    /// Relative to the worktree root.
    pub path: PathBuf,
    pub source: Option<Document>,
    pub rendered: Option<Rendered>,
    /// Why the file has no rendered form although its kind has one.
    pub error: Option<String>,
    pub size: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Markdown,
    Svg,
    Pdf,
    Image(image::ImageFormat),
}

fn kind(path: &Path, bytes: &[u8]) -> Option<Kind> {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "md" | "markdown" | "mkd" | "mdown" => return Some(Kind::Markdown),
        "svg" | "svgz" => return Some(Kind::Svg),
        "pdf" => return Some(Kind::Pdf),
        _ => {}
    }
    if bytes.starts_with(b"%PDF-") {
        return Some(Kind::Pdf);
    }
    image::guess_format(bytes)
        .ok()
        .or_else(|| image::ImageFormat::from_extension(&ext))
        .filter(|format| format.reading_enabled())
        .map(Kind::Image)
}

/// The byte limit to read `path` with: larger for pictures and PDFs, which
/// are useless cut off.
pub fn read_limit(path: &Path) -> u64 {
    match kind(path, &[]) {
        Some(Kind::Pdf | Kind::Image(_)) => MAX_MEDIA_BYTES + 1,
        _ => MAX_BYTES + 1,
    }
}

/// Prepares `bytes`, read from `path` with [`read_limit`]. Notes in the
/// rendered form are in the language of `tr`.
pub fn open(path: PathBuf, bytes: Vec<u8>, tr: &'static crate::i18n::Strings) -> Opened {
    let size = bytes.len();
    let mut opened = Opened {
        path,
        source: None,
        rendered: None,
        error: None,
        size,
    };
    let kind = kind(&opened.path, &bytes);
    let media = matches!(kind, Some(Kind::Pdf | Kind::Image(_)));
    if media && size as u64 > MAX_MEDIA_BYTES {
        opened.error = Some(format!("larger than {} MiB", MAX_MEDIA_BYTES >> 20));
        return opened;
    }
    let rendered = match kind {
        Some(Kind::Markdown) => {
            let end = bytes.len().min(MAX_BYTES as usize);
            let markdown = Markdown::parse_in(&String::from_utf8_lossy(&bytes[..end]), tr);
            let laid = markdown.layout(0);
            Some(Ok(Rendered::Markdown { markdown, laid }))
        }
        Some(Kind::Svg) => Some(guard(|| svg(&bytes)).map(|picture| Rendered::Picture {
            picture,
            format: "SVG",
        })),
        Some(Kind::Image(format)) => {
            Some(
                guard(|| raster(&bytes, format)).map(|picture| Rendered::Picture {
                    picture,
                    format: format_name(format),
                }),
            )
        }
        Some(Kind::Pdf) => {
            let bytes = Arc::new(bytes);
            let rendered = guard(|| {
                let pages = page_count(&bytes)?;
                let picture = render_page(&bytes, 0)?;
                Ok(Rendered::Pdf(Pdf {
                    bytes: bytes.clone(),
                    pages,
                    page: 0,
                    picture: Some(picture),
                    rendering: None,
                }))
            });
            match rendered {
                Ok(rendered) => opened.rendered = Some(rendered),
                Err(error) => opened.error = Some(error),
            }
            return opened;
        }
        None => None,
    };
    if !media {
        // SVG and Markdown are text too, so the source view can show them.
        match Loaded::new(&opened.path, bytes) {
            Loaded::Text(document) => opened.source = Some(document),
            Loaded::Binary => {}
        }
    }
    match rendered {
        Some(Ok(rendered)) => opened.rendered = Some(rendered),
        Some(Err(error)) => opened.error = Some(error),
        None => {}
    }
    opened
}

/// Runs a decoder, turning a panic inside it into an error: a broken file
/// must not take the loader thread down with it.
fn guard<T>(job: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    panic::catch_unwind(AssertUnwindSafe(job))
        .unwrap_or_else(|_| Err("the decoder failed on this file".to_owned()))
}

fn format_name(format: image::ImageFormat) -> &'static str {
    use image::ImageFormat as F;
    match format {
        F::Png => "PNG",
        F::Jpeg => "JPEG",
        F::Gif => "GIF",
        F::WebP => "WebP",
        F::Bmp => "BMP",
        F::Ico => "ICO",
        _ => "Image",
    }
}

fn next_serial() -> u64 {
    static SERIAL: AtomicU64 = AtomicU64::new(1);
    SERIAL.fetch_add(1, Ordering::Relaxed)
}

/// Copies `width` × `height` RGBA pixels into the top left of a clear
/// [`RASTER`] square.
fn pad(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    let side = RASTER as usize;
    let mut out = vec![0; side * side * 4];
    let row = width as usize * 4;
    for y in 0..height as usize {
        out[y * side * 4..y * side * 4 + row].copy_from_slice(&rgba[y * row..(y + 1) * row]);
    }
    out
}

fn raster(bytes: &[u8], format: image::ImageFormat) -> Result<Picture, String> {
    let image = image::load_from_memory_with_format(bytes, format).map_err(|e| e.to_string())?;
    let natural = (image.width() as f32, image.height() as f32);
    let image = if image.width() > RASTER || image.height() > RASTER {
        image.thumbnail(RASTER, RASTER)
    } else {
        image
    };
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    Ok(Picture {
        rgba: pad(rgba.as_raw(), width, height),
        width,
        height,
        natural,
        scalable: false,
        serial: next_serial(),
    })
}

/// The installed fonts, for text in SVG drawings. Loaded once, on the first
/// SVG.
fn fonts() -> Arc<resvg::usvg::fontdb::Database> {
    static FONTS: OnceLock<Arc<resvg::usvg::fontdb::Database>> = OnceLock::new();
    FONTS
        .get_or_init(|| {
            let mut fonts = resvg::usvg::fontdb::Database::new();
            fonts.load_system_fonts();
            Arc::new(fonts)
        })
        .clone()
}

fn svg(bytes: &[u8]) -> Result<Picture, String> {
    use resvg::{tiny_skia, usvg};
    let options = usvg::Options {
        fontdb: fonts(),
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_data(bytes, &options).map_err(|e| e.to_string())?;
    let size = tree.size();
    let (w, h) = (size.width(), size.height());
    let scale = RASTER as f32 / w.max(h);
    let width = ((w * scale).round() as u32).clamp(1, RASTER);
    let height = ((h * scale).round() as u32).clamp(1, RASTER);
    let mut pixmap = tiny_skia::Pixmap::new(width, height).ok_or("empty drawing")?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for pixel in pixmap.pixels() {
        let color = pixel.demultiply();
        rgba.extend_from_slice(&[color.red(), color.green(), color.blue(), color.alpha()]);
    }
    Ok(Picture {
        rgba: pad(&rgba, width, height),
        width,
        height,
        natural: (w, h),
        scalable: true,
        serial: next_serial(),
    })
}

fn parse_pdf(bytes: &Arc<Vec<u8>>) -> Result<hayro::hayro_syntax::Pdf, String> {
    hayro::hayro_syntax::Pdf::new(bytes.clone()).map_err(|error| match error {
        hayro::hayro_syntax::LoadPdfError::Decryption(_) => "the PDF is encrypted".to_owned(),
        hayro::hayro_syntax::LoadPdfError::Invalid => "not a valid PDF".to_owned(),
    })
}

fn page_count(bytes: &Arc<Vec<u8>>) -> Result<usize, String> {
    let pdf = parse_pdf(bytes)?;
    match pdf.pages().len() {
        0 => Err("the PDF has no pages".to_owned()),
        pages => Ok(pages),
    }
}

/// Draws page `page` (from zero) of a PDF on white, as large as fits the
/// raster.
pub fn render_page(bytes: &Arc<Vec<u8>>, page: usize) -> Result<Picture, String> {
    guard(|| {
        let pdf = parse_pdf(bytes)?;
        let page = pdf.pages().get(page).ok_or("no such page")?;
        let (w, h) = page.render_dimensions();
        if w <= 0.0 || h <= 0.0 {
            return Err("the page is empty".to_owned());
        }
        let scale = (RASTER as f32 - 1.0) / w.max(h);
        let pixmap = hayro::render(
            page,
            &hayro::RenderCache::new(),
            &hayro::hayro_interpret::InterpreterSettings::default(),
            &hayro::RenderSettings::default(),
            &hayro::PixmapSettings {
                x_scale: scale,
                y_scale: scale,
                bg_color: hayro::vello_cpu::color::palette::css::WHITE,
            },
        );
        let (width, height) = (u32::from(pixmap.width()), u32::from(pixmap.height()));
        if width == 0 || height == 0 || width > RASTER || height > RASTER {
            return Err("the page is empty".to_owned());
        }
        // The page sits on opaque white, so premultiplied pixels are the
        // straight ones.
        Ok(Picture {
            rgba: pad(pixmap.data_as_u8_slice(), width, height),
            width,
            height,
            natural: (w, h),
            scalable: true,
            serial: next_serial(),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_come_from_names_and_magic_bytes() {
        assert_eq!(kind(Path::new("README.md"), b"# hi"), Some(Kind::Markdown));
        assert_eq!(kind(Path::new("logo.SVG"), b"<svg"), Some(Kind::Svg));
        assert_eq!(kind(Path::new("paper"), b"%PDF-1.7\n"), Some(Kind::Pdf));
        assert_eq!(
            kind(Path::new("photo"), b"\x89PNG\r\n\x1a\n...."),
            Some(Kind::Image(image::ImageFormat::Png))
        );
        assert_eq!(kind(Path::new("main.rs"), b"fn main() {}"), None);
        assert_eq!(read_limit(Path::new("a.pdf")), MAX_MEDIA_BYTES + 1);
        assert_eq!(read_limit(Path::new("a.rs")), MAX_BYTES + 1);
    }

    #[test]
    fn pictures_fill_the_top_left_of_the_raster() {
        let mut png = Vec::new();
        let image = image::RgbaImage::from_pixel(3, 2, image::Rgba([255, 0, 0, 255]));
        image
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let opened = open(PathBuf::from("dot.png"), png, &crate::i18n::EN);
        assert!(opened.source.is_none());
        let Some(Rendered::Picture { picture, format }) = opened.rendered else {
            panic!("no picture: {:?}", opened.error);
        };
        assert_eq!(format, "PNG");
        assert_eq!((picture.width, picture.height), (3, 2));
        assert_eq!(picture.natural, (3.0, 2.0));
        assert_eq!(picture.rgba.len(), (RASTER * RASTER * 4) as usize);
        assert_eq!(&picture.rgba[..4], &[255, 0, 0, 255]);
        assert_eq!(&picture.rgba[12..16], &[0, 0, 0, 0]);
        let row = (RASTER * 4) as usize;
        assert_eq!(&picture.rgba[row..row + 4], &[255, 0, 0, 255]);
    }

    #[test]
    fn svg_keeps_its_source() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10"><rect width="20" height="10" fill="blue"/></svg>"#;
        let opened = open(PathBuf::from("box.svg"), svg.to_vec(), &crate::i18n::EN);
        assert!(opened.source.is_some());
        let Some(Rendered::Picture { picture, .. }) = opened.rendered else {
            panic!("no picture: {:?}", opened.error);
        };
        assert_eq!((picture.width, picture.height), (RASTER, RASTER / 2));
        assert_eq!(&picture.rgba[..4], &[0, 0, 255, 255]);
    }

    #[test]
    fn broken_media_reports_why() {
        let opened = open(
            PathBuf::from("bad.png"),
            b"\x89PNG\r\n\x1a\nbroken".to_vec(),
            &crate::i18n::EN,
        );
        assert!(opened.rendered.is_none());
        assert!(opened.error.is_some());
        let opened = open(
            PathBuf::from("bad.pdf"),
            b"%PDF-1.4 nothing".to_vec(),
            &crate::i18n::EN,
        );
        assert!(opened.rendered.is_none());
        assert!(opened.error.is_some());
    }
}
