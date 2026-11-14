//! Paint order, clipping, opacity and text placement, checked on pixels: each page is parsed,
//! styled, laid out and painted in-process, then single pixels are compared.

mod common;

use common::{px, render};

const RED: [u8; 3] = [255, 0, 0];
const GREEN: [u8; 3] = [0, 128, 0];
const BLUE: [u8; 3] = [0, 0, 255];
const WHITE: [u8; 3] = [255, 255, 255];

const RESET: &str = "<style>body{margin:0} div{box-sizing:border-box}</style>";

#[test]
fn negative_z_index_paints_below_in_flow_blocks() {
    let img = render(&format!(
        "{RESET}<style>
        #ctx {{ position: relative; z-index: 0; width: 200px; height: 200px }}
        #neg {{ position: absolute; z-index: -1; left: 0; top: 0; width: 100px; height: 100px; background: red }}
        #flow {{ width: 50px; height: 50px; background: green }}
        </style><div id=ctx><div id=neg></div><div id=flow></div></div>"
    ));
    assert_eq!(px(&img, 25, 25), GREEN, "in-flow block over z-index:-1");
    assert_eq!(px(&img, 75, 75), RED, "negative layer still painted");
}

#[test]
fn z_index_orders_positioned_boxes_not_tree_order() {
    let img = render(&format!(
        "{RESET}<style>
        div {{ position: absolute; top: 0; width: 100px; height: 100px }}
        #a {{ left: 0; z-index: 2; background: green }}
        #b {{ left: 50px; z-index: 1; background: red }}
        </style><div id=a></div><div id=b></div>"
    ));
    assert_eq!(
        px(&img, 75, 50),
        GREEN,
        "higher z-index wins over later tree order"
    );
    assert_eq!(px(&img, 125, 50), RED);
}

#[test]
fn positioned_boxes_paint_over_later_in_flow_blocks() {
    let img = render(&format!(
        "{RESET}<style>
        #pos {{ position: relative; width: 100px; height: 100px; background: green }}
        #after {{ margin-top: -50px; width: 100px; height: 100px; background: red }}
        </style><div id=pos></div><div id=after></div>"
    ));
    assert_eq!(
        px(&img, 50, 75),
        GREEN,
        "z-index:auto positioned paints after in-flow blocks"
    );
    assert_eq!(px(&img, 50, 125), RED);
}

#[test]
fn floats_paint_over_in_flow_block_backgrounds() {
    let img = render(&format!(
        "{RESET}<style>
        #f {{ float: left; width: 100px; height: 100px; background: green }}
        #b {{ height: 50px; background: red }}
        </style><div id=f></div><div id=b></div>"
    ));
    assert_eq!(
        px(&img, 50, 25),
        GREEN,
        "float over a following block's background"
    );
    assert_eq!(px(&img, 150, 25), RED);
}

#[test]
fn overflow_hidden_clips_descendants_but_not_escaping_abspos() {
    let img = render(&format!(
        "{RESET}<style>
        #pos {{ position: relative; width: 300px; height: 200px }}
        #clip {{ overflow: hidden; width: 100px; height: 100px }}
        #big {{ width: 200px; height: 50px; background: blue }}
        #abs {{ position: absolute; left: 0; top: 50px; width: 200px; height: 50px; background: green }}
        </style><div id=pos><div id=clip><div id=big></div><div id=abs></div></div></div>"
    ));
    assert_eq!(px(&img, 50, 25), BLUE);
    assert_eq!(px(&img, 150, 25), WHITE, "in-flow child clipped at 100px");
    assert_eq!(
        px(&img, 150, 75),
        GREEN,
        "abspos child whose containing block is outside the clip is not clipped"
    );
}

#[test]
fn overflow_hidden_clips_abspos_when_it_is_the_containing_block() {
    let img = render(&format!(
        "{RESET}<style>
        #clip {{ position: relative; overflow: hidden; width: 100px; height: 100px }}
        #abs {{ position: absolute; left: 50px; top: 0; width: 200px; height: 50px; background: green }}
        </style><div id=clip><div id=abs></div></div>"
    ));
    assert_eq!(px(&img, 75, 25), GREEN);
    assert_eq!(px(&img, 150, 25), WHITE);
}

