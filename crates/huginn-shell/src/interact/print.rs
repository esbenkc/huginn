//! Cmd+P: print the page to a PDF in the downloads folder. The page is laid
//! out again at the printable width of a US Letter page with Chrome's
//! default margins (0.4 in), cut into pages between lines of text, and each
//! page is rasterized at 2x by our own rasterizer and embedded as a
//! Flate-compressed image. Runs on the tab's engine thread (it owns the page).

use std::io::Write;
use std::path::{Path, PathBuf};

use huginn_layout::{LayoutEngine, LayoutTree, Rect};
use huginn_paint::{CpuRasterizer, Rasterizer};
use huginn_style::Viewport;

use crate::pipeline::Page;

/// US Letter in PDF points (1/72 in).
const PAGE_W_PT: f32 = 612.0;
const PAGE_H_PT: f32 = 792.0;
/// Chrome's default margins: 0.4 in.
const MARGIN_PT: f32 = 28.8;
/// CSS px per PDF point.
const PX_PER_PT: f32 = 96.0 / 72.0;
/// Raster scale: device px per CSS px (192 dpi).
const RASTER_SCALE: f32 = 2.0;

/// Printable area in CSS px.
pub fn content_size() -> (f32, f32) {
    (
        (PAGE_W_PT - 2.0 * MARGIN_PT) * PX_PER_PT,
        (PAGE_H_PT - 2.0 * MARGIN_PT) * PX_PER_PT,
    )
}

/// Lay `page` out for print, write the PDF, then restore the screen styles.
/// Returns the PDF bytes and the number of pages.
pub fn print_pdf(
    page: &mut Page,
    layout: &mut LayoutEngine,
    screen: Viewport,
    title: &str,
) -> (Vec<u8>, usize) {
    let (w, h) = content_size();
    let print = Viewport {
        width: w,
        height: h,
        device_pixel_ratio: RASTER_SCALE,
    };
    page.restyle(print);
    let tree = page.layout(layout, print);
    page.restyle(screen);
    let breaks = page_breaks(&tree, h);
    let mut raster = CpuRasterizer::new();
    let mut pdf = PdfWriter::new(title);
    for pair in breaks.windows(2) {
        let region = Rect {
            x: 0.0,
            y: pair[0],
            width: w,
            height: pair[1] - pair[0],
        };
        let list = huginn_paint::build_display_list(&tree, Some(region));
        let img = raster.rasterize(&list, region, RASTER_SCALE);
        let rgb: Vec<u8> = img
            .data
            .chunks_exact(4)
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect();
        pdf.page_with_image(img.width, img.height, &rgb, region.height / PX_PER_PT);
    }
    let pages = breaks.len() - 1;
    (pdf.finish(), pages)
}

/// Page boundaries (document y, CSS px): pages are at most `page_h` tall and
/// end above a line of text that would be cut, unless that leaves less than
/// half a page.
pub fn page_breaks(tree: &LayoutTree, page_h: f32) -> Vec<f32> {
    let total = tree.document_size.height.max(1.0);
    let mut out = vec![0.0];
    let mut top = 0.0f32;
    while top + 0.5 < total {
        let mut end = (top + page_h).min(total);
        if end < total
            && let Some(index) = &tree.text_index
        {
            // Move the break up above every line box it cuts.
            loop {
                let cut = index
                    .runs
                    .iter()
                    .filter(|r| !r.text.is_empty() && r.top < end && r.bottom > end)
                    .map(|r| r.top)
                    .fold(f32::INFINITY, f32::min);
                if !cut.is_finite() || cut <= top + page_h / 2.0 {
                    break;
                }
                end = cut;
            }
        }
        out.push(end);
        top = end;
    }
    out
}

/// A minimal PDF 1.4 writer: one full-bleed image per page.
struct PdfWriter {
    buf: Vec<u8>,
    offsets: Vec<usize>,
    pages: Vec<usize>,
    title: String,
}

impl PdfWriter {
    fn new(title: &str) -> PdfWriter {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n");
        // Objects 1 (catalog), 2 (page tree) and 3 (info) are written last.
        PdfWriter {
            buf,
            offsets: vec![0; 3],
            pages: Vec::new(),
            title: title.to_owned(),
        }
    }

    /// Start object `id` (1-based) at the current offset.
    fn begin(&mut self, id: usize) {
        if self.offsets.len() < id {
            self.offsets.resize(id, 0);
        }
        self.offsets[id - 1] = self.buf.len();
        let _ = write!(self.buf, "{id} 0 obj\n");
    }

    fn next_id(&self) -> usize {
        self.offsets.len() + 1
    }

    fn stream(&mut self, dict: &str, data: &[u8]) -> usize {
        let id = self.next_id();
        self.begin(id);
        let _ = write!(self.buf, "<< {dict} /Length {} >>\nstream\n", data.len());
        self.buf.extend_from_slice(data);
        self.buf.extend_from_slice(b"\nendstream\nendobj\n");
        id
    }

