//! Renders a page in-process (parse, style, layout, paint) at 400x300 CSS px
//! and scale 1, for pixel checks.

use huginn_layout::{LayoutEngine, LayoutInput, Rect, Size};
use huginn_net::{ImageStore, Url};
use huginn_paint::{CpuRasterizer, Rasterizer, RgbaImage};
use huginn_style::{NoImports, Origin, StyleEngine, Viewport};

const W: f32 = 400.0;
const H: f32 = 300.0;

pub fn render(html: &str) -> RgbaImage {
    let doc = huginn_dom::parse_html(html);
    let mut engine = StyleEngine::new(Viewport {
        width: W,
        height: H,
        device_pixel_ratio: 1.0,
    });
    // Author <style> sheets.
    for n in doc.elements_by_local_name(&huginn_dom::local_name!("style")) {
        engine.add_stylesheet(
            &doc.text_content(n),
            "about:blank",
            Origin::Author,
            &NoImports,
        );
    }
    let styles = engine.compute(&doc);
    let images = ImageStore::new();
    let base = Url::parse("about:blank").unwrap();
    let tree = LayoutEngine::new().layout(&LayoutInput {
        document: &doc,
        styles: &styles,
        images: &images,
        base_url: &base,
        viewport: Size {
            width: W,
            height: H,
        },
    });
    let region = Rect {
        x: 0.0,
        y: 0.0,
        width: W,
        height: H,
    };
    let list = huginn_paint::build_display_list(&tree, Some(region));
    CpuRasterizer::new().rasterize(&list, region, 1.0)
}

pub fn px(img: &RgbaImage, x: u32, y: u32) -> [u8; 3] {
    let i = ((y * img.width + x) * 4) as usize;
    [img.data[i], img.data[i + 1], img.data[i + 2]]
}
