//! Renders every page in `fixtures/` with the real `huginn` binary and checks
//! the contract: exit 0, one JSON line with sane phase timings, and a PNG of
//! the requested size that actually has content on it.

use std::path::{Path, PathBuf};
use std::process::Command;

const PHASES: [&str; 4] = ["parse_ms", "style_ms", "layout_ms", "paint_ms"];

fn fixtures() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    let mut pages: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "html"))
        .collect();
    pages.sort();
    assert!(!pages.is_empty(), "no fixtures in {}", dir.display());
    pages
}

/// A fresh output directory per test (tests run in parallel).
fn out_dir(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("huginn-{test}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Decode a PNG into (width, height, RGBA bytes).
fn decode_png(path: &Path) -> (u32, u32, Vec<u8>) {
    let file = std::fs::File::open(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut reader = png::Decoder::new(std::io::BufReader::new(file))
        .read_info()
        .expect("valid PNG header");
    let mut buf = vec![0; reader.output_buffer_size().expect("buffer size")];
    let info = reader.next_frame(&mut buf).expect("valid PNG data");
    assert_eq!(info.color_type, png::ColorType::Rgba, "{}", path.display());
    buf.truncate(info.buffer_size());
    (info.width, info.height, buf)
}

fn render(page: &Path, out: &Path, width: u32, height: u32) -> serde_json::Value {
    let output = Command::new(env!("CARGO_BIN_EXE_huginn"))
        .arg("render")
        .arg(page)
        .args(["--out", out.to_str().expect("utf-8 path")])
        .args([
            "--width",
            &width.to_string(),
            "--height",
            &height.to_string(),
        ])
        .output()
        .expect("run huginn");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{}: exit {:?}\nstderr: {}",
        page.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "{}: expected one JSON line, got {stdout:?}",
        page.display()
    );
    serde_json::from_str(lines[0])
        .unwrap_or_else(|e| panic!("{}: bad JSON {:?}: {e}", page.display(), lines[0]))
}

#[test]
fn every_fixture_renders_with_valid_timings_and_pixels() {
    let dir = out_dir("fixtures");
    for page in fixtures() {
        let name = page.file_stem().unwrap().to_string_lossy().into_owned();
        let png_path = dir.join(format!("{name}.png"));
        let report = render(&page, &png_path, 1280, 800);

        // Timing JSON: url, node count, every phase a finite non-negative
        // number, and the total covers each phase.
        let url = report["url"]
            .as_str()
            .unwrap_or_else(|| panic!("{name}: no url in {report}"));
        assert!(
            url.starts_with("file://") && url.ends_with(".html"),
            "{name}: url {url}"
        );
        assert!(
            report["nodes"].as_u64().is_some_and(|n| n > 1),
            "{name}: nodes in {report}"
        );
        let total = report["total_ms"]
            .as_f64()
            .unwrap_or_else(|| panic!("{name}: no total_ms in {report}"));
        for key in PHASES {
            let v = report[key]
                .as_f64()
                .unwrap_or_else(|| panic!("{name}: no {key} in {report}"));
            assert!(v.is_finite() && v >= 0.0, "{name}: {key} = {v}");
            assert!(v <= total, "{name}: {key} = {v} exceeds total_ms = {total}");
        }

        // PNG: requested size, and not a blank canvas.
        let (w, h, rgba) = decode_png(&png_path);
        assert_eq!((w, h), (1280, 800), "{name}: PNG size");
        let first = &rgba[..4];
        assert!(
            rgba.chunks_exact(4).any(|px| px != first),
            "{name}: PNG is a single flat color"
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn viewport_size_sets_png_size() {
    let dir = out_dir("viewport");
    let page = fixtures().into_iter().next().expect("a fixture");
    let png_path = dir.join("small.png");
    render(&page, &png_path, 400, 300);
    let (w, h, _) = decode_png(&png_path);
    assert_eq!((w, h), (400, 300));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn svg_inline_image_and_background_are_drawn() {
    let dir = out_dir("svg");
    let page = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/svg.html");
    let png_path = dir.join("svg.png");
    render(&page, &png_path, 800, 300);
    let (w, _, rgba) = decode_png(&png_path);
    let px = |x: u32, y: u32| {
        let i = ((y * w + x) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
    };
    // Inline <svg> filled with currentColor (the CSS color, red).
    assert_eq!(px(125, 125), [255, 0, 0, 255], "inline svg");
    // <img src=*.svg>, 10x10 natural size drawn at 50x50.
    assert_eq!(px(325, 125), [0, 0, 255, 255], "svg <img>");
    assert_eq!(px(345, 145), [0, 0, 255, 255], "svg <img> fills its box");
    // CSS background: a 50x50 green circle; the corner stays white.
    assert_eq!(px(525, 125), [0, 255, 0, 255], "svg background");
    assert_eq!(px(502, 102), [255, 255, 255, 255], "outside the circle");
    // A 4:1 SVG with `height: 20px; width: auto` is 80 px wide, filled edge to edge.
    assert_eq!(px(2, 210), [255, 0, 255, 255], "wide svg left edge");
    assert_eq!(px(77, 210), [255, 0, 255, 255], "wide svg right edge");
    assert_eq!(px(84, 210), [255, 255, 255, 255], "wide svg ends at 80 px");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn video_poster_is_drawn_at_its_size_and_contained() {
    let dir = out_dir("video");
    let page = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/video.html");
    let png_path = dir.join("video.png");
    render(&page, &png_path, 800, 300);
    let (w, _, rgba) = decode_png(&png_path);
    let px = |x: u32, y: u32| {
        let i = ((y * w + x) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
    };
    let (magenta, white) = ([255, 0, 255, 255], [255, 255, 255, 255]);
    // 100x100 video, 4:1 poster: object-fit contain draws it 100x25 at y 137.5.
    assert_eq!(px(50, 150), magenta, "contained poster center");
    assert_eq!(px(2, 140), magenta, "contained poster left edge");
    assert_eq!(px(97, 160), magenta, "contained poster right edge");
    assert_eq!(px(50, 110), white, "letterbox above");
    assert_eq!(px(50, 190), white, "letterbox below");
    // No size: the poster's natural 40x10.
    assert_eq!(px(237, 105), magenta, "natural-size poster");
    assert_eq!(px(243, 105), white, "natural width ends at 40 px");
    assert_eq!(px(220, 112), white, "natural height ends at 10 px");
    // width=80 alone: height 20 from the poster's ratio.
    assert_eq!(px(377, 117), magenta, "width-only poster");
    assert_eq!(px(340, 123), white, "width-only height ends at 20 px");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn script_scrolls_an_element_and_its_sticky_header_sticks() {
    let dir = out_dir("scroll");
    let page = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/scroll.html");
    let png_path = dir.join("scroll.png");
    render(&page, &png_path, 400, 200);
    let (w, _, rgba) = decode_png(&png_path);
    let px = |x: u32, y: u32| {
        let i = ((y * w + x) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
    };
    // scrollTop, scrollHeight, offsetTop and getBoundingClientRect agree (blue, not red).
    assert_eq!(px(25, 125), [0, 0, 255, 255], "script checks");
    // Scrolled by 100px: the sticky header covers the row under it, the
    // current row is 40px down, the last visible row is the blue one.
    assert_eq!(
        px(100, 10),
        [255, 255, 0, 255],
        "sticky header at the top of the scrollport"
    );
    assert_eq!(px(100, 30), [0, 255, 136, 255], "row 6");
    assert_eq!(px(100, 50), [0, 255, 255, 255], "current row");
    assert_eq!(px(100, 90), [0, 0, 255, 255], "row 9");
    std::fs::remove_dir_all(&dir).ok();
}
