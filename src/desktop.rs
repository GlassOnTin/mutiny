//! Application icon and desktop entry.
//!
//! The icon assets are embedded with `include_bytes!`, so the binary is
//! self-contained the same way the shell-integration snippets are: a fresh
//! build shows its icon without anyone running an installer first, and
//! `mutiny install-desktop` writes the same bytes into the user's hicolor theme
//! and applications directory.
//!
//! Motif and palette follow the Haven sibling icon: a flat white silhouette on
//! deep navy, plain geometric fills, no gradients or filters. See
//! `assets/icon/dev.mutiny.Terminal.svg` for the design notes.

use std::io;
use std::path::{Path, PathBuf};

/// The stable identifier from VISION. Also the GTK application id, the icon
/// name, and the desktop file's basename, so the three cannot drift.
pub const APP_ID: &str = "dev.mutiny.Terminal";

/// Raster sizes shipped in the hicolor theme. 16 is the one that matters most:
/// it is what a taskbar and a window titlebar actually use.
pub const ICON_SIZES: [u32; 7] = [16, 32, 48, 64, 128, 256, 512];

pub const ICON_SVG: &[u8] = include_bytes!("../assets/icon/dev.mutiny.Terminal.svg");

const PNG_16: &[u8] = include_bytes!("../assets/icon/hicolor/16x16/apps/dev.mutiny.Terminal.png");
const PNG_32: &[u8] = include_bytes!("../assets/icon/hicolor/32x32/apps/dev.mutiny.Terminal.png");
const PNG_48: &[u8] = include_bytes!("../assets/icon/hicolor/48x48/apps/dev.mutiny.Terminal.png");
const PNG_64: &[u8] = include_bytes!("../assets/icon/hicolor/64x64/apps/dev.mutiny.Terminal.png");
const PNG_128: &[u8] =
    include_bytes!("../assets/icon/hicolor/128x128/apps/dev.mutiny.Terminal.png");
const PNG_256: &[u8] =
    include_bytes!("../assets/icon/hicolor/256x256/apps/dev.mutiny.Terminal.png");
const PNG_512: &[u8] =
    include_bytes!("../assets/icon/hicolor/512x512/apps/dev.mutiny.Terminal.png");

fn png_for(size: u32) -> Option<&'static [u8]> {
    match size {
        16 => Some(PNG_16),
        32 => Some(PNG_32),
        48 => Some(PNG_48),
        64 => Some(PNG_64),
        128 => Some(PNG_128),
        256 => Some(PNG_256),
        512 => Some(PNG_512),
        _ => None,
    }
}

/// `$XDG_DATA_HOME`, falling back to `~/.local/share`.
pub fn data_home() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_DATA_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| String::from("."));
    PathBuf::from(home).join(".local").join("share")
}

fn cache_home() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_CACHE_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| String::from("."));
    PathBuf::from(home).join(".cache")
}

/// Relative hicolor path for a raster size.
pub fn hicolor_png_rel(size: u32) -> PathBuf {
    PathBuf::from("icons")
        .join("hicolor")
        .join(format!("{size}x{size}"))
        .join("apps")
        .join(format!("{APP_ID}.png"))
}

pub fn hicolor_svg_rel() -> PathBuf {
    PathBuf::from("icons")
        .join("hicolor")
        .join("scalable")
        .join("apps")
        .join(format!("{APP_ID}.svg"))
}

pub fn desktop_rel() -> PathBuf {
    PathBuf::from("applications").join(format!("{APP_ID}.desktop"))
}

/// The freedesktop entry. `StartupWMClass` matches the GTK application id so a
/// running window is associated with this launcher rather than spawning a
/// second, iconless taskbar entry.
pub fn desktop_entry(exec: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Version=1.0\n\
         Name=Mutiny\n\
         GenericName=Terminal\n\
         Comment=Terminal for humans and agents, on the same panes\n\
         Exec={exec}\n\
         Icon={APP_ID}\n\
         Terminal=false\n\
         Categories=System;TerminalEmulator;\n\
         Keywords=terminal;shell;console;tmux;agent;\n\
         StartupNotify=true\n\
         StartupWMClass={APP_ID}\n",
        exec = exec.display()
    )
}

/// Writes a file only when its contents differ, so repeated startups and
/// installs do not churn mtimes (which would make icon caches rebuild).
fn write_if_changed(path: &Path, bytes: &[u8]) -> io::Result<bool> {
    if let Ok(existing) = std::fs::read(path)
        && existing == bytes
    {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)?;
    Ok(true)
}