    /// A page showing an RGB image `w` x `h` px, `height_pt` tall, at the top
    /// margin, the printable width wide.
    fn page_with_image(&mut self, w: u32, h: u32, rgb: &[u8], height_pt: f32) {
        let data = miniz_oxide::deflate::compress_to_vec_zlib(rgb, 6);
        let img = self.stream(
            &format!("/Type /XObject /Subtype /Image /Width {w} /Height {h} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /FlateDecode"),
            &data,
        );
        let width_pt = PAGE_W_PT - 2.0 * MARGIN_PT;
        let y = PAGE_H_PT - MARGIN_PT - height_pt;
        let content =
            format!("q {width_pt:.3} 0 0 {height_pt:.3} {MARGIN_PT:.3} {y:.3} cm /Im0 Do Q\n");
        let contents = self.stream("", content.as_bytes());
        let id = self.next_id();
        self.begin(id);
        let _ = write!(
            self.buf,
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {PAGE_W_PT} {PAGE_H_PT}] \
             /Resources << /XObject << /Im0 {img} 0 R >> >> /Contents {contents} 0 R >>\nendobj\n"
        );
        self.pages.push(id);
    }

    fn finish(mut self) -> Vec<u8> {
        self.begin(1);
        self.buf
            .extend_from_slice(b"<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
        self.begin(2);
        let kids: Vec<String> = self.pages.iter().map(|p| format!("{p} 0 R")).collect();
        let _ = write!(
            self.buf,
            "<< /Type /Pages /Kids [{}] /Count {} >>\nendobj\n",
            kids.join(" "),
            self.pages.len()
        );
        self.begin(3);
        let _ = write!(
            self.buf,
            "<< /Title {} /Producer (Huginn) >>\nendobj\n",
            pdf_text(&self.title)
        );
        let xref = self.buf.len();
        let _ = write!(
            self.buf,
            "xref\n0 {}\n0000000000 65535 f \n",
            self.offsets.len() + 1
        );
        for &o in &self.offsets {
            let _ = write!(self.buf, "{o:010} 00000 n \n");
        }
        let _ = write!(
            self.buf,
            "trailer\n<< /Size {} /Root 1 0 R /Info 3 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            self.offsets.len() + 1
        );
        self.buf
    }
}

/// A PDF text string: UTF-16BE with a byte order mark, as a hex string.
fn pdf_text(s: &str) -> String {
    let mut out = String::from("<FEFF");
    for u in s.encode_utf16() {
        out.push_str(&format!("{u:04X}"));
    }
    out.push('>');
    out
}

/// Where downloads go: `$HUGINN_DOWNLOADS` (tests), else `~/Downloads`.
pub fn downloads_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("HUGINN_DOWNLOADS") {
        return PathBuf::from(d);
    }
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join("Downloads")
}

/// A file name from a page title (or URL): no path separators or control
/// characters, at most 100 characters.
pub fn file_stem(title: &str, fallback: &str) -> String {
    let src = if title.trim().is_empty() {
        fallback
    } else {
        title
    };
    let s: String = src
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .take(100)
        .collect();
    let s = s.trim().trim_start_matches('.').to_owned();
    if s.is_empty() { "page".into() } else { s }
}

/// `dir/stem.ext`, or `dir/stem (2).ext` and so on if it exists.
pub fn unique_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let mut p = dir.join(format!("{stem}.{ext}"));
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{stem} ({n}).{ext}"));
        n += 1;
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_has_pages_and_a_valid_xref() {
        let mut w = PdfWriter::new("Tést");
        w.page_with_image(2, 1, &[255, 0, 0, 0, 0, 255], 100.0);
        w.page_with_image(1, 1, &[0, 255, 0], 50.0);
        let pdf = w.finish();
        let find = |hay: &[u8], pat: &[u8]| hay.windows(pat.len()).rposition(|w| w == pat);
        assert!(pdf.starts_with(b"%PDF-1.4"));
        assert!(find(&pdf, b"/Count 2").is_some());
        assert!(pdf.ends_with(b"%%EOF\n"));
        // startxref points at the table, and every entry at "<id> 0 obj".
        let xref = find(&pdf, b"\nxref\n").unwrap() + 1;
        let tail = std::str::from_utf8(&pdf[xref..]).unwrap();
        let startxref: usize = tail[tail.find("startxref\n").unwrap() + 10..]
            .lines()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(startxref, xref);
        let entries: Vec<&str> = tail
            .lines()
            .skip(3)
            .take_while(|l| l.ends_with(" n "))
            .collect();
        assert_eq!(
            entries.len(),
            3 + 2 * 3,
            "catalog, pages, info + image, contents, page per page"
        );
        for (i, line) in entries.iter().enumerate() {
            let off: usize = line[..10].parse().unwrap();
            assert!(
                pdf[off..].starts_with(format!("{} 0 obj", i + 1).as_bytes()),
                "object {}",
                i + 1
            );
        }
        assert!(find(&pdf, b"/Title <FEFF005400E900730074>").is_some());
    }

    #[test]
    fn file_names_are_safe_and_unique() {
        assert_eq!(file_stem("a/b: c?", "x"), "a_b_ c_");
        assert_eq!(file_stem("  ", "example.com"), "example.com");
        assert_eq!(file_stem("..hidden", "x"), "hidden");
        let dir = std::env::temp_dir().join(format!("huginn-print-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = unique_path(&dir, "p", "pdf");
        std::fs::write(&a, b"x").unwrap();
        let b = unique_path(&dir, "p", "pdf");
        assert_eq!(b.file_name().unwrap(), "p (2).pdf");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
