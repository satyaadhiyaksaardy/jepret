//! Native macOS SF Symbols, rasterized at runtime (nothing bundled).

use image::RgbaImage;
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSImage, NSImageSymbolConfiguration};
use objc2_foundation::{NSDictionary, NSString};

/// Render an SF Symbol as a white-on-transparent mask (tint it when drawing). Must run on the main thread.
pub fn sf_symbol(name: &str, point_size: f64) -> Option<RgbaImage> {
    let img = NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(name), None)?;
    let cfg = NSImageSymbolConfiguration::configurationWithPointSize_weight(point_size, 0.23); // medium weight
    let img = img.imageWithSymbolConfiguration(&cfg)?;
    let tiff = img.TIFFRepresentation()?;
    let rep = NSBitmapImageRep::imageRepWithData(&tiff)?;
    let png = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }?;
    let mut out = image::load_from_memory(&png.to_vec()).ok()?.to_rgba8();
    for p in out.pixels_mut() {
        p.0[..3].copy_from_slice(&[255, 255, 255]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn renders_system_symbol() {
        let img = super::sf_symbol("arrow.up.right", 32.0).expect("SF Symbols available on macOS 11+");
        assert!(img.width() > 10 && img.pixels().any(|p| p.0[3] > 0));
        assert!(super::sf_symbol("definitely.not.a.symbol", 32.0).is_none());
    }
}
