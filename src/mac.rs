//! macOS integration: SF Symbols, app activation, file picker and launch-at-login.
//! AppKit calls must run on the main thread; they are no-ops elsewhere.

use image::RgbaImage;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBitmapImageFileType, NSBitmapImageRep, NSImage,
    NSImageSymbolConfiguration, NSModalResponseOK, NSOpenPanel,
};
use objc2_foundation::{NSDictionary, NSString};
use std::path::PathBuf;

const LOGIN_LABEL: &str = "com.satyaadhiyaksa.snip";

/// Run as a menu-bar app: no Dock icon, no app menu.
pub fn menu_bar_only() {
    if let Some(mtm) = MainThreadMarker::new() {
        NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    }
}

/// Show our windows and take focus. Done through AppKit directly because egui does not
/// redraw a hidden window, so it cannot process a "show" request on its own.
pub fn bring_to_front() {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let app = NSApplication::sharedApplication(mtm);
    for w in app.windows().iter() {
        w.makeKeyAndOrderFront(None);
    }
    #[allow(deprecated)] // `activate()` needs macOS 14; we support 12+.
    app.activateIgnoringOtherApps(true);
}

/// Native "Open" dialog. Returns the chosen file, if any.
pub fn pick_file() -> Option<PathBuf> {
    let mtm = MainThreadMarker::new()?;
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseDirectories(false);
    panel.setAllowsMultipleSelection(false);
    if panel.runModal() != NSModalResponseOK {
        return None;
    }
    Some(PathBuf::from(panel.URL()?.path()?.to_string()))
}

/// Opens System Settings at Privacy & Security → Screen Recording.
pub fn open_screen_recording_settings() {
    let url = "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";
    std::process::Command::new("open").arg(url).spawn().ok();
}

fn login_agent_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(format!("Library/LaunchAgents/{LOGIN_LABEL}.plist"))
}

pub fn launch_at_login() -> bool {
    login_agent_path().exists()
}

/// Launch at login via a per-user LaunchAgent pointing at the current executable.
/// (SMAppService is the modern API but requires a signed, bundled app.)
pub fn set_launch_at_login(enabled: bool) -> std::io::Result<()> {
    let path = login_agent_path();
    if !enabled {
        return std::fs::remove_file(path).or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) });
    }
    let exe = std::env::current_exe()?;
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{LOGIN_LABEL}</string>
    <key>ProgramArguments</key><array><string>{}</string></array>
    <key>RunAtLoad</key><true/>
</dict>
</plist>
"#,
        exe.display()
    );
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, plist)
}

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
