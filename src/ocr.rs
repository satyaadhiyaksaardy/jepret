//! Text and QR/barcode recognition with Apple's Vision framework.

use image::RgbaImage;
use objc2::AnyThread;
use objc2::rc::{Retained, autoreleasepool};
use objc2_foundation::{NSArray, NSData, NSDictionary};
use objc2_vision::{
    VNDetectBarcodesRequest, VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel,
};
use std::sync::mpsc::{Receiver, channel};

/// Recognized text lines (in Vision's reading order) followed by any barcode/QR payloads.
pub fn recognize(img: &RgbaImage) -> Vec<String> {
    let mut png = Vec::new();
    if img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).is_err() {
        return vec![];
    }
    autoreleasepool(|_| {
        let data = NSData::with_bytes(&png);
        let handler = VNImageRequestHandler::initWithData_options(VNImageRequestHandler::alloc(), &data, &NSDictionary::new());
        let text = VNRecognizeTextRequest::new();
        text.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
        text.setUsesLanguageCorrection(true);
        let codes = unsafe { VNDetectBarcodesRequest::new() }; // SAFETY: plain allocation + init
        let requests: Retained<NSArray<VNRequest>> = NSArray::from_slice(&[text.as_ref(), codes.as_ref()]);
        if let Err(e) = handler.performRequests_error(&requests) {
            eprintln!("OCR failed: {e:?}");
            return vec![];
        }
        let mut out: Vec<String> = text
            .results()
            .map(|r| r.iter().filter_map(|o| o.topCandidates(1).firstObject().map(|c| c.string().to_string())).collect())
            .unwrap_or_default();
        // SAFETY: plain property reads on results returned by a completed request.
        if let Some(found) = unsafe { codes.results() } {
            out.extend(found.iter().filter_map(|o| unsafe { o.payloadStringValue() }.map(|s| s.to_string())));
        }
        out
    })
}

/// Run recognition on a background thread so the UI stays responsive.
pub fn spawn(img: RgbaImage) -> Receiver<Vec<String>> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        tx.send(recognize(&img)).ok();
    });
    rx
}

/// Copy recognized lines to the clipboard; returns a short message for the user.
pub fn copy_result(lines: &[String]) -> String {
    if lines.is_empty() {
        return "No text or QR code found".into();
    }
    let text = lines.join("\n");
    arboard::Clipboard::new().and_then(|mut cb| cb.set_text(&text)).map_err(|e| eprintln!("copy failed: {e}")).ok();
    let preview: String = text.chars().take(60).collect::<String>().replace('\n', " ");
    let more = if text.chars().count() > 60 { "…" } else { "" };
    format!("Copied: {preview}{more}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::{Mark, Shape, flatten};

    #[test]
    fn recognizes_rendered_text() {
        let base = RgbaImage::from_pixel(600, 120, image::Rgba([255, 255, 255, 255]));
        let mark = Mark { shape: Shape::Text { at: [20.0, 30.0], text: "Hello Jepret 2026".into() }, color: [0, 0, 0], width: 6.0 };
        let lines = recognize(&flatten(&base, &[mark]));
        assert!(lines.iter().any(|l| l.contains("Hello Jepret")), "got {lines:?}");
        assert!(recognize(&base).is_empty());
    }
}
