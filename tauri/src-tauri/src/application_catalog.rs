#[cfg(target_os = "macos")]
use objc2::AllocAnyThread;
#[cfg(target_os = "macos")]
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSDeviceRGBColorSpace, NSGraphicsContext, NSWorkspace,
};
#[cfg(target_os = "macos")]
use objc2_foundation::{
    NSBundle, NSDictionary, NSNumber, NSPoint, NSRect, NSSize, NSString, NSURL,
};
use serde::Serialize;
#[cfg(target_os = "macos")]
use std::collections::HashSet;
#[cfg(target_os = "macos")]
use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
const HOPP_BUNDLE_ID: &str = "com.hopp.app";
pub(crate) const NOTIFICATION_CENTER_BUNDLE_ID: &str = "com.apple.notificationcenterui";

#[derive(Debug, Clone, Serialize)]
pub struct InstalledApplication {
    pub bundle_id: String,
    pub name: String,
    pub icon_png: Option<Vec<u8>>,
}

#[cfg(target_os = "macos")]
pub fn list_installed_applications() -> Vec<InstalledApplication> {
    let workspace = NSWorkspace::sharedWorkspace();
    let mut paths = application_paths();
    let notification_center = NSString::from_str(NOTIFICATION_CENTER_BUNDLE_ID);
    if let Some(url) = workspace.URLForApplicationWithBundleIdentifier(&notification_center) {
        if let Some(path) = url.path() {
            paths.push(PathBuf::from(path.to_string()));
        }
    }

    let mut seen = HashSet::new();
    let mut applications = paths
        .into_iter()
        .filter_map(|path| application_at_path(&workspace, &path))
        .filter(|application| seen.insert(application.bundle_id.clone()))
        .collect::<Vec<_>>();
    applications.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.bundle_id.cmp(&right.bundle_id))
    });
    applications
}

#[cfg(not(target_os = "macos"))]
pub fn list_installed_applications() -> Vec<InstalledApplication> {
    Vec::new()
}

#[cfg(target_os = "macos")]
fn application_paths() -> Vec<PathBuf> {
    let mut roots = vec![
        PathBuf::from("/Applications"),
        PathBuf::from("/System/Applications"),
    ];
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join("Applications"));
    }

    let mut applications = vec![PathBuf::from(
        "/System/Library/CoreServices/NotificationCenter.app",
    )];
    applications.extend(app_bundles_in(roots));
    applications
}

/// The `.app` bundles in `roots` and their subfolders. Follows a symlink only
/// when it is itself an app: Safari in /Applications is a symlink into the
/// system's Cryptex volume. Symlinked folders are skipped, so links can't loop.
#[cfg(target_os = "macos")]
fn app_bundles_in(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut applications = Vec::new();
    let mut pending = roots;
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            let is_app = path.extension().is_some_and(|extension| extension == "app");
            if file_type.is_symlink() {
                if is_app && path.is_dir() {
                    applications.push(path);
                }
            } else if is_app {
                applications.push(path);
            } else if file_type.is_dir() {
                pending.push(path);
            }
        }
    }
    applications
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn finds_symlinked_apps_without_following_symlinked_folders() {
        let root = std::env::temp_dir().join(format!("hopp-app-catalog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let system = root.join("System");
        let applications = root.join("Applications");
        std::fs::create_dir_all(system.join("Safari.app")).unwrap();
        std::fs::create_dir_all(applications.join("Utilities/Terminal.app")).unwrap();
        std::fs::create_dir_all(applications.join("Firefox.app")).unwrap();
        std::os::unix::fs::symlink(system.join("Safari.app"), applications.join("Safari.app"))
            .unwrap();
        std::os::unix::fs::symlink(&root, applications.join("Loop")).unwrap();

        let mut names = app_bundles_in(vec![applications])
            .into_iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        let _ = std::fs::remove_dir_all(&root);

        assert_eq!(names, ["Firefox.app", "Safari.app", "Terminal.app"]);
    }
}

#[cfg(target_os = "macos")]
fn application_at_path(workspace: &NSWorkspace, path: &Path) -> Option<InstalledApplication> {
    let path_string = NSString::from_str(path.to_str()?);
    let url = NSURL::fileURLWithPath(&path_string);
    let bundle = NSBundle::bundleWithURL(&url)?;
    let bundle_id = bundle.bundleIdentifier()?.to_string();
    if bundle_id == HOPP_BUNDLE_ID
        || (bundle_id != NOTIFICATION_CENTER_BUNDLE_ID
            && (bool_info(&bundle, "LSUIElement") || bool_info(&bundle, "LSBackgroundOnly")))
    {
        return None;
    }

    let name = string_info(&bundle, "CFBundleDisplayName")
        .or_else(|| string_info(&bundle, "CFBundleName"))
        .or_else(|| {
            path.file_stem()
                .map(|name| name.to_string_lossy().into_owned())
        })?;
    let icon_png = icon_png(workspace, &path_string);

    Some(InstalledApplication {
        bundle_id,
        name,
        icon_png,
    })
}

#[cfg(target_os = "macos")]
fn icon_png(workspace: &NSWorkspace, path: &NSString) -> Option<Vec<u8>> {
    let icon = workspace.iconForFile(path);
    let bitmap = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            64,
            64,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            0,
            0,
        )
    }?;
    let context = NSGraphicsContext::graphicsContextWithBitmapImageRep(&bitmap)?;
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&context));
    icon.drawInRect(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(64.0, 64.0)));
    NSGraphicsContext::restoreGraphicsState_class();
    unsafe {
        bitmap.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }
    .map(|data| data.to_vec())
}

#[cfg(target_os = "macos")]
fn string_info(bundle: &NSBundle, key: &str) -> Option<String> {
    let value = bundle.objectForInfoDictionaryKey(&NSString::from_str(key))?;
    value
        .downcast::<NSString>()
        .ok()
        .map(|value| value.to_string())
}

#[cfg(target_os = "macos")]
fn bool_info(bundle: &NSBundle, key: &str) -> bool {
    let Some(value) = bundle.objectForInfoDictionaryKey(&NSString::from_str(key)) else {
        return false;
    };
    value
        .downcast::<NSNumber>()
        .ok()
        .is_some_and(|value| value.boolValue())
}
