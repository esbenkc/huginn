//! Geometry for backgrounds, gradients, transforms and replaced content.
//! Pure functions from computed values and boxes to display-list numbers.

use huginn_layout::Rect;
use huginn_style::{
    BackgroundSize, Color, ColorStop, Gradient, GradientShape, LengthPercentage, ObjectFit,
    RadialExtent, TransformOp,
};
use vello_cpu::kurbo::Affine;

/// Size of one background tile (CSS Backgrounds 3, 3.9). `intrinsic` is the
/// image size; gradients have none and fill the positioning area.
pub(crate) fn tile_size(
    size: BackgroundSize,
    area: Rect,
    intrinsic: Option<(f32, f32)>,
) -> (f32, f32) {
    let (aw, ah) = (area.width, area.height);
    match size {
        BackgroundSize::Auto => intrinsic.unwrap_or((aw, ah)),
        BackgroundSize::Cover | BackgroundSize::Contain => match intrinsic {
            Some((iw, ih)) if iw > 0.0 && ih > 0.0 => {
                let (sx, sy) = (aw / iw, ah / ih);
                let s = if size == BackgroundSize::Cover {
                    sx.max(sy)
                } else {
                    sx.min(sy)
                };
                (iw * s, ih * s)
            }
            _ => (aw, ah),
        },
        BackgroundSize::Explicit(w, h) => {
            let (rw, rh) = (w.resolve(Some(aw)), h.resolve(Some(ah)));
            match (rw, rh, intrinsic) {
                (Some(w), Some(h), _) => (w, h),
                (Some(w), None, Some((iw, ih))) if iw > 0.0 => (w, w * ih / iw),
                (None, Some(h), Some((iw, ih))) if ih > 0.0 => (h * iw / ih, h),
                (Some(w), None, _) => (w, ah),
                (None, Some(h), _) => (aw, h),
                (None, None, _) => intrinsic.unwrap_or((aw, ah)),
            }
        }
    }
}

/// Top-left of the first tile: percentages align the same point of the tile
/// and the area (CSS Backgrounds 3, 3.6).
pub(crate) fn tile_origin(
    pos: (LengthPercentage, LengthPercentage),
    area: Rect,
    tile: (f32, f32),
) -> (f32, f32) {
    // `room` (area minus tile) is the percentage basis.
    let axis = |p: LengthPercentage, room: f32| p.resolve(Some(room));
    (
        area.x + axis(pos.0, area.width - tile.0),
        area.y + axis(pos.1, area.height - tile.1),
    )
}

/// The rectangle an image is drawn into inside its content box
/// (`object-position` is always `50% 50%` for now).
pub(crate) fn object_fit_rect(content: Rect, iw: f32, ih: f32, fit: ObjectFit) -> Rect {
    if iw <= 0.0 || ih <= 0.0 || content.width <= 0.0 || content.height <= 0.0 {
        return content;
    }
    let contain = (content.width / iw).min(content.height / ih);
    let s = match fit {
        ObjectFit::Fill => return content,
        ObjectFit::Contain => contain,
        ObjectFit::Cover => (content.width / iw).max(content.height / ih),
        ObjectFit::None => 1.0,
        ObjectFit::ScaleDown => contain.min(1.0),
    };
    let (w, h) = (iw * s, ih * s);
    Rect {
        x: content.x + (content.width - w) / 2.0,
        y: content.y + (content.height - h) / 2.0,
        width: w,
        height: h,
    }
}

/// The CSS `transform` of a box as an affine map in document coordinates:
/// translate(origin) * ops * translate(-origin). Percentages resolve against
/// the border box.
pub(crate) fn transform_affine(
    ops: &[TransformOp],
    origin: (LengthPercentage, LengthPercentage),
    border_box: Rect,
) -> Affine {
    let (w, h) = (border_box.width, border_box.height);
    let ox = f64::from(border_box.x + origin.0.resolve(Some(w)));
    let oy = f64::from(border_box.y + origin.1.resolve(Some(h)));
    let mut m = Affine::translate((ox, oy));
    for op in ops {
        m *= match *op {
            TransformOp::Translate(x, y) => {
                Affine::translate((f64::from(x.resolve(Some(w))), f64::from(y.resolve(Some(h)))))
            }
            TransformOp::Scale(x, y) => Affine::scale_non_uniform(f64::from(x), f64::from(y)),
            TransformOp::Rotate(deg) => Affine::rotate(f64::from(deg).to_radians()),
            TransformOp::Skew(x, y) => Affine::skew(
                f64::from(x).to_radians().tan(),
                f64::from(y).to_radians().tan(),
            ),
            TransformOp::Matrix(c) => Affine::new(c.map(f64::from)),
        };
    }
    m * Affine::translate((-ox, -oy))
}