#[test]
fn clip_path_shapes_clip_the_box_and_its_descendants() {
    let img = render(&format!(
        "{RESET}<style>
        div {{ width: 100px; height: 100px; background: blue }}
        #inset {{ clip-path: inset(10px 0 10px 0) }}
        #circle {{ clip-path: circle(50%); position: absolute; left: 150px; top: 0 }}
        #tri {{ clip-path: polygon(0 0, 100% 0, 0 100%); margin-top: 20px }}
        #gone {{ clip-path: inset(50%); position: absolute; left: 150px; top: 150px }}
        i {{ position: absolute; left: 0; top: 0; width: 300px; height: 300px; background: green }}
        </style><div id=inset></div><div id=circle></div><div id=tri></div><div id=gone><i></i></div>"
    ));
    assert_eq!(px(&img, 50, 5), WHITE, "inset top");
    assert_eq!(px(&img, 50, 50), BLUE);
    assert_eq!(px(&img, 50, 95), WHITE, "inset bottom");
    assert_eq!(px(&img, 200, 50), BLUE, "circle center");
    assert_eq!(
        px(&img, 153, 3),
        WHITE,
        "outside the circle, inside the box"
    );
    assert_eq!(px(&img, 10, 130), BLUE, "triangle top-left half");
    assert_eq!(px(&img, 90, 210), WHITE, "triangle bottom-right half");
    // An empty shape hides the box and its positioned descendants.
    assert_eq!(px(&img, 160, 160), WHITE);
    assert_eq!(px(&img, 350, 290), WHITE);
}

#[test]
fn opacity_composites_the_subtree_as_one_group() {
    let img = render(&format!(
        "{RESET}<style>
        #g {{ opacity: 0.5; width: 200px; height: 200px }}
        #a {{ width: 100px; height: 100px; background: blue }}
        #b {{ margin-top: -50px; width: 100px; height: 100px; background: blue }}
        </style><div id=g><div id=a></div><div id=b></div></div>"
    ));
    // Overlap and single coverage look the same: the group is blended once.
    let single = px(&img, 50, 25);
    let overlap = px(&img, 50, 75);
    assert_eq!(single, overlap);
    assert!(
        (126..=129).contains(&single[0]),
        "half-transparent blue over white: {single:?}"
    );
}

#[test]
fn text_baseline_lands_on_a_whole_pixel() {
    let text = |top: &str| {
        render(&format!(
            "{RESET}<div style='padding-top:{top};font:20px sans-serif'>HHH</div>"
        ))
    };
    // A half-pixel offset does not blur the text: it draws exactly like the
    // text one whole pixel up or down.
    let half = text("10.5px");
    assert!(
        half.data == text("10px").data || half.data == text("11px").data,
        "text at a fractional baseline differs from both whole-pixel positions"
    );
    assert!(text("10px").data != text("11px").data, "the text moved");
}

#[test]
fn inline_element_background_and_border_paint_per_line_piece() {
    // Two lines of ink-free text (no-break spaces): the start edge (padding,
    // border) is only on the first piece, the end edge only on the last.
    let img = render(&format!(
        "{RESET}<style>
        p {{ margin: 0; width: 100px; font: 20px/40px sans-serif }}
        span {{ background: green; padding: 0 30px; border-left: 10px solid blue; border-right: 10px solid red }}
        </style><p><span>\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}\u{a0} \u{a0}\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}</span></p>"
    ));
    assert_eq!(px(&img, 5, 20), BLUE, "start border on the first line");
    assert_eq!(px(&img, 25, 20), GREEN, "start padding has the background");
    assert_eq!(
        px(&img, 2, 60),
        GREEN,
        "second line starts without the start edge"
    );
    assert_eq!(px(&img, 99, 20), WHITE, "first line has no end edge");
    let red = (0..100).filter(|&x| px(&img, x, 60) == RED).count();
    assert!(
        (8..=10).contains(&red),
        "end border on the second line: {red} px"
    );
}

#[test]
fn box_edges_are_snapped_to_whole_pixels() {
    // Blink paints box geometry pixel-snapped: a 1px border and a background
    // at a fractional offset each cover whole pixels in full color, not two
    // half-tinted rows. Edges round half up: 10.5 -> 11, 39.9 -> 40.
    let img = render(&format!(
        "{RESET}<div style='padding-top:10.5px'><div style='height:20px;border-bottom:1px solid blue;background:red'></div></div>
        <div style='margin-top:9.4px;height:5px;background:green'></div>"
    ));
    // The red box spans 10.5..30.5: rows 11..30, the border on row 30.
    assert_eq!(px(&img, 5, 10), WHITE, "background starts at row 11");
    assert_eq!(px(&img, 5, 11), RED);
    assert_eq!(px(&img, 5, 29), RED);
    assert_eq!(px(&img, 5, 30), BLUE, "the border is one full blue row");
    assert_eq!(px(&img, 5, 31), WHITE);
    // The green box spans 39.9..44.9: rows 40..44.
    assert_eq!(px(&img, 5, 39), WHITE);
    assert_eq!((px(&img, 5, 40), px(&img, 5, 44)), (GREEN, GREEN));
    assert_eq!(px(&img, 5, 45), WHITE);
}
