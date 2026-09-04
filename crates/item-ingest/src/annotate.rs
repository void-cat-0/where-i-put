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

/// Annotate a copy of one decoded frame: zone rectangles first (dashed gray),
/// then every surviving detection box of that frame in its label's color --
/// including duplicates of one object (they are exactly what makes a row's
/// hit_count race ahead of the frame counter, and one glance should explain
/// it). `None` if the buffer is shorter than the frame.
pub fn annotate(
    rgb: &[u8],
    w: u32,
    h: u32,
    survivors: &[&Detection],
    regions: &[Region],
) -> Option<image::RgbImage> {
    let mut img = image::RgbImage::from_raw(w, h, rgb.to_vec())?;
    for rg in regions {
        draw_rect_dashed(&mut img, rg.rect, REGION_COLOR);
    }
    let t = box_thickness(h);
    for d in survivors {
        draw_rect(&mut img, d.bbox, palette_color(&d.label), t);
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
        let out = annotate(&rgb, 64, 48, &[&d], std::slice::from_ref(&region)).unwrap();
        // box top edge: box color wins over the dashed region line
        assert_eq!(*out.get_pixel(24, 8), palette_color("bottle"));
        // region top edge at a dash-on offset, away from the box
        assert_eq!(*out.get_pixel(4, 4), REGION_COLOR);
        // untouched background stays put
        assert_eq!(*out.get_pixel(50, 46), image::Rgb([200, 200, 200]));
    }

    #[test]
    fn annotate_rejects_short_buffer() {
        assert!(annotate(&[0u8; 3], 64, 48, &[], &[]).is_none());
    }
}
