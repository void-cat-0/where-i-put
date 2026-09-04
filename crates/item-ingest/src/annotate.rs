//! Burn detector boxes and zone rectangles into snapshot JPEGs.
//!
//! Pure pixel helpers only -- no DB, no IO. The annotation lives in image
//! pixels, never in the database: observations stay box-free (we deliberately
//! do not store per-frame detections), and the snapshot of an observation's
//! birth frame is the one moment where frame + boxes coincide.

use item_core::{Detection, Region};

/// High-contrast strokes that stay readable on indoor footage. 12 buckets
/// spread the common COCO labels apart (checked against FNV collisions).
const PALETTE: [[u8; 3]; 12] = [
    [255, 60, 60],   // red
    [80, 200, 120],  // green
    [80, 150, 255],  // blue
    [255, 200, 60],  // yellow
    [230, 120, 255], // magenta
    [80, 220, 220],  // cyan
    [255, 140, 60],  // orange
    [170, 130, 255], // violet
    [160, 255, 120], // lime
    [255, 100, 180], // pink
    [120, 200, 255], // sky
    [255, 230, 140], // sand
];

/// Neutral stroke for zone/region outlines: gray dashed, so it never reads
/// as an object box.
pub const REGION_COLOR: image::Rgb<u8> = image::Rgb([150, 150, 150]);

/// Deterministic color per label (FNV-1a). Labels may share a color after
/// ~12 distinct classes; position on the image disambiguates those.
pub fn palette_color(label: &str) -> image::Rgb<u8> {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in label.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    image::Rgb(PALETTE[(h % PALETTE.len() as u64) as usize])
}

/// Box stroke width: ~2px at 720p, scaling with frame height.
pub fn box_thickness(height: u32) -> u32 {
    (height / 360).max(1)
}

/// Clamp a float-pixel rect to image bounds. `None` when fully outside or
/// degenerate. Detectors occasionally emit boxes slightly past the frame.
fn clamp_rect(img: &image::RgbImage, r: [f32; 4]) -> Option<(u32, u32, u32, u32)> {
    let wmax = img.width().saturating_sub(1) as i64;
    let hmax = img.height().saturating_sub(1) as i64;
    let x0 = (r[0].floor() as i64).clamp(0, wmax) as u32;
    let y0 = (r[1].floor() as i64).clamp(0, hmax) as u32;
    let x1 = (r[2].ceil() as i64).clamp(0, wmax) as u32;
    let y1 = (r[3].ceil() as i64).clamp(0, hmax) as u32;
    (x1 >= x0 && y1 >= y0).then_some((x0, y0, x1, y1))
}

/// Solid rectangle outline, `t` px thick, strokes hugging the inside of the
/// rect so the marked object region stays readable.
pub fn draw_rect(img: &mut image::RgbImage, rect: [f32; 4], color: image::Rgb<u8>, t: u32) {
    let Some((x0, y0, x1, y1)) = clamp_rect(img, rect) else {
        return;
    };
    for k in 0..t {
        for x in x0..=x1 {
            img.put_pixel(x, (y0 + k).min(y1), color);
            img.put_pixel(x, y1.saturating_sub(k), color);
        }
        for y in y0..=y1 {
            img.put_pixel((x0 + k).min(x1), y, color);
            img.put_pixel(x1.saturating_sub(k), y, color);
        }
    }
}

/// 10-on/6-off dashed rectangle outline (1px) for zone boundaries.
pub fn draw_rect_dashed(img: &mut image::RgbImage, rect: [f32; 4], color: image::Rgb<u8>) {
    let Some((x0, y0, x1, y1)) = clamp_rect(img, rect) else {
        return;
    };
    const CYCLE: u32 = 16;
    const ON: u32 = 10;
    for x in x0..=x1 {
        if (x - x0) % CYCLE < ON {
            img.put_pixel(x, y0, color);
            img.put_pixel(x, y1, color);
        }
    }
    for y in y0..=y1 {
        if (y - y0) % CYCLE < ON {
            img.put_pixel(x0, y, color);
            img.put_pixel(x1, y, color);
        }
    }
}

/// Ring drawn around the row's own box so it pops off the frame.
pub const WHITE_RING: image::Rgb<u8> = image::Rgb([255, 255, 255]);