/// Writes every icon into `root` in hicolor layout. Returns the paths written.
pub fn write_icons(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    for size in ICON_SIZES {
        let Some(bytes) = png_for(size) else { continue };
        let path = root.join(hicolor_png_rel(size));
        if write_if_changed(&path, bytes)? {
            written.push(path);
        }
    }
    let svg = root.join(hicolor_svg_rel());
    if write_if_changed(&svg, ICON_SVG)? {
        written.push(svg);
    }
    Ok(written)
}

/// A private hicolor tree holding the embedded icons, for the GTK icon theme to
/// search. This is what makes the icon appear from a plain `cargo run` without
/// installing anything into the user's directories.
pub fn ensure_icon_search_path() -> io::Result<PathBuf> {
    let root = cache_home().join("mutiny").join("icons");
    // write_icons expects the data-home layout (icons/hicolor/...), so give it
    // a root whose child is already named icons.
    let base = root.parent().map(Path::to_path_buf).unwrap_or(root.clone());
    write_icons(&base)?;
    Ok(root)
}

/// What an install did, for the CLI to report.
#[derive(Debug, Default)]
pub struct InstallReport {
    pub icons: Vec<PathBuf>,
    pub desktop: Option<PathBuf>,
}

/// Installs the icons and the desktop entry under the user's data home.
/// Idempotent: unchanged files are left alone.
pub fn install() -> io::Result<InstallReport> {
    let root = data_home();
    let icons = write_icons(&root)?;
    let exec = std::env::current_exe()?;
    let desktop_path = root.join(desktop_rel());
    let entry = desktop_entry(&exec);
    let changed = write_if_changed(&desktop_path, entry.as_bytes())?;
    Ok(InstallReport {
        icons,
        desktop: if changed { Some(desktop_path) } else { None },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_assets_cover_every_advertised_size() {
        for size in ICON_SIZES {
            let bytes = png_for(size).unwrap_or_else(|| panic!("no PNG for {size}"));
            // PNG magic, then the IHDR width/height big-endian at offset 16.
            assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "size {size}");
            let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
            let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
            assert_eq!(
                (w, h),
                (size, size),
                "embedded PNG {size} has wrong dimensions"
            );
        }
        assert!(ICON_SVG.starts_with(b"<?xml"), "SVG must be XML");
        assert!(
            std::str::from_utf8(ICON_SVG).unwrap().contains("#1A1A2E"),
            "palette must match the Haven sibling icon"
        );
    }

    #[test]
    fn desktop_entry_has_the_required_fields() {
        let entry = desktop_entry(Path::new("/usr/bin/mutiny"));
        for line in [
            "Type=Application",
            "Name=Mutiny",
            "Icon=dev.mutiny.Terminal",
            "Exec=/usr/bin/mutiny",
            "Terminal=false",
            "Categories=System;TerminalEmulator;",
            "StartupWMClass=dev.mutiny.Terminal",
        ] {
            assert!(entry.contains(line), "missing {line:?} in:\n{entry}");
        }
        assert!(entry.starts_with("[Desktop Entry]\n"));
    }

    #[test]
    fn install_writes_a_complete_idempotent_tree() {
        let dir = std::env::temp_dir().join(format!("mutiny-icon-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let written = write_icons(&dir).expect("write icons");
        assert_eq!(
            written.len(),
            ICON_SIZES.len() + 1,
            "7 rasters plus the SVG"
        );
        for size in ICON_SIZES {
            assert!(dir.join(hicolor_png_rel(size)).is_file(), "size {size}");
        }
        assert!(dir.join(hicolor_svg_rel()).is_file());

        // Second pass changes nothing, so icon caches are not invalidated.
        assert!(write_icons(&dir).expect("second pass").is_empty());

        let entry = dir.join(desktop_rel());
        std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
        std::fs::write(&entry, desktop_entry(Path::new("/usr/bin/mutiny"))).unwrap();
        assert!(entry.is_file());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hicolor_paths_use_the_app_id_consistently() {
        for size in ICON_SIZES {
            let rel = hicolor_png_rel(size);
            assert!(rel.ends_with(format!("{APP_ID}.png")));
            assert!(rel.to_string_lossy().contains(&format!("{size}x{size}")));
        }
        assert!(hicolor_svg_rel().ends_with(format!("{APP_ID}.svg")));
        assert!(desktop_rel().ends_with(format!("{APP_ID}.desktop")));
    }
}
