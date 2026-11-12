//! The app icon (icons/baselynx.svg, rendered by icons/render.py).
//!
//! Windows takes it from the executable's resources (build.rs embeds icons/baselynx.ico as
//! icon 1, which GPUI loads for every window). The others get it at run time from the PNG
//! embedded here: the Dock on macOS, the window on X11, and on Linux a desktop entry, which
//! is how Wayland compositors and app launchers find an app's icon.

#[cfg_attr(target_os = "windows", allow(dead_code))]
const PNG: &[u8] = include_bytes!("../icons/baselynx.png");

/// The window icon, where the platform takes one per window (X11).
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub fn apply(options: &mut gpui_kit::WindowOptions) {
    options.icon = image::load_from_memory_with_format(PNG, image::ImageFormat::Png)
        .ok()
        .map(|i| std::sync::Arc::new(i.into_rgba8()));
}

#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
pub fn apply(_options: &mut gpui_kit::WindowOptions) {}

/// Once per start, on the main thread, before the first window.
pub fn install() {
    #[cfg(target_os = "macos")]
    dock_icon();
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    desktop_entry();
}

/// A program started outside an `.app` bundle shows a generic icon in the Dock; give it ours.
#[cfg(target_os = "macos")]
fn dock_icon() {
    use objc2::{AllocAnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;
    let Some(mtm) = MainThreadMarker::new() else { return };
    let data = NSData::with_bytes(PNG);
    let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else { return };
    #[allow(unused_unsafe)]
    unsafe {
        NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image))
    };
}

/// `~/.local/share/applications/baselynx.desktop` and its icon, pointing at this binary:
/// Wayland shows the icon of the entry named after the window's app id, and the entry puts
/// baselynx in the app launcher. Rewritten only when it changes (the binary moved, a new
/// icon); builds run from a `target` folder leave the system alone.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn desktop_entry() {
    use std::path::{Component, PathBuf};
    let Ok(exe) = std::env::current_exe() else { return };
    if exe.components().any(|c| c == Component::Normal("target".as_ref())) {
        return;
    }
    let data = match std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => match std::env::var_os("HOME") {
            Some(h) => PathBuf::from(h).join(".local/share"),
            None => return,
        },
    };
    let exec = exe.display().to_string().replace('\\', "\\\\").replace('"', "\\\"");
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=baselynx\n\
         Comment=Desktop client for PostgreSQL, MySQL and SQLite\n\
         Exec=\"{exec}\"\nIcon=baselynx\nTerminal=false\nCategories=Development;Database;\n\
         StartupWMClass=baselynx\n"
    );
    write_if_changed(&data.join("icons/hicolor/512x512/apps/baselynx.png"), PNG);
    write_if_changed(&data.join("applications/baselynx.desktop"), entry.as_bytes());
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn write_if_changed(path: &std::path::Path, bytes: &[u8]) {
    if std::fs::read(path).is_ok_and(|old| old == bytes) {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Best effort: an icon is not worth failing a start over.
    let _ = std::fs::write(path, bytes);
}