/// A gradient resolved against its box, ready to rasterize.
#[derive(Clone, Debug, PartialEq)]
pub enum ResolvedGradient {
    /// From `start` to `end`.
    Linear { start: (f32, f32), end: (f32, f32) },
    /// Rings around `center`: offset 0 at `start_radius`, 1 at `end_radius`,
    /// scaled vertically by `y_scale` (ellipses).
    Radial {
        center: (f32, f32),
        start_radius: f32,
        end_radius: f32,
        y_scale: f32,
    },
}

impl ResolvedGradient {
    /// The same gradient moved by (dx, dy).
    pub fn translated(&self, dx: f32, dy: f32) -> ResolvedGradient {
        match *self {
            ResolvedGradient::Linear { start, end } => ResolvedGradient::Linear {
                start: (start.0 + dx, start.1 + dy),
                end: (end.0 + dx, end.1 + dy),
            },
            ResolvedGradient::Radial {
                center,
                start_radius,
                end_radius,
                y_scale,
            } => ResolvedGradient::Radial {
                center: (center.0 + dx, center.1 + dy),
                start_radius,
                end_radius,
                y_scale,
            },
        }
    }
}

/// Resolve a CSS gradient for a gradient box (one background tile). Returns
/// the geometry, the color stops as (offset, color), and whether the stops
/// repeat. `None` when nothing is drawn (degenerate size).
pub(crate) fn resolve_gradient(
    g: &Gradient,
    tile: Rect,
) -> Option<(ResolvedGradient, Vec<(f32, Color)>, bool)> {
    let (w, h) = (tile.width, tile.height);
    if w <= 0.0 || h <= 0.0 || g.stops.is_empty() {
        return None;
    }
    let (cx, cy) = (tile.x + w / 2.0, tile.y + h / 2.0);
    let (mut geom, len) = match g.shape {
        GradientShape::Linear { angle_deg, corner } => {
            // CSS Images 3, 3.1.1: for `to <corner>` the angle makes the 50%
            // line pass through the two other corners.
            let a = match corner {
                Some((right, bottom)) => {
                    let sx = if right { 1.0 } else { -1.0 };
                    let sy = if bottom { 1.0 } else { -1.0 };
                    (sx * h).atan2(-sy * w)
                }
                None => angle_deg.to_radians(),
            };
            let (sin, cos) = a.sin_cos();
            let len = (w * sin).abs() + (h * cos).abs();
            let (dx, dy) = (sin * len / 2.0, -cos * len / 2.0);
            (
                ResolvedGradient::Linear {
                    start: (cx - dx, cy - dy),
                    end: (cx + dx, cy + dy),
                },
                len,
            )
        }
        GradientShape::Radial {
            circle,
            extent,
            center,
        } => {
            let px = center.0.resolve(Some(w));
            let py = center.1.resolve(Some(h));
            let (near_x, far_x) = (px.abs().min((w - px).abs()), px.abs().max((w - px).abs()));
            let (near_y, far_y) = (py.abs().min((h - py).abs()), py.abs().max((h - py).abs()));
            let (rx, ry) = match (extent, circle) {
                (RadialExtent::ClosestSide, true) => (near_x.min(near_y), near_x.min(near_y)),
                (RadialExtent::FarthestSide, true) => (far_x.max(far_y), far_x.max(far_y)),
                (RadialExtent::ClosestCorner, true) => {
                    let r = near_x.hypot(near_y);
                    (r, r)
                }
                (RadialExtent::FarthestCorner, true) => {
                    let r = far_x.hypot(far_y);
                    (r, r)
                }
                (RadialExtent::ClosestSide, false) => (near_x, near_y),
                (RadialExtent::FarthestSide, false) => (far_x, far_y),
                // The ellipse with the side's aspect ratio through the corner.
                (RadialExtent::ClosestCorner, false) => (
                    near_x * std::f32::consts::SQRT_2,
                    near_y * std::f32::consts::SQRT_2,
                ),
                (RadialExtent::FarthestCorner, false) => (
                    far_x * std::f32::consts::SQRT_2,
                    far_y * std::f32::consts::SQRT_2,
                ),
                (RadialExtent::Explicit(x, _), true) => {
                    let r = x.resolve(Some(w));
                    (r, r)
                }
                (RadialExtent::Explicit(x, y), false) => (x.resolve(Some(w)), y.resolve(Some(h))),
            };
            if rx <= 0.0 || ry <= 0.0 {
                return None;
            }
            (
                ResolvedGradient::Radial {
                    center: (tile.x + px, tile.y + py),
                    start_radius: 0.0,
                    end_radius: rx,
                    y_scale: ry / rx,
                },
                rx,
            )
        }
    };
    let mut stops = resolve_stops(&g.stops, len);
    if g.repeating {
        // Map the gradient line onto [first stop, last stop] and repeat it.
        let (s0, s1) = (stops[0].0, stops[stops.len() - 1].0);
        if s1 - s0 <= 1e-4 {
            // Zero-length period: the spec paints the average color.
            let c = stops[stops.len() - 1].1;
            return Some((geom, vec![(0.0, c), (1.0, c)], false));
        }
        for s in &mut stops {
            s.0 = (s.0 - s0) / (s1 - s0);
        }
        match &mut geom {
            ResolvedGradient::Linear { start, end } => {
                let (x0, y0, dx, dy) = (start.0, start.1, end.0 - start.0, end.1 - start.1);
                *start = (x0 + dx * s0, y0 + dy * s0);
                *end = (x0 + dx * s1, y0 + dy * s1);
            }
            ResolvedGradient::Radial {
                start_radius,
                end_radius,
                ..
            } => {
                let r = *end_radius;
                *start_radius = r * s0;
                *end_radius = r * s1;
            }
        }
        Some((geom, stops, true))
    } else {
        Some((geom, clamp_unit(stops), false))
    }
}