/// "label conf" -- the chip text for one detection.
pub fn label_text(d: &Detection) -> String {
    format!("{} {:.2}", d.label, d.confidence)
}

/// Draw ASCII text in the public-domain IBM VGA 8x8 font (`font8x8`),
/// magnified `scale`x; returns the pixel width drawn. Out-of-frame glyph
/// pixels are clipped, not panicked on.
pub fn draw_text(
    img: &mut image::RgbImage,
    x: u32,
    y: u32,
    text: &str,
    color: image::Rgb<u8>,
    scale: u32,
) -> u32 {
    use font8x8::UnicodeFonts;
    let scale = scale.max(1);
    let put = |img: &mut image::RgbImage, x: u32, y: u32| {
        if x < img.width() && y < img.height() {
            img.put_pixel(x, y, color);
        }
    };
    for (ci, ch) in text.chars().enumerate() {
        let ch = if ch.is_ascii() { ch } else { '?' };
        let Some(glyph) = font8x8::BASIC_FONTS.get(ch) else {
            continue;
        };
        let gx = x + ci as u32 * 8 * scale;
        for (row, &bits) in glyph.iter().enumerate() {
            for col in 0..8u32 {
                // font8x8 is LSB-first: bit 0 is the LEFTMOST pixel (matches
                // the crate's own print_set renderer).
                if bits & (1 << col) != 0 {
                    for dy in 0..scale {
                        for dx in 0..scale {
                            put(img, gx + col * scale + dx, y + row as u32 * scale + dy);
                        }
                    }
                }
            }
        }
    }
    text.chars().count() as u32 * 8 * scale
}

/// Ultralytics-style chip: label-colored plate with black text, anchored to
/// a detection's top-left corner `lift`px above it (highlighted rows lift it
/// past their white ring). Flips INSIDE the box when there is no room above.
pub fn draw_label(img: &mut image::RgbImage, d: &Detection, scale: u32, lift: u32) {
    let text = label_text(d);
    let scale = scale.max(1);
    let pad = scale;
    let tw = text.chars().count() as u32 * 8 * scale;
    let (pw, ph) = (tw + 2 * pad, 8 * scale + 2 * pad);
    let (x0, y0, _x1, y1) = match clamp_rect(img, d.bbox) {
        Some(r) => r,
        None => return,
    };
    let y0 = y0.saturating_sub(lift);
    let (px, py) = match y0.checked_sub(ph) {
        Some(y) => (x0, y),
        None => (x0, y1.saturating_sub(ph)), // no room above: sit inside
    };
    for x in px..(px + pw).min(img.width()) {
        for y in py..(py + ph).min(img.height()) {
            img.put_pixel(x, y, palette_color(&d.label));
        }
    }
    draw_text(img, px + pad, py + pad, &text, image::Rgb([0, 0, 0]), scale);
}

/// Annotated copy of one decoded frame:
/// - zone rects: dashed gray,
/// - every other surviving detection of that frame: 1px label-colored rect
///   with an "label conf" chip (thin chips keep the frame readable),
/// - `highlight` (index into `survivors` -- the row being born): thick
///   label-colored rect, a white outer ring, and a larger chip.
///
/// The highlight MUST differ per row even when many rows share one birth
/// frame: each new observation renders its own copy of the frame here.
/// Duplicates of one object are still drawn (they explain hit_count races).
/// Returns `None` if the buffer is shorter than the frame.
pub fn annotate(
    rgb: &[u8],
    w: u32,
    h: u32,
    survivors: &[&Detection],
    regions: &[Region],
    highlight: Option<usize>,
) -> Option<image::RgbImage> {
    let mut img = image::RgbImage::from_raw(w, h, rgb.to_vec())?;
    for rg in regions {
        draw_rect_dashed(&mut img, rg.rect, REGION_COLOR);
    }
    let t = box_thickness(h);
    let hl = highlight.filter(|i| *i < survivors.len());

    for (i, d) in survivors.iter().enumerate() {
        if hl == Some(i) {
            continue;
        }
        draw_rect(&mut img, d.bbox, palette_color(&d.label), 1);
        draw_label(&mut img, d, 1, 0);
    }
    if let Some(i) = hl {
        let d = survivors[i];
        let color = palette_color(&d.label);
        // white ring hugging the box from outside
        let g = 2 * t;
        let ring = [
            d.bbox[0] - g as f32,
            d.bbox[1] - g as f32,
            d.bbox[2] + g as f32,
            d.bbox[3] + g as f32,
        ];
        draw_rect(&mut img, ring, WHITE_RING, t);
        draw_rect(&mut img, d.bbox, color, t + 1);
        draw_label(&mut img, d, t + 1, g);
    }
    Some(img)
}

