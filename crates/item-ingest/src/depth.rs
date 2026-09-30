//! Optional ordinal relative-depth evidence provider.
//!
//! The provider intentionally answers only same-frame ordering. It never
//! produces metres, calibration, or a physical containment decision.

#[cfg(feature = "relative-depth")]
use std::time::Duration;

use item_core::Detection;
use thiserror::Error;

pub const PROMPT_VERSION: &str = "relative-depth-prompt-v1";
pub const BACKEND_VERSION: &str = "openai-compatible-relative-vlm-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepthRelation {
    Nearer,
    Middle,
    Farther,
    Unknown,
}

impl DepthRelation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Nearer => "nearer",
            Self::Middle => "middle",
            Self::Farther => "farther",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepthQuality {
    Good,
    Degraded,
    Invalid,
}

impl DepthQuality {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Good => "good",
            Self::Degraded => "degraded",
            Self::Invalid => "invalid",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DepthEstimate {
    pub index: usize,
    pub relation_to_camera: DepthRelation,
    /// Ordinal score within this one provider response, not a probability or
    /// distance and not comparable across frames.
    pub relative_score: Option<f32>,
    pub quality: DepthQuality,
    pub frame_id: String,
    pub response_id: String,
    pub prompt_version: String,
    pub backend_version: String,
}

#[derive(Debug, Error)]
pub enum DepthError {
    #[error("relative depth provider: {0}")]
    Message(String),
    #[cfg(feature = "relative-depth")]
    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

pub trait RelativeDepthProvider: Send {
    fn estimate(
        &self,
        rgb: &[u8],
        width: u32,
        height: u32,
        detections: &[Detection],
    ) -> Result<Vec<DepthEstimate>, DepthError>;

    fn provider_name(&self) -> &str;
    fn model_ref(&self) -> Option<&str>;
}

pub struct NoDepthProvider;

impl RelativeDepthProvider for NoDepthProvider {
    fn estimate(
        &self,
        _rgb: &[u8],
        _width: u32,
        _height: u32,
        _detections: &[Detection],
    ) -> Result<Vec<DepthEstimate>, DepthError> {
        Ok(Vec::new())
    }

    fn provider_name(&self) -> &str {
        "none"
    }

    fn model_ref(&self) -> Option<&str> {
        None
    }
}

#[cfg(feature = "relative-depth")]
pub struct RelativeVlmProvider {
    http: reqwest::blocking::Client,
    base_url: String,
    model: String,
}

#[cfg(feature = "relative-depth")]
impl RelativeVlmProvider {
    pub fn new(base_url: &str, model: &str, timeout: Duration) -> Result<Self, DepthError> {
        if base_url.trim().is_empty() {
            return Err(DepthError::Message("depth base_url is empty".into()));
        }
        if model.trim().is_empty() {
            return Err(DepthError::Message("depth model is empty".into()));
        }
        if timeout.is_zero() {
            return Err(DepthError::Message(
                "depth timeout must be greater than zero".into(),
            ));
        }
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(timeout)
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').into(),
            model: model.into(),
        })
    }