/// CSS Images 3, 3.5.3: fill in missing stop positions and make them
/// non-decreasing. `len` is the gradient line length in px.
fn resolve_stops(stops: &[ColorStop], len: f32) -> Vec<(f32, Color)> {
    let len = len.max(1e-3);
    let n = stops.len();
    let mut pos: Vec<Option<f32>> = stops
        .iter()
        .map(|s| s.position.map(|p| p.resolve(Some(len)) / len))
        .collect();
    if pos[0].is_none() {
        pos[0] = Some(0.0);
    }
    if n > 1 && pos[n - 1].is_none() {
        pos[n - 1] = Some(1.0);
    }
    let mut max = f32::MIN;
    for p in pos.iter_mut().flatten() {
        if *p < max {
            *p = max;
        } else {
            max = *p;
        }
    }
    let mut i = 1;
    while i < n {
        if pos[i].is_some() {
            i += 1;
            continue;
        }
        let prev = pos[i - 1].unwrap_or(0.0);
        let mut j = i;
        while pos[j].is_none() {
            j += 1;
        }
        let next = pos[j].unwrap_or(1.0);
        let steps = (j - i + 1) as f32;
        for (k, p) in pos.iter_mut().enumerate().take(j).skip(i) {
            *p = Some(prev + (next - prev) * (k - i + 1) as f32 / steps);
        }
        i = j;
    }
    stops
        .iter()
        .zip(pos)
        .map(|(s, p)| (p.unwrap_or(0.0), s.color))
        .collect()
}

/// Cut stops to [0, 1], interpolating the colors at the ends (a padded
/// gradient keeps the end colors beyond them).
fn clamp_unit(stops: Vec<(f32, Color)>) -> Vec<(f32, Color)> {
    let (first, last) = (stops[0].0, stops[stops.len() - 1].0);
    if stops.len() == 1 {
        return vec![(0.0, stops[0].1), (1.0, stops[0].1)];
    }
    if first >= 0.0 && last <= 1.0 {
        return stops;
    }
    let at = |t: f32| -> Color {
        if t <= first {
            return stops[0].1;
        }
        if t >= last {
            return stops[stops.len() - 1].1;
        }
        for pair in stops.windows(2) {
            let ((p0, c0), (p1, c1)) = (pair[0], pair[1]);
            if t >= p0 && t <= p1 {
                let f = if p1 > p0 { (t - p0) / (p1 - p0) } else { 1.0 };
                return lerp_color(c0, c1, f);
            }
        }
        stops[stops.len() - 1].1
    };
    let mut out = Vec::with_capacity(stops.len() + 2);
    out.push((0.0, at(0.0)));
    out.extend(stops.iter().copied().filter(|s| s.0 > 0.0 && s.0 < 1.0));
    out.push((1.0, at(1.0)));
    out
}

