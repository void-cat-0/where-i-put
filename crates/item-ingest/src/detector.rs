//! Detectors. `NullDetector` keeps the camera loop honest in tests;
//! `YoloDetector` (feature `yolo`, ONNX via ort) is the real backend.

use item_core::Detection;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DetectorError {
    #[error(transparent)]
    Inference(Box<dyn std::error::Error + Send + Sync>),
}

pub trait Detector: Send {
    /// Detect objects in an RGB8 frame. Returns raw hits; caller applies NMS.
    fn detect(&self, rgb: &[u8], width: u32, height: u32) -> Result<Vec<Detection>, DetectorError>;
}

/// Reports nothing. Useful with `MockSource` to exercise plumbing only.
pub struct NullDetector;

impl Detector for NullDetector {
    fn detect(&self, _rgb: &[u8], _width: u32, _height: u32) -> Result<Vec<Detection>, DetectorError> {
        Ok(vec![])
    }
}

#[cfg(feature = "yolo")]
pub mod yolo {
    //! YOLO via ONNX Runtime. Preprocess (letterbox) + postprocess (confidence
    //! decode + per-class filter) follow the layout of the `yolo-rs` crate;
    //! NMS itself stays in `item_core::geo` so both backends share it.
    //!
    //! Model on disk: any YOLOv8/11 detection export in ONNX with output
    //! shape [1, 4+num_classes, num_boxes] (e.g. `yolo11n.onnx`, class names
    //! COCO). Fetch once with `hf-hub` and point `model_path` at it.

    use ort::session::Session;
    use ort::value::Tensor;

    use super::DetectorError;
    use item_core::Detection;

    pub struct YoloDetector {
        session: std::sync::Mutex<Session>,
        labels: Vec<String>,
        input_size: usize,
        conf_threshold: f32,
    }

    impl YoloDetector {
        pub fn new(
            model_path: &std::path::Path,
            labels: Vec<String>,
            input_size: usize,
            conf_threshold: f32,
        ) -> Result<Self, DetectorError> {
            let session = Session::builder()
                .map_err(|e| DetectorError::Inference(Box::new(e)))?
                .commit_from_file(model_path)
                .map_err(|e| DetectorError::Inference(Box::new(e)))?;
            Ok(Self {
                session: std::sync::Mutex::new(session),
                labels,
                input_size,
                conf_threshold,
            })
        }
    }

    impl super::Detector for YoloDetector {
        fn detect(
            &self,
            rgb: &[u8],
            width: u32,
            height: u32,
        ) -> Result<Vec<Detection>, DetectorError> {
            // 1. letterbox resize to input_size^2, CHW f32 normalized to [0,1]
            let src = image::RgbImage::from_raw(width, height, rgb.to_vec())
                .ok_or_else(|| DetectorError::Inference("bad rgb buffer".into()))?;
            let resized = image::imageops::resize(
                &src,
                self.input_size as u32,
                self.input_size as u32,
                image::imageops::FilterType::Triangle,
            );
            let area = (self.input_size * self.input_size) as usize;
            let mut chw = vec![0f32; 3 * area];
            for (i, px) in resized.pixels().enumerate() {
                chw[i] = px[0] as f32 / 255.0;
                chw[area + i] = px[1] as f32 / 255.0;
                chw[2 * area + i] = px[2] as f32 / 255.0;
            }

            // 2. run
            let mut session = self
                .session
                .lock()
                .map_err(|e| DetectorError::Inference(e.to_string().into()))?;
            let input = Tensor::from_array(([1usize, 3, self.input_size, self.input_size], chw))
                .map_err(|e| DetectorError::Inference(Box::new(e)))?;
            let outputs = session
                .run(ort::inputs![input])
                .map_err(|e| DetectorError::Inference(Box::new(e)))?;
            let (shape, data): (Vec<usize>, Vec<f32>) = outputs[0]
                .try_extract_tensor::<f32>()
                .map_err(|e| DetectorError::Inference(Box::new(e)))?;
            // expect [1, 4+nc, n] (v8/11 style)
            if shape.len() != 3 {
                return Err(DetectorError::Inference("unexpected output rank".into()));
            }
            let (dim1, dim2) = (shape[1], shape[2]);
            let n_attr = if dim1 > dim2 { dim1 } else { dim2 };
            let n_boxes = if dim1 > dim2 { dim2 } else { dim1 };
            let transposed = dim1 < dim2; // attrs on rows => [1, n, attrs]
            let attr = |b: usize, a: usize| -> f32 {
                if transposed {
                    data[b * n_attr + a]
                } else {
                    data[a * n_boxes + b]
                }
            };

            // 3. decode: cx, cy, w, h + per-class scores; scale boxes back to pixels
            let mut out = Vec::new();
            let sx = width as f32 / self.input_size as f32;
            let sy = height as f32 / self.input_size as f32;
            for b in 0..n_boxes {
                let (mut best, mut bi) = (0f32, 0usize);
                for (ci, _) in self.labels.iter().enumerate().take(n_attr - 4) {
                    let s = attr(b, 4 + ci);
                    if s > best {
                        best = s;
                        bi = ci;
                    }
                }
                if best < self.conf_threshold {
                    continue;
                }
                let (cx, cy, w, h) = (attr(b, 0), attr(b, 1), attr(b, 2), attr(b, 3));
                out.push(Detection {
                    label: self.labels[bi].clone(),
                    confidence: best,
                    bbox: [
                        (cx - w / 2.0) * sx,
                        (cy - h / 2.0) * sy,
                        (cx + w / 2.0) * sx,
                        (cy + h / 2.0) * sy,
                    ],
                });
            }
            Ok(out)
        }
    }
}