    fn prompt(&self, detections: &[Detection]) -> String {
        let boxes = detections
            .iter()
            .enumerate()
            .map(|(i, d)| {
                format!(
                    "{i}: [{:.0},{:.0},{:.0},{:.0}]",
                    d.bbox[0], d.bbox[1], d.bbox[2], d.bbox[3]
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        format!(
            "[{PROMPT_VERSION}] Estimate only ordinal relative depth to the camera for these detected boxes in this image. Boxes are source-pixel coordinates: {boxes}. Return ONLY a JSON array. Each object may contain only index, relation_to_camera, relative_score, and quality. relation_to_camera must be nearer, middle, farther, or unknown. quality must be good, degraded, or invalid. relative_score is optional and must be a finite ordinal in 0..1, never metres, probability, calibration, or a depth map. Omitted box entries mean unknown."
        )
    }
}

#[cfg(feature = "relative-depth")]
impl RelativeDepthProvider for RelativeVlmProvider {
    fn estimate(
        &self,
        rgb: &[u8],
        width: u32,
        height: u32,
        detections: &[Detection],
    ) -> Result<Vec<DepthEstimate>, DepthError> {
        // No detections means there is nothing to ask the provider about.
        if detections.is_empty() {
            return Ok(Vec::new());
        }
        let image = image::RgbImage::from_raw(width, height, rgb.to_vec())
            .ok_or_else(|| DepthError::Message("bad rgb buffer".into()))?;
        let mut jpeg = Vec::new();
        image::DynamicImage::ImageRgb8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut jpeg),
                image::ImageFormat::Jpeg,
            )
            .map_err(|e| DepthError::Message(format!("jpeg encode: {e}")))?;
        let data_url = format!(
            "data:image/jpeg;base64,{}",
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, jpeg)
        );
        let request = serde_json::json!({
            "model": self.model,
            "temperature": 0.0,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": self.prompt(detections)},
                    {"type": "image_url", "image_url": {"url": data_url}}
                ]
            }]
        });
        let response: serde_json::Value = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .json(&request)
            .send()?
            .error_for_status()?
            .json()?;
        let content = response
            .get("choices")
            .and_then(|v| v.get(0))
            .and_then(|v| v.get("message"))
            .and_then(|v| v.get("content"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| DepthError::Message("sidecar response has no text content".into()))?;
        let response_id = response
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("unknown-response")
            .to_string();
        let frame_id = format!("frame-{}", stable_id(rgb));
        parse_response_with_provenance(
            content,
            detections.len(),
            frame_id,
            response_id,
            PROMPT_VERSION.into(),
            BACKEND_VERSION.into(),
        )
    }

    fn provider_name(&self) -> &str {
        "vlm_ordinal"
    }

    fn model_ref(&self) -> Option<&str> {
        Some(&self.model)
    }
}

