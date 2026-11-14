//! `filter`, `backdrop-filter`, `mix-blend-mode` and `isolation`, checked on
//! pixels against Chrome: each expected value is what headless Chrome draws
//! for the same page (400x300, scale 1, sRGB).

mod common;

use common::{px, render};
use huginn_paint::RgbaImage;

const RESET: &str = "<style>body{margin:0} div{box-sizing:border-box}</style>";

/// Every channel within `tol` of Chrome's.
fn near(img: &RgbaImage, (x, y): (u32, u32), want: [u8; 3], tol: u8, what: &str) {
    let got = px(img, x, y);
    let off = got
        .iter()
        .zip(want)
        .map(|(&g, w)| g.abs_diff(w))
        .max()
        .unwrap_or(0);
    assert!(off <= tol, "{what} at ({x}, {y}): {got:?}, Chrome {want:?}");
}

/// A 40x40 orange box at (x, y) with extra style.
fn swatch(x: u32, y: u32, css: &str) -> String {
    format!(
        "<div style='position:absolute;left:{x}px;top:{y}px;width:40px;height:40px;\
         background:rgb(200,100,50);{css}'></div>"
    )
}

#[test]
fn color_functions_match_chrome() {
    let cases = [
        ((0, 0), "filter:brightness(1.5)", [255, 150, 75]),
        ((50, 0), "filter:contrast(2)", [255, 73, 0]),
        ((100, 0), "filter:grayscale(1)", [118, 118, 118]),
        ((150, 0), "filter:hue-rotate(90deg)", [50, 146, 35]),
        ((200, 0), "filter:invert(0.75)", [91, 141, 166]),
        ((250, 0), "filter:opacity(0.5)", [227, 177, 152]),
        ((300, 0), "filter:saturate(3)", [255, 65, 0]),
        ((350, 0), "filter:sepia(1)", [165, 147, 114]),
        // Applied left to right, clamped after each function.
        (
            (0, 50),
            "filter:sepia(20%) hue-rotate(60deg) brightness(90%) contrast(110%)",
            [92, 124, 20],
        ),
        (
            (50, 50),
            "filter:grayscale(0.5) brightness(1.08)",
            [172, 118, 91],
        ),
        ((100, 50), "filter:blur(0px)", [200, 100, 50]),
    ];
    let html: String = cases
        .iter()
        .map(|&((x, y), css, _)| swatch(x, y, css))
        .collect();
    let img = render(&format!("{RESET}{html}"));
    for ((x, y), css, want) in cases {
        near(&img, (x + 20, y + 20), want, 1, css);
    }
}

#[test]
fn blur_spreads_three_standard_deviations_past_the_box() {
    let img = render(&format!(
        "{RESET}<div style='position:absolute;left:100px;top:100px;width:60px;height:60px;\
         background:#000;filter:blur(8px)'></div>\
         <div style='position:absolute;left:250px;top:100px;width:60px;height:60px;\
         background:#00f;filter:blur(1.5px)'></div>"
    ));
    let gray = |v: u8| [v, v, v];
    near(&img, (130, 130), gray(0), 2, "center");
    near(&img, (100, 130), gray(121), 2, "left edge");
    near(&img, (130, 100), gray(121), 2, "top edge");
    near(&img, (92, 130), gray(211), 3, "one sigma out");
    near(&img, (84, 130), gray(249), 3, "two sigma out");
    near(&img, (76, 130), gray(255), 0, "three sigma out");
    // A small blur uses a true Gaussian kernel.
    near(&img, (250, 130), [94, 94, 255], 2, "small blur edge");
    near(&img, (248, 130), [216, 216, 255], 2, "small blur outside");
    near(&img, (252, 130), [11, 11, 255], 2, "small blur inside");
}

#[test]
fn drop_shadow_follows_the_alpha_of_the_content() {
    let img = render(&format!(
        "{RESET}<div style='position:absolute;left:50px;top:50px;width:80px;height:80px;\
         border:10px solid #00f;filter:drop-shadow(20px 20px 0 red)'></div>\
         <div style='position:absolute;left:220px;top:50px;width:60px;height:60px;\
         background:#000;filter:drop-shadow(0 30px 10px rgba(0,0,255,.5))'></div>"
    ));
    let (red, white) = ([255, 0, 0], [255, 255, 255]);
    near(&img, (55, 90), [0, 0, 255], 0, "the ring itself");
    near(&img, (90, 90), white, 0, "the hole");
    near(&img, (75, 145), red, 0, "shadow of the ring");
    near(
        &img,
        (90, 100),
        white,
        0,
        "no shadow inside the ring's hole",
    );
    near(&img, (140, 90), red, 0, "shadow past the box");
    // The blur length is a standard deviation, as in blur().
    near(
        &img,
        (250, 125),
        [136, 136, 255],
        3,
        "blurred shadow inside",
    );
    near(&img, (250, 140), [194, 194, 255], 3, "blurred shadow edge");
    near(
        &img,
        (250, 150),
        [236, 236, 255],
        3,
        "blurred shadow outside",
    );
}

