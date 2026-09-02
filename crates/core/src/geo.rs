//! Box geometry: IoU matching and greedy NMS.
//!
//! Deliberately hand-rolled (~60 lines) instead of pulling in a tracker crate:
//! no maintained Rust port of norfair exists, and fixed-camera zone aggregation
//! only needs frame-level association, not long-horizon trajectories.

/// Intersection over union of two [x0, y0, x1, y1] boxes.
pub fn iou(a: &[f32; 4], b: &[f32; 4]) -> f32 {
    let ix0 = a[0].max(b[0]);
    let iy0 = a[1].max(b[1]);
    let ix1 = a[2].min(b[2]);
    let iy1 = a[3].min(b[3]);
    let w = (ix1 - ix0).max(0.0);
    let h = (iy1 - iy0).max(0.0);
    let inter = w * h;
    let area_a = (a[2] - a[0]).max(0.0) * (a[3] - a[1]).max(0.0);
    let area_b = (b[2] - b[0]).max(0.0) * (b[3] - b[1]).max(0.0);
    let union = area_a + area_b - inter;
    if union <= 0.0 { 0.0 } else { inter / union }
}

/// Greedy NMS over (label, score, box): sort by score, keep a box if its IoU
/// with every kept box of the same label is below `threshold`.
pub fn nms<T: LabelBox>(dets: &[T], threshold: f32) -> Vec<usize> {
    let mut order: Vec<usize> = (0..dets.len()).collect();
    order.sort_by(|&i, &j| dets[j].score().total_cmp(&dets[i].score()));
    let mut kept: Vec<usize> = Vec::new();
    for &i in &order {
        let suppress = kept.iter().any(|&k| {
            dets[k].label() == dets[i].label() && iou(dets[k].bbox(), dets[i].bbox()) > threshold
        });
        if !suppress {
            kept.push(i);
        }
    }
    kept
}

/// Minimal trait so `nms` works over `Detection` and raw slices.
pub trait LabelBox {
    fn label(&self) -> &str;
    fn score(&self) -> f32;
    fn bbox(&self) -> &[f32; 4];
}

impl LabelBox for crate::Detection {
    fn label(&self) -> &str {
        &self.label
    }
    fn score(&self) -> f32 {
        self.confidence
    }
    fn bbox(&self) -> &[f32; 4] {
        &self.bbox
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Detection;

    fn d(label: &str, c: f32, b: [f32; 4]) -> Detection {
        Detection { label: label.into(), confidence: c, bbox: b }
    }

    #[test]
    fn iou_identical_is_one() {
        assert!((iou(&[0.0, 0.0, 10.0, 10.0], &[0.0, 0.0, 10.0, 10.0]) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn iou_disjoint_is_zero() {
        assert_eq!(iou(&[0.0, 0.0, 1.0, 1.0], &[5.0, 5.0, 6.0, 6.0]), 0.0);
    }

    #[test]
    fn nms_keeps_best_and_other_label() {
        let dets = vec![
            d("keys", 0.9, [0.0, 0.0, 10.0, 10.0]),
            d("keys", 0.6, [1.0, 1.0, 10.0, 10.0]), // overlaps heavily -> suppressed
            d("keys", 0.7, [50.0, 50.0, 60.0, 60.0]), // separate -> kept
            d("bag", 0.8, [0.0, 0.0, 10.0, 10.0]), // different label -> kept
        ];
        let kept = nms(&dets, 0.5);
        assert_eq!(kept, vec![0, 3, 2]);
    }
}