#[cfg(feature = "relative-depth")]
fn stable_id(bytes: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

#[allow(dead_code)]
pub fn parse_response(content: &str, count: usize) -> Result<Vec<DepthEstimate>, DepthError> {
    parse_response_with_provenance(
        content,
        count,
        "unknown-frame".into(),
        "unknown-response".into(),
        PROMPT_VERSION.into(),
        BACKEND_VERSION.into(),
    )
}

pub fn parse_response_with_provenance(
    content: &str,
    count: usize,
    frame_id: String,
    response_id: String,
    prompt_version: String,
    backend_version: String,
) -> Result<Vec<DepthEstimate>, DepthError> {
    let array = extract_array(content)?;
    let values: Vec<serde_json::Value> = serde_json::from_str(array)
        .map_err(|e| DepthError::Message(format!("invalid depth JSON: {e}")))?;
    let mut seen = vec![false; count];
    let mut out = Vec::with_capacity(values.len());
    for value in values {
        let object = value
            .as_object()
            .ok_or_else(|| DepthError::Message("depth array entries must be objects".into()))?;
        for key in object.keys() {
            if !matches!(
                key.as_str(),
                "index" | "relation_to_camera" | "relative_score" | "quality"
            ) {
                return Err(DepthError::Message(format!(
                    "unsupported depth field '{key}' (metric/calibration fields are forbidden)"
                )));
            }
        }
        let index = object
            .get("index")
            .ok_or_else(|| DepthError::Message("depth item has no index".into()))?
            .as_u64()
            .ok_or_else(|| {
                DepthError::Message("depth index must be a non-negative integer".into())
            })?;
        let index = usize::try_from(index)
            .map_err(|_| DepthError::Message("depth index is too large".into()))?;
        if index >= count || seen[index] {
            return Err(DepthError::Message(format!(
                "duplicate or out-of-range depth index {index}"
            )));
        }
        seen[index] = true;
        let relation = match object.get("relation_to_camera") {
            None => DepthRelation::Unknown,
            Some(v) => match v.as_str() {
                Some("nearer") => DepthRelation::Nearer,
                Some("middle") => DepthRelation::Middle,
                Some("farther") => DepthRelation::Farther,
                Some("unknown") => DepthRelation::Unknown,
                Some(other) => {
                    return Err(DepthError::Message(format!(
                        "invalid depth relation {other}"
                    )));
                }
                None => {
                    return Err(DepthError::Message(
                        "relation_to_camera must be a string".into(),
                    ));
                }
            },
        };
        let relative_score = match object.get("relative_score") {
            None | Some(serde_json::Value::Null) => None,
            Some(v) => {
                let n = v.as_f64().ok_or_else(|| {
                    DepthError::Message("relative_score must be a number or null".into())
                })?;
                if !n.is_finite() || !(0.0..=1.0).contains(&n) {
                    return Err(DepthError::Message(
                        "relative score must be finite and in 0..1".into(),
                    ));
                }
                Some(n as f32)
            }
        };
        let quality = match object.get("quality") {
            None => DepthQuality::Degraded,
            Some(v) => match v.as_str() {
                Some("good") => DepthQuality::Good,
                Some("degraded") => DepthQuality::Degraded,
                Some("invalid") => DepthQuality::Invalid,
                Some(other) => {
                    return Err(DepthError::Message(format!(
                        "invalid depth quality {other}"
                    )));
                }
                None => return Err(DepthError::Message("quality must be a string".into())),
            },
        };
        out.push(DepthEstimate {
            index,
            relation_to_camera: relation,
            relative_score,
            quality,
            frame_id: frame_id.clone(),
            response_id: response_id.clone(),
            prompt_version: prompt_version.clone(),
            backend_version: backend_version.clone(),
        });
    }
    // An omitted entry is explicit unknown evidence, not an implicit nearer or
    // farther claim. Preserve one provenance-bearing result per input box.
    for (index, was_seen) in seen.iter().enumerate().take(count) {
        if !was_seen {
            out.push(DepthEstimate {
                index,
                relation_to_camera: DepthRelation::Unknown,
                relative_score: None,
                quality: DepthQuality::Degraded,
                frame_id: frame_id.clone(),
                response_id: response_id.clone(),
                prompt_version: prompt_version.clone(),
                backend_version: backend_version.clone(),
            });
        }
    }
    out.sort_by_key(|estimate| estimate.index);
    Ok(out)
}

/// Locate one complete JSON array while respecting quoted strings and escapes.
fn extract_array(content: &str) -> Result<&str, DepthError> {
    let bytes = content.as_bytes();
    let mut quoted = false;
    let mut escaped = false;
    let mut start = None;
    let mut nesting = 0usize;
    let mut saw_array = false;
    for (i, byte) in bytes.iter().copied().enumerate() {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
            continue;
        }
        match byte {
            b'"' => quoted = true,
            b'[' if start.is_none() => {
                start = Some(i);
                nesting = 1;
                saw_array = true;
            }
            b'[' if start.is_some() => nesting += 1,
            b']' if start.is_some() => {
                nesting = nesting.saturating_sub(1);
                if nesting == 0 {
                    let begin = start.expect("array start set");
                    let candidate = &content[begin..=i];
                    // A prose bracket may be balanced but not JSON. Continue
                    // scanning so a later fenced response can be recovered.
                    if serde_json::from_str::<serde_json::Value>(candidate)
                        .ok()
                        .is_some_and(|v| v.is_array())
                    {
                        return Ok(candidate);
                    }
                    start = None;
                }
            }
            _ => {}
        }
    }
    if quoted {
        Err(DepthError::Message(
            "depth response has unterminated string".into(),
        ))
    } else if saw_array {
        Err(DepthError::Message(
            "depth response has no valid JSON array".into(),
        ))
    } else {
        Err(DepthError::Message(
            "depth response has no JSON array".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ordinal_response_and_rejects_bad_indices() {
        let good = parse_response(
            "```json [{\"index\":0,\"relation_to_camera\":\"nearer\",\"relative_score\":0.8,\"quality\":\"good\"}] ```",
            2,
        )
        .unwrap();
        assert_eq!(good[0].relation_to_camera, DepthRelation::Nearer);
        assert_eq!(good[1].relation_to_camera, DepthRelation::Unknown);
        assert!(parse_response("[{\"index\":2,\"relation_to_camera\":\"nearer\"}]", 2).is_err());
    }

    #[test]
    fn parser_is_strict_about_types_and_metric_fields() {
        assert!(parse_response("[{\"index\":0,\"relative_score\":\"0.5\"}]", 1).is_err());
        assert!(parse_response("[{\"index\":0,\"quality\":3}]", 1).is_err());
        assert!(parse_response("[{\"index\":0,\"depth_m\":2.0}]", 1).is_err());
        assert!(parse_response("[{\"index\":0},{\"index\":0}]", 1).is_err());
    }

    #[test]
    fn scanner_ignores_brackets_in_strings_and_prose() {
        let parsed = parse_response(
            "model said [not json] then ```json [{\"index\":0,\"relation_to_camera\":\"unknown\"}]```",
            1,
        )
        .unwrap();
        assert_eq!(parsed[0].relation_to_camera, DepthRelation::Unknown);
    }
}