#[cfg(test)]
mod tests {
    use super::*;
    use item_core::Region;

    fn det(label: &str, bbox: [f32; 4]) -> Detection {
        Detection {
            label: label.into(),
            confidence: 0.9,
            bbox,
        }
    }

    fn blank(w: u32, h: u32) -> image::RgbImage {
        image::RgbImage::from_pixel(w, h, image::Rgb([0, 0, 0]))
    }

    #[test]
    fn palette_is_deterministic_and_spreads_common_labels() {
        assert_eq!(palette_color("bottle"), palette_color("bottle"));
        assert_ne!(palette_color("bottle"), palette_color("cup"));
        assert_ne!(palette_color("bottle"), palette_color("person"));
        assert_ne!(palette_color("cup"), palette_color("person"));
    }

    #[test]
    fn draw_rect_strokes_outline_not_interior() {
        let mut img = blank(100, 50);
        let red = image::Rgb([255, 0, 0]);
        draw_rect(&mut img, [10.0, 10.0, 40.0, 30.0], red, 1);
        for (x, y) in [(10, 10), (25, 10), (40, 10), (25, 30), (10, 20), (40, 20)] {
            assert_eq!(img.get_pixel(x, y).0, red.0, "outline pixel ({x},{y})");
        }
        assert_eq!(*img.get_pixel(25, 20), image::Rgb([0, 0, 0]), "interior");
        assert_eq!(*img.get_pixel(9, 10), image::Rgb([0, 0, 0]), "outside left");
        assert_eq!(*img.get_pixel(10, 9), image::Rgb([0, 0, 0]), "outside top");
    }

    #[test]
    fn draw_rect_clamps_out_of_frame_boxes() {
        let mut img = blank(100, 50);
        draw_rect(
            &mut img,
            [-50.0, -50.0, 500.0, 400.0],
            image::Rgb([255, 255, 255]),
            2,
        );
        assert_eq!(*img.get_pixel(0, 0), image::Rgb([255, 255, 255]));
        assert_eq!(*img.get_pixel(99, 49), image::Rgb([255, 255, 255]));
        assert_eq!(
            *img.get_pixel(50, 25),
            image::Rgb([0, 0, 0]),
            "interior untouched"
        );
        // fully outside -> no panic, no pixels changed
        let before = img.clone();
        draw_rect(
            &mut img,
            [200.0, 200.0, 300.0, 300.0],
            image::Rgb([255, 255, 255]),
            1,
        );
        assert_eq!(before, img);
    }

    #[test]
    fn dashed_outline_alternates_on_and_off() {
        let mut img = blank(100, 50);
        draw_rect_dashed(
            &mut img,
            [10.0, 10.0, 90.0, 10.0],
            image::Rgb([255, 255, 255]),
        );
        let on = (10..=90)
            .filter(|x| *img.get_pixel(*x, 10) == image::Rgb([255, 255, 255]))
            .count();
        assert!(
            on > 0 && on < 81,
            "expected a mix of dashes and gaps, got {on} on-pixels"
        );
    }

    #[test]
    fn annotate_draws_regions_below_boxes() {
        let rgb = vec![200u8; 64 * 48 * 3]; // flat gray frame
        let d = det("bottle", [8.0, 8.0, 40.0, 32.0]);
        let region = Region {
            id: 1,
            camera_id: "cam".into(),
            name: "desk".into(),
            rect: [4.0, 4.0, 44.0, 44.0],
        };
        let out = annotate(&rgb, 64, 48, &[&d], std::slice::from_ref(&region), None).unwrap();
        // box top edge: box color wins over the dashed region line
        assert_eq!(*out.get_pixel(24, 8), palette_color("bottle"));
        // region top edge at a dash-on offset, away from the box
        assert_eq!(*out.get_pixel(4, 4), REGION_COLOR);
        // untouched background stays put
        assert_eq!(*out.get_pixel(50, 46), image::Rgb([200, 200, 200]));
    }

