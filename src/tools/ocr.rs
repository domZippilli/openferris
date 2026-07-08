use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;

use super::{Tool, files, require_str};

const OCR_TIMEOUT: Duration = Duration::from_secs(180);
const DEFAULT_MAX_ITEMS: u64 = 200;
const HARD_MAX_ITEMS: u64 = 1000;

pub struct OcrImageTool {
    allowed_dirs: Vec<PathBuf>,
}

impl OcrImageTool {
    pub fn new(allowed_dirs: Vec<PathBuf>) -> Self {
        Self { allowed_dirs }
    }
}

fn requested_min_confidence(params: &serde_json::Value) -> Result<f64> {
    match params.get("min_confidence") {
        None | Some(serde_json::Value::Null) => Ok(0.0),
        Some(value) => {
            let confidence = value.as_f64().ok_or_else(|| {
                anyhow::anyhow!("min_confidence must be a number from 0.0 to 1.0")
            })?;
            if !(0.0..=1.0).contains(&confidence) {
                bail!("min_confidence must be between 0.0 and 1.0");
            }
            Ok(confidence)
        }
    }
}

fn requested_max_items(params: &serde_json::Value) -> Result<u64> {
    match params.get("max_items") {
        None | Some(serde_json::Value::Null) => Ok(DEFAULT_MAX_ITEMS),
        Some(value) => {
            let max_items = value
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("max_items must be a positive integer"))?;
            if max_items == 0 {
                bail!("max_items must be greater than zero");
            }
            if max_items > HARD_MAX_ITEMS {
                bail!("max_items may not exceed {}", HARD_MAX_ITEMS);
            }
            Ok(max_items)
        }
    }
}

fn ocr_script() -> &'static str {
    r#"
import json
import sys

from rapidocr_onnxruntime import RapidOCR

path = sys.argv[1]
min_confidence = float(sys.argv[2])
max_items = int(sys.argv[3])

ocr = RapidOCR()
result, elapsed = ocr(path)

items = []
texts = []
for raw in result or []:
    box, text, confidence = raw
    confidence = float(confidence)
    if confidence < min_confidence:
        continue
    item = {
        "text": text,
        "confidence": confidence,
        "bbox": [[float(point[0]), float(point[1])] for point in box],
    }
    items.append(item)
    texts.append(text)
    if len(items) >= max_items:
        break

print(json.dumps({
    "text": "\n".join(texts),
    "items": items,
    "item_count": len(items),
    "truncated": bool(result) and len(result) > len(items),
    "elapsed_seconds": elapsed,
}, ensure_ascii=False))
"#
}

#[async_trait]
impl Tool for OcrImageTool {
    fn name(&self) -> &str {
        "ocr_image"
    }

    fn description_for_llm(&self) -> &str {
        "Run OCR on an image file in the workspace without loading image bytes into context. \
         Parameters: {\"path\": \"<workspace image path>\", \"min_confidence\": <optional 0.0-1.0>, \"max_items\": <optional up to 1000>}. \
         Returns JSON with extracted text, item_count, truncated, elapsed_seconds, and items containing text, confidence, and bbox. \
         Use this after gws.drive.download_file_to_path for screenshots, labels, receipts, documents, and other images containing text."
    }

    async fn execute(&self, params: serde_json::Value) -> Result<String> {
        let path = require_str(&params, "path")?;
        if path.trim().is_empty() {
            bail!("path must not be empty");
        }

        let min_confidence = requested_min_confidence(&params)?;
        let max_items = requested_max_items(&params)?;
        let validated = files::validate_path(path, &self.allowed_dirs)?;

        if !validated.exists() {
            bail!("File not found: {}", path);
        }
        if !validated.is_file() {
            bail!("Not a file: {}", path);
        }

        let output = tokio::time::timeout(
            OCR_TIMEOUT,
            tokio::process::Command::new("uv")
                .args([
                    "run",
                    "--quiet",
                    "--with",
                    "rapidocr-onnxruntime",
                    "--with",
                    "pillow",
                    "python",
                    "-c",
                    ocr_script(),
                ])
                .arg(&validated)
                .arg(min_confidence.to_string())
                .arg(max_items.to_string())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("ocr_image timed out after {:?}", OCR_TIMEOUT))?
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow::anyhow!("uv is not installed; ocr_image requires uv to run RapidOCR")
            } else {
                anyhow::anyhow!("Failed to run OCR helper: {}", e)
            }
        })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            bail!(
                "OCR helper exited with {}: {}{}",
                output.status,
                stdout.trim(),
                stderr.trim()
            );
        }

        let mut result: serde_json::Value =
            serde_json::from_str(stdout.trim()).context("Failed to parse OCR helper output")?;
        if let Some(obj) = result.as_object_mut() {
            obj.insert("path".to_string(), json!(validated.display().to_string()));
        }

        Ok(result.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_requested_min_confidence() {
        assert_eq!(requested_min_confidence(&json!({})).unwrap(), 0.0);
        assert_eq!(
            requested_min_confidence(&json!({"min_confidence": 0.75})).unwrap(),
            0.75
        );
        assert!(requested_min_confidence(&json!({"min_confidence": -0.1})).is_err());
        assert!(requested_min_confidence(&json!({"min_confidence": 1.1})).is_err());
    }

    #[test]
    fn test_requested_max_items() {
        assert_eq!(requested_max_items(&json!({})).unwrap(), DEFAULT_MAX_ITEMS);
        assert_eq!(requested_max_items(&json!({"max_items": 5})).unwrap(), 5);
        assert!(requested_max_items(&json!({"max_items": 0})).is_err());
        assert!(requested_max_items(&json!({"max_items": HARD_MAX_ITEMS + 1})).is_err());
    }
}