#[test]
fn mix_blend_mode_blends_with_what_its_parent_context_painted() {
    let img = render(&format!(
        "{RESET}<style>.a {{ position: absolute }}</style>\
         <div class=a style='left:0;top:0;width:400px;height:100px;background:#000'></div>\
         <div class=a style='left:0;top:100px;width:400px;height:100px;background:rgb(255,200,0)'></div>\
         <div class=a style='left:50px;top:50px;width:100px;height:100px;background:#fff;\
         mix-blend-mode:difference'></div>\
         <div class=a style='left:200px;top:50px;width:100px;height:100px;\
         background:rgb(0,128,255);mix-blend-mode:multiply'></div>\
         <div class=a style='left:0;top:220px;width:400px;height:80px;background:red'></div>\
         <div class=a style='left:20px;top:230px;width:150px;height:60px;isolation:isolate'>\
         <div style='width:60px;height:60px;background:#fff;mix-blend-mode:difference'></div></div>\
         <div class=a style='left:220px;top:230px;width:150px;height:60px'>\
         <div style='width:60px;height:60px;background:#fff;mix-blend-mode:difference'></div></div>"
    ));
    near(&img, (100, 75), [255, 255, 255], 0, "difference over black");
    near(&img, (100, 125), [0, 55, 255], 0, "difference over yellow");
    near(&img, (250, 75), [0, 0, 0], 0, "multiply over black");
    near(&img, (250, 125), [0, 100, 0], 0, "multiply over yellow");
    // An isolated group: the blending child sees only the group (nothing).
    near(
        &img,
        (50, 260),
        [255, 255, 255],
        0,
        "inside isolation: isolate",
    );
    // A positioned box without z-index is no stacking context: the child
    // blends with the red below.
    near(&img, (250, 260), [0, 255, 255], 0, "no isolation");
}

#[test]
fn backdrop_filter_filters_what_is_behind_inside_the_border_box() {
    let img = render(&format!(
        "{RESET}<style>.a {{ position: absolute }}</style>\
         <div class=a style='left:0;top:0;width:400px;height:300px;\
         background:repeating-linear-gradient(90deg,#000 0 10px,#fff 10px 20px)'></div>\
         <div class=a style='left:100px;top:50px;width:100px;height:100px;backdrop-filter:blur(6px)'></div>\
         <div class=a style='left:250px;top:50px;width:100px;height:100px;border-radius:50px;\
         background:rgba(255,0,0,.25);backdrop-filter:invert(1)'></div>\
         <div class=a style='left:100px;top:180px;width:100px;height:100px;\
         backdrop-filter:blur(4px);opacity:.5'></div>"
    ));
    let gray = |v: u8| [v, v, v];
    // Stripes 10 px wide, blurred inside the box only.
    near(&img, (150, 100), gray(132), 6, "blurred stripes");
    near(&img, (155, 100), gray(154), 6, "blurred stripes");
    near(&img, (95, 100), gray(255), 0, "outside, left");
    near(&img, (205, 100), gray(0), 0, "outside, right");
    // Edges mirror the backdrop instead of fading to transparent.
    near(&img, (101, 100), gray(27), 8, "mirrored edge");
    // Inverted, under the element's own background, inside its radius.
    near(&img, (300, 100), [255, 191, 191], 1, "inverted backdrop");
    near(&img, (252, 52), gray(255), 0, "outside the radius");
    // The element's opacity fades its filtered backdrop too.
    near(&img, (150, 230), gray(197), 6, "half opacity");
}

#[test]
fn an_identity_filter_still_makes_a_stacking_context() {
    // The z-index:2 child stays inside its filtered parent's context, so the
    // later z-index:1 sibling covers it.
    let img = render(&format!(
        "{RESET}<div style='width:100px;height:100px;background:red;filter:blur(0)'>\
         <div style='position:relative;z-index:2;width:60px;height:60px;background:blue'></div></div>\
         <div style='position:relative;z-index:1;margin-top:-80px;margin-left:20px;width:60px;\
         height:60px;background:lime'></div>"
    ));
    near(&img, (40, 40), [0, 255, 0], 0, "sibling over the child");
    near(&img, (10, 10), [0, 0, 255], 0, "child");
    near(&img, (90, 90), [255, 0, 0], 0, "parent");
}

#[test]
fn a_root_filter_covers_its_canvas_background_but_not_the_default_white() {
    let page = |body: &str| {
        format!(
            "<html style='filter:invert(1)'><body style='margin:0;{body}'>\
             <div style='width:100px;height:100px;background:rgb(0,0,255)'></div></body></html>"
        )
    };
    let img = render(&page(""));
    near(&img, (50, 50), [255, 255, 0], 0, "inverted content");
    near(&img, (200, 250), [255, 255, 255], 0, "default canvas");
    let img = render(&page("background:rgb(200,0,0)"));
    near(&img, (50, 50), [255, 255, 0], 0, "inverted content");
    near(
        &img,
        (200, 250),
        [55, 255, 255],
        0,
        "inverted canvas background",
    );
}