    #[test]
    fn annotate_rejects_short_buffer() {
        assert!(annotate(&[0u8; 3], 64, 48, &[], &[], None).is_none());
    }

    // font8x8 is LSB-first (bit 0 = leftmost). 'F' is the discriminator:
    // correct order puts the stem on the LEFT and bars extend right; a
    // reversed (MSB) bug would mirror it. 'A' just checks advance width.
    #[test]
    fn draw_text_renders_readable_glyphs() {
        let mut img = blank(40, 10);
        let white = image::Rgb([255, 255, 255]);
        assert_eq!(draw_text(&mut img, 0, 0, "A", white, 1), 8);
        assert_eq!(draw_text(&mut img, 0, 0, "ABC", white, 1), 24);

        let mut f = blank(16, 10);
        draw_text(&mut f, 0, 0, "F", white, 1);
        // VGA 'F' puts its stem at columns 1-2 with bars extending right;
        // a mirrored (MSB-read) bug would move the stem to the right side.
        let stem_left = (1..7).filter(|y| *f.get_pixel(1, *y) == white).count();
        let stem_right = (3..7).filter(|y| *f.get_pixel(5, *y) == white).count();
        assert!(
            stem_left >= 4,
            "'F' stem should hug the left side, got {stem_left}"
        );
        assert_eq!(stem_right, 0, "ink in the right column = mirrored glyph");
    }

    #[test]
    fn draw_text_scales_and_clips() {
        let mut img = blank(30, 30);
        let red = image::Rgb([255, 0, 0]);
        assert_eq!(
            draw_text(&mut img, 2, 2, "F", red, 3),
            24,
            "scale 3 -> 24px advance"
        );
        assert!(
            (0..24).any(|x| *img.get_pixel(2 + x, 2) == red),
            "scale-3 bar present"
        );
        // far-right draw must clip without panicking
        draw_text(&mut img, 28, 0, "F", red, 3);
    }

    #[test]
    fn highlight_rings_only_its_own_box() {
        let d1 = det("bottle", [10.0, 10.0, 30.0, 30.0]);
        let d2 = det("bottle", [50.0, 50.0, 80.0, 80.0]);
        let dets = [&d1, &d2];
        let rgb = vec![0u8; 100 * 100 * 3];
        let out = annotate(&rgb, 100, 100, &dets, &[], Some(1)).unwrap();
        // White pixels are ONLY ever produced by the ring, so counting them
        // per neighborhood proves d2 is ringed and d1 is not -- regardless
        // of where the (pink/black) chips happen to land.
        let whites = |x0: u32, y0: u32, x1: u32, y1: u32| -> u32 {
            (x0..=x1)
                .flat_map(|x| (y0..=y1).map(move |y| (x, y)))
                .filter(|(x, y)| *out.get_pixel(*x, *y) == WHITE_RING)
                .count() as u32
        };
        assert_eq!(whites(0, 0, 44, 44), 0, "non-highlighted d1 gets no ring");
        // ring perimeter at [48..82]^2 is ~136 px; require most of it:
        assert!(whites(44, 44, 84, 84) > 120, "highlighted d2 is ringed");
    }

    #[test]
    fn every_row_shares_one_frame_but_highlights_its_own_box() {
        let d1 = det("cup", [10.0, 10.0, 30.0, 30.0]);
        let d2 = det("chair", [50.0, 50.0, 70.0, 70.0]);
        let dets = [&d1, &d2];
        let rgb = vec![128u8; 90 * 90 * 3];
        let a = annotate(&rgb, 90, 90, &dets, &[], Some(0)).unwrap();
        let b = annotate(&rgb, 90, 90, &dets, &[], Some(1)).unwrap();
        assert_ne!(a, b, "same frame, different rows must not share a file");
        // ring presence flips with the highlight:
        let ring_at = |img: &image::RgbImage, x: u32, y: u32| {
            (x..x + 4).any(|cx| (y..y + 4).any(|cy| *img.get_pixel(cx, cy) == WHITE_RING))
        };
        assert!(ring_at(&a, 6, 6), "a rings the cup box");
        assert!(!ring_at(&b, 6, 6), "b must not ring the cup box");
        assert!(ring_at(&b, 46, 46), "b rings the chair box");
    }
}
