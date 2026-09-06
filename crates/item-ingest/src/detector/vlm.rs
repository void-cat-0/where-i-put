//! VLM grounding detector: open-vocabulary detection behind the same
//! `Detector` trait, calling a sidecar that exposes an OpenAI-compatible
//! multimodal `POST {base_url}/chat/completions` (llama.cpp server, Ollama,
//! vLLM, cloud APIs). `base_url` must include the `/v1` prefix, matching the
//! `ITEM_VLM_BASE_URL` convention used by item-query's ask bar.
//!
//! Grounding contract: the image is sent as a base64 JPEG data URL plus a
//! text instruction asking for a JSON array of
//! `{"label", "bbox_2d": [xmin, ymin, xmax, ymax]}`. Coordinates are requested
//! normalized to 0-1000 (the Qwen-VL family convention); replies that exceed
//! 1000 anywhere are treated as absolute pixels instead. VLMs emit no
//! calibrated confidence, so a `score`/`confidence` field is used when present
//! and 1.0 otherwise -- NMS ordering still works.
//!
//! One HTTP round trip per detect() call: with a local LLM that is seconds,
//! not milliseconds -- run the camera loop with a low `--detect-fps` (0.2 by
//! default for this backend) and expect frames to stall while the request is
//! in flight.

use std::io::Cursor;
use std::str::FromStr;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};

use super::DetectorError;
use item_core::Detection;

/// Household vocabulary beyond COCO-80 -- the reason this backend exists.
/// Overridable via `--targets`; an empty string switches to open-ended
/// "list everything visible" mode.
pub const DEFAULT_TARGETS: &str =
    "remote,keys,scissors,charger,glasses,wallet,umbrella,medicine,bottle,cup,laptop,phone";

/// Coordinate convention the sidecar is asked to emit (and replies are
/// interpreted with, unless they obviously exceed it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordMode {
    /// Qwen-VL convention: integers in [0, 1000] relative to width/height.
    Norm1000,
    /// Absolute pixels of the frame as sent.
    Pixel,
}

impl FromStr for CoordMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "norm1000" => Ok(CoordMode::Norm1000),
            "pixel" => Ok(CoordMode::Pixel),
            other => Err(format!("unknown --vlm-coords '{other}' (norm1000|pixel)")),
        }
    }
}