/// Interpolate in premultiplied sRGB, like CSS gradients.
fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    let (aa, ba) = (f32::from(a.a) / 255.0, f32::from(b.a) / 255.0);
    let alpha = aa + (ba - aa) * t;
    if alpha <= 0.0 {
        return Color::TRANSPARENT;
    }
    let ch = |x: u8, y: u8| {
        let v = (f32::from(x) * aa + (f32::from(y) * ba - f32::from(x) * aa) * t) / alpha;
        v.round().clamp(0.0, 255.0) as u8
    };
    Color::rgba(
        ch(a.r, b.r),
        ch(a.g, b.g),
        ch(a.b, b.b),
        (alpha * 255.0).round() as u8,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stop(c: Color, p: Option<f32>) -> ColorStop {
        ColorStop {
            color: c,
            position: p.map(LengthPercentage::Percent),
        }
    }

    #[test]
    fn missing_stop_positions_are_spread_evenly_and_kept_monotonic() {
        let r = Color::rgb(255, 0, 0);
        let s = resolve_stops(
            &[
                stop(r, None),
                stop(r, Some(50.0)),
                stop(r, None),
                stop(r, Some(20.0)),
                stop(r, None),
            ],
            100.0,
        );
        let pos: Vec<f32> = s.iter().map(|s| s.0).collect();
        assert_eq!(pos, vec![0.0, 0.5, 0.5, 0.5, 1.0]);
    }

    #[test]
    fn to_corner_angle_passes_the_middle_line_through_the_other_corners() {
        let g = Gradient {
            repeating: false,
            shape: GradientShape::Linear {
                angle_deg: 180.0,
                corner: Some((true, true)),
            },
            stops: vec![stop(Color::BLACK, None), stop(Color::WHITE, None)],
        };
        let tile = Rect {
            x: 0.0,
            y: 0.0,
            width: 200.0,
            height: 100.0,
        };
        let (geom, _, _) = resolve_gradient(&g, tile).unwrap();
        let ResolvedGradient::Linear { start, end } = geom else {
            panic!()
        };
        // The direction is perpendicular to the top-right/bottom-left diagonal.
        let (dx, dy) = (end.0 - start.0, end.1 - start.1);
        let dot = dx * -200.0 + dy * 100.0;
        assert!(dot.abs() < 1e-2, "{start:?} {end:?}");
        // And the start/end points sit on the lines through the corners.
        let along =
            |p: (f32, f32)| ((p.0 - start.0) * dx + (p.1 - start.1) * dy) / (dx * dx + dy * dy);
        assert!(along((0.0, 0.0)).abs() < 1e-4);
        assert!((along((200.0, 100.0)) - 1.0).abs() < 1e-4);
    }

    #[test]
    fn stops_outside_the_line_are_cut_with_interpolated_end_colors() {
        let s = clamp_unit(vec![(-1.0, Color::BLACK), (1.0, Color::WHITE)]);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].0, 0.0);
        assert_eq!(s[0].1, Color::rgb(128, 128, 128));
        assert_eq!(s[1], (1.0, Color::WHITE));
    }

    #[test]
    fn cover_and_contain_keep_the_aspect_ratio() {
        let area = Rect {
            x: 0.0,
            y: 0.0,
            width: 200.0,
            height: 100.0,
        };
        assert_eq!(
            tile_size(BackgroundSize::Cover, area, Some((50.0, 50.0))),
            (200.0, 200.0)
        );
        assert_eq!(
            tile_size(BackgroundSize::Contain, area, Some((50.0, 50.0))),
            (100.0, 100.0)
        );
        let half = huginn_style::LengthPercentageAuto::Percent(50.0);
        assert_eq!(
            tile_size(
                BackgroundSize::Explicit(half, huginn_style::LengthPercentageAuto::Auto),
                area,
                Some((50.0, 25.0))
            ),
            (100.0, 50.0)
        );
    }
}
