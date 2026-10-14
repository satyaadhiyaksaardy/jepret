//! macOS integration: SF Symbols, app activation, file picker and launch-at-login.
//! AppKit calls must run on the main thread; they are no-ops elsewhere.

use image::RgbaImage;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSApplication, NSApplicationActivationOptions, NSApplicationActivationPolicy, NSBitmapImageFileType,
    NSBitmapImageRep, NSImage, NSImageSymbolConfiguration, NSModalResponseOK, NSOpenPanel, NSRunningApplication,
    NSScreen, NSScreenSaverWindowLevel, NSView, NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask, NSWorkspace,
};
use objc2_foundation::{NSDictionary, NSPoint, NSRect, NSSize, NSString};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::cell::RefCell;
use std::path::PathBuf;

const LOGIN_LABEL: &str = "com.satyaadhiyaksa.snip";

/// Run as a menu-bar app: no Dock icon, no app menu.
pub fn menu_bar_only() {
    if let Some(mtm) = MainThreadMarker::new() {
        NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    }
}

// The app's single window is never hidden: egui stops redrawing hidden windows, so instead the window
// is parked fully transparent and click-through while idle, and reshaped for the overlay or editor.

thread_local! {
    /// App that was frontmost before we took focus, so focus can be handed back on close.
    static PREVIOUS_APP: RefCell<Option<Retained<NSRunningApplication>>> = const { RefCell::new(None) };
}

fn ns_window(handle: &impl HasWindowHandle) -> Option<Retained<NSWindow>> {
    let RawWindowHandle::AppKit(h) = handle.window_handle().ok()?.as_raw() else { return None };
    let view: &NSView = unsafe { h.ns_view.cast().as_ref() };
    view.window()
}

fn take_focus(mtm: MainThreadMarker, window: &NSWindow) {
    let ours = NSRunningApplication::currentApplication();
    if let Some(front) = NSWorkspace::sharedWorkspace().frontmostApplication()
        && front.processIdentifier() != ours.processIdentifier()
    {
        PREVIOUS_APP.with(|p| *p.borrow_mut() = Some(front));
    }
    window.setIgnoresMouseEvents(false);
    window.makeKeyAndOrderFront(None);
    #[allow(deprecated)] // `activate()` needs macOS 14; we support 12+.
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
}

/// Idle: a 1x1 transparent, click-through window kept above everything, out of Mission Control and
/// Cmd+Tab; focus goes back to the previous app. It must never be covered: macOS reports covered
/// windows as occluded and egui then stops rendering, so the next capture would never appear.
pub fn park(handle: &impl HasWindowHandle) {
    let (Some(_mtm), Some(w)) = (MainThreadMarker::new(), ns_window(handle)) else { return };
    w.setAlphaValue(0.0);
    w.setIgnoresMouseEvents(true);
    w.setStyleMask(NSWindowStyleMask::Borderless);
    w.setLevel(NSScreenSaverWindowLevel);
    w.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::Transient
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    w.setFrame_display(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0)), false);
    w.orderFrontRegardless();
    if let Some(prev) = PREVIOUS_APP.with(|p| p.borrow_mut().take()) {
        prev.activateWithOptions(NSApplicationActivationOptions::empty());
    }
}

/// Borderless window covering the whole primary display, above the menu bar and Dock.
/// Stays transparent until `reveal` so the user never sees an unpainted frame.
pub fn cover_screen(handle: &impl HasWindowHandle) {
    let (Some(mtm), Some(w)) = (MainThreadMarker::new(), ns_window(handle)) else { return };
    let Some(screen) = NSScreen::screens(mtm).firstObject() else { return };
    w.setStyleMask(NSWindowStyleMask::Borderless);
    w.setLevel(NSScreenSaverWindowLevel);
    w.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::Transient
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    w.setFrame_display(screen.frame(), true);
    take_focus(mtm, &w);
}

/// Regular titled, resizable window with the given content size (points), centered.
pub fn editor_window(handle: &impl HasWindowHandle, width: f64, height: f64) {
    let (Some(mtm), Some(w)) = (MainThreadMarker::new(), ns_window(handle)) else { return };
    w.setStyleMask(
        NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Miniaturizable | NSWindowStyleMask::Resizable,
    );
    w.setLevel(0);
    w.setCollectionBehavior(NSWindowCollectionBehavior::Managed);
    w.setTitle(&NSString::from_str("Snip"));
    w.setContentSize(NSSize::new(width, height));
    w.center();
    take_focus(mtm, &w);
}

/// Make the window visible once egui has painted it at its new size.
pub fn reveal(handle: &impl HasWindowHandle) {
    if let (Some(_), Some(w)) = (MainThreadMarker::new(), ns_window(handle)) {
        w.setAlphaValue(1.0);
    }
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