/// Split a comma-separated targets string into a clean vocabulary
/// (trimmed, lowercased, deduplicated, order kept).
pub fn parse_targets(spec: &str) -> Vec<String> {
    let mut out = Vec::new();
    for t in spec.split(',') {
        let t = t.trim().to_lowercase();
        if !t.is_empty() && !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

pub struct VlmGroundDetector {
    http: reqwest::blocking::Client,
    base_url: String,
    model: String,
    targets: Vec<String>,
    coords: CoordMode,
}

impl VlmGroundDetector {
    pub fn new(
        base_url: &str,
        model: &str,
        targets: Vec<String>,
        timeout: Duration,
        coords: CoordMode,
    ) -> Result<Self, DetectorError> {
        if base_url.trim().is_empty() {
            return Err(DetectorError::Inference(
                "vlm base_url is empty (set --vlm-base-url or ITEM_VLM_BASE_URL)".into(),
            ));
        }
        if model.trim().is_empty() {
            return Err(DetectorError::Inference(
                "vlm model is empty (set --vlm-model or ITEM_VLM_MODEL)".into(),
            ));
        }
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(timeout)
            .build()
            .map_err(|e| DetectorError::Inference(Box::new(e)))?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            targets,
            coords,
        })
    }

    fn instruction(&self) -> String {
        let coords_note = match self.coords {
            CoordMode::Norm1000 => {
                "xmin/ymin/xmax/ymax are integers normalized to 0-1000 relative to \
                 image width/height"
            }
            CoordMode::Pixel => "xmin/ymin/xmax/ymax are absolute pixel coordinates",
        };
        let task = if self.targets.is_empty() {
            "List every distinct object visible in the image; use a short english \
             name as the label."
                .to_string()
        } else {
            format!(
                "Detect every instance of the following object types: {}.",
                self.targets.join(", ")
            )
        };
        format!(
            "{task} For each instance output one JSON object \
             {{\"label\": \"...\", \"bbox_2d\": [xmin, ymin, xmax, ymax]}} where \
             {coords_note}. Respond with ONLY the JSON array -- no markdown, no \
             explanation. If none are found, respond with []."
        )
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    temperature: f32,
    messages: Vec<Msg>,
}

#[derive(Serialize)]
struct Msg {
    role: &'static str,
    content: Vec<Part>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Part {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

#[derive(Serialize)]
struct ImageUrl {
    url: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: RespMsg,
}

#[derive(Deserialize)]
struct RespMsg {
    #[serde(default)]
    content: Option<String>,
}

/// One grounded object as the sidecar answered it, before scaling.
#[derive(Debug, Deserialize, PartialEq)]
struct RawBox {
    #[serde(default)]
    label: String,
    #[serde(alias = "bbox", alias = "box_2d")]
    bbox_2d: Option<[f64; 4]>,
    #[serde(default)]
    score: Option<f64>,
    #[serde(default, alias = "confidence")]
    confidence: Option<f64>,
}

fn inference_err(e: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> DetectorError {
    DetectorError::Inference(e.into())
}

/// JPEG-encode the frame and wrap it as a data URL (multimodal content part).
fn frame_to_data_url(rgb: &[u8], width: u32, height: u32) -> Result<String, DetectorError> {
    let img = image::RgbImage::from_raw(width, height, rgb.to_vec())
        .ok_or_else(|| inference_err("bad rgb buffer"))?;
    let mut jpeg = Vec::new();
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
        .map_err(|e| inference_err(format!("jpeg encode: {e}")))?;
    Ok(format!("data:image/jpeg;base64,{}", B64.encode(&jpeg)))
}

/// Pull the outermost JSON array out of a chat reply: models love wrapping
/// their JSON in ```json fences and prose. String-aware bracket scan; no
/// array at all (e.g. "I don't see any...") is an empty result, not an error.
fn extract_array(content: &str) -> Result<Vec<RawBox>, DetectorError> {
    let bytes = content.as_bytes();
    let Some(start) = content.find('[') else {
        return Ok(vec![]);
    };
    let (mut depth, mut in_str, mut esc) = (0i32, false, false);
    let mut end = None;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(i + 1);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(end) = end else {
        return Ok(vec![]); // unterminated: treat as "no objects"
    };
    serde_json::from_str::<Vec<RawBox>>(&content[start..end])
        .map_err(|e| inference_err(format!("vlm reply is not a parseable detection array: {e}")))
}

/// Scale raw grounded boxes to pixel-space `Detection`s: auto-detect replies
/// that came back in absolute pixels (any coordinate > 1000), clamp to the
/// frame, order endpoints, normalize labels.
fn to_detections(raw: Vec<RawBox>, width: u32, height: u32, coords: CoordMode) -> Vec<Detection> {
    let (w, h) = (width as f32, height as f32);
    let pixel_reply = raw
        .iter()
        .filter_map(|b| b.bbox_2d.as_ref())
        .flatten()
        .any(|v| *v > 1000.0);
    let norm = !pixel_reply && coords == CoordMode::Norm1000;
    let (sx, sy) = if norm {
        (w / 1000.0, h / 1000.0)
    } else {
        (1.0, 1.0)
    };

    raw.into_iter()
        .filter_map(|b| {
            let [x1, y1, x2, y2] = b.bbox_2d?;
            let (mut x1, mut x2) = ((x1 as f32 * sx), (x2 as f32 * sx));
            let (mut y1, mut y2) = ((y1 as f32 * sy), (y2 as f32 * sy));
            if x1 > x2 {
                std::mem::swap(&mut x1, &mut x2);
            }
            if y1 > y2 {
                std::mem::swap(&mut y1, &mut y2);
            }
            let conf = b.score.or(b.confidence).map(|v| v.clamp(0.0, 1.0) as f32);
            let mut label = b.label.trim().to_lowercase();
            if label.is_empty() {
                label = "object".into();
            }
            Some(Detection {
                label,
                confidence: conf.unwrap_or(1.0),
                bbox: [
                    x1.clamp(0.0, w),
                    y1.clamp(0.0, h),
                    x2.clamp(0.0, w),
                    y2.clamp(0.0, h),
                ],
            })
        })
        .collect()
}

impl super::Detector for VlmGroundDetector {
    fn detect(&self, rgb: &[u8], width: u32, height: u32) -> Result<Vec<Detection>, DetectorError> {
        let data_url = frame_to_data_url(rgb, width, height)?;
        let req = ChatRequest {
            model: &self.model,
            temperature: 0.0,
            messages: vec![Msg {
                role: "user",
                content: vec![
                    Part::ImageUrl {
                        image_url: ImageUrl { url: data_url },
                    },
                    Part::Text {
                        text: self.instruction(),
                    },
                ],
            }],
        };

        let url = format!("{}/chat/completions", self.base_url);
        let resp = self
            .http
            .post(&url)
            .json(&req)
            .send()
            .and_then(|r| r.error_for_status())
            .map_err(|e| inference_err(format!("vlm sidecar {url}: {e}")))?;
        let body: ChatResponse = resp
            .json()
            .map_err(|e| inference_err(format!("vlm sidecar response: {e}")))?;
        let content = body
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| inference_err("vlm returned no choices"))?
            .message
            .content
            .unwrap_or_default();
        tracing::debug!(bytes = content.len(), reply = %content, "vlm reply");

        let raw = extract_array(&content)?;
        Ok(to_detections(raw, width, height, self.coords))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detector::Detector;

    fn det_label_conf(b: &[Detection]) -> Vec<(&str, f32)> {
        b.iter().map(|d| (d.label.as_str(), d.confidence)).collect()
    }

    #[test]
    fn targets_parse_trims_lowercases_dedups() {
        assert_eq!(
            parse_targets(" Remote, keys ,, keys\n, "),
            vec!["remote", "keys"]
        );
        assert!(parse_targets("").is_empty());
    }

    #[test]
    fn extracts_array_from_fenced_prose() {
        let reply = "Sure! Here is what I found:\n```json\n\
            [{\"label\": \"Remote\", \"bbox_2d\": [100, 200, 500, 800]},\n \
            {\"label\":\"keys\",\"bbox\":[1100,600,2200,900],\"score\":0.9}]\n```\n\
            Let me know if you need more.";
        let raw = extract_array(reply).unwrap();
        assert_eq!(raw.len(), 2);
        assert_eq!(raw[1].bbox_2d, Some([1100.0, 600.0, 2200.0, 900.0]));
    }

    #[test]
    fn prose_without_array_is_empty_not_error() {
        assert!(
            extract_array("I don't see any of those objects.")
                .unwrap()
                .is_empty()
        );
        assert!(extract_array("").unwrap().is_empty());
    }

    #[test]
    fn unterminated_array_is_empty_not_error() {
        assert!(
            extract_array("found: [{\"label\":\"x\"}")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn garbage_array_is_error() {
        assert!(extract_array("[{label: no quotes}]").is_err());
    }

    #[test]
    fn norm1000_scales_by_frame_size() {
        let raw = extract_array("[{\"label\":\"remote\",\"bbox_2d\":[100,200,500,800]}]").unwrap();
        let dets = to_detections(raw, 100, 80, CoordMode::Norm1000);
        assert_eq!(dets.len(), 1);
        let d = &dets[0];
        assert_eq!(d.bbox, [10.0, 16.0, 50.0, 64.0]);
        assert_eq!(d.confidence, 1.0); // no score field
    }

    #[test]
    fn reply_over_1000_is_treated_as_pixels() {
        // any coordinate > 1000 flips the whole reply to pixel interpretation
        let raw = extract_array("[{\"label\":\"tv\",\"bbox_2d\":[40,30,1200,150],\"score\":0.9}]")
            .unwrap();
        let dets = to_detections(raw, 1280, 720, CoordMode::Norm1000);
        assert_eq!(dets[0].bbox, [40.0, 30.0, 1200.0, 150.0]);
        assert_eq!(dets[0].confidence, 0.9);
    }

    #[test]
    fn pixel_mode_leaves_coordinates_alone() {
        let raw =
            extract_array("[{\"label\":\"mug\",\"bbox_2d\":[10,10,90,70],\"confidence\":0.5}]")
                .unwrap();
        let dets = to_detections(raw, 100, 80, CoordMode::Pixel);
        assert_eq!(dets[0].bbox, [10.0, 10.0, 90.0, 70.0]);
        assert_eq!(dets[0].confidence, 0.5);
    }

    #[test]
    fn endpoints_are_ordered_and_clamped() {
        let raw =
            extract_array("[{\"label\":\"a\",\"bbox_2d\":[900,900,-50,1500]},{\"label\":\"b\",\"bbox_2d\":[-30,-30,1200,1200]}]")
                .unwrap();
        let dets = to_detections(raw, 100, 80, CoordMode::Pixel);
        // first: >1000 => pixel mode; -50..900 swapped/clamped
        assert_eq!(dets[0].bbox, [0.0, 80.0, 100.0, 80.0]); // y swap+clamp; x clamp
        // degenerate zero-area boxes still pass through (NMS/zone handle them)
        assert_eq!(det_label_conf(&dets), vec![("a", 1.0), ("b", 1.0)]);
    }

    #[test]
    fn empty_label_defaults_to_object() {
        let raw = extract_array("[{\"label\":\"  \",\"bbox_2d\":[1,2,3,4]}]").unwrap();
        let dets = to_detections(raw, 100, 80, CoordMode::Pixel);
        assert_eq!(dets[0].label, "object");
    }

    #[test]
    fn instruction_lists_targets_or_open_mode() {
        let d = VlmGroundDetector::new(
            "http://x/v1",
            "m",
            parse_targets("remote, keys"),
            Duration::from_secs(5),
            CoordMode::Norm1000,
        )
        .unwrap();
        let text = d.instruction();
        assert!(text.contains("remote, keys"));
        assert!(text.contains("0-1000"));
        assert!(!text.contains("every distinct object"));

        let d = VlmGroundDetector::new(
            "http://x/v1",
            "m",
            vec![],
            Duration::from_secs(5),
            CoordMode::Pixel,
        )
        .unwrap();
        let text = d.instruction();
        assert!(text.contains("every distinct object"));
        assert!(text.contains("absolute pixel"));
    }

    #[test]
    fn empty_base_url_or_model_is_constructor_error() {
        assert!(
            VlmGroundDetector::new("", "m", vec![], Duration::from_secs(1), CoordMode::Pixel)
                .is_err()
        );
        assert!(
            VlmGroundDetector::new(
                "http://x/v1",
                " ",
                vec![],
                Duration::from_secs(1),
                CoordMode::Pixel
            )
            .is_err()
        );
    }

    /// Full HTTP round trip against a canned OpenAI-style sidecar on the
    /// loopback: request must carry the data URL + instruction, response
    /// boxes must land in pixel space.
    #[test]
    fn detect_round_trips_a_canned_sidecar() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = vec![0u8; 64 * 1024];
            let mut got = Vec::new();
            // headers
            let mut header_end = 0usize;
            loop {
                let n = sock.read(&mut buf).unwrap();
                assert!(n > 0, "client closed before sending headers");
                got.extend_from_slice(&buf[..n]);
                if let Some(pos) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                    header_end = pos + 4;
                    break;
                }
            }
            let headers = String::from_utf8_lossy(&got[..header_end]).to_lowercase();
            let clen: usize = headers
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .expect("content-length header")
                .trim()
                .parse()
                .unwrap();
            while got.len() < header_end + clen {
                let n = sock.read(&mut buf).unwrap();
                assert!(n > 0, "client closed before sending body");
                got.extend_from_slice(&buf[..n]);
            }
            let req_body = String::from_utf8_lossy(&got[header_end..]).to_string();
            assert!(
                req_body.contains("\"image_url\""),
                "multimodal part missing"
            );
            assert!(
                req_body.contains("data:image/jpeg;base64,"),
                "jpeg not embedded"
            );
            assert!(req_body.contains("remote"), "instruction missing targets");

            let content = "```json\n[{\"label\":\"Remote\",\"bbox_2d\":[100,200,500,800]},\
                           {\"label\":\"keys\",\"bbox_2d\":[100,600,300,900],\"score\":0.88}]\n```";
            let body = serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": content}}]
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).unwrap();
            sock.flush().unwrap();
        });

        let det = VlmGroundDetector::new(
            &format!("http://{addr}/v1"),
            "qwen2.5vl",
            parse_targets("remote, keys"),
            Duration::from_secs(10),
            CoordMode::Norm1000,
        )
        .unwrap();

        // 200x100 flat frame; norm1000 reply scales by sx=0.2 / sy=0.1.
        let frame = vec![128u8; 200 * 100 * 3];
        let dets = det.detect(&frame, 200, 100).unwrap();

        server.join().unwrap();
        assert_eq!(det_label_conf(&dets), vec![("remote", 1.0), ("keys", 0.88)]);
        // remote: 100,200,500,800 -> *0.2 / *0.1
        assert_eq!(dets[0].bbox, [20.0, 20.0, 100.0, 80.0]);
        // keys: 100,600,300,900 -> [20,60,60,90], score kept
        assert_eq!(dets[1].bbox, [20.0, 60.0, 60.0, 90.0]);
    }
}
