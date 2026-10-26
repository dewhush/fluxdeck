//! Pure-sysfs display discovery. Kernel namespace only; DRM connector names are authoritative.

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayKind {
    Backlight,
    Ddc,
}

#[derive(Debug, Clone)]
pub struct Display {
    pub kind: DisplayKind,
    pub name: String, // "Laptop panel" or connector name like "HDMI-A-1"
    pub key: String,  // "bl:{sysfs path}" or "ddc:{bus}:{conn}"
    pub max: u32,     // backlight max_brightness; DDC typical 100
}

/// Parse trailing `i2c-N` component of a path into N.
fn parse_i2c_bus(path: &Path) -> Option<u32> {
    path.components().next_back()?.as_os_str().to_str()?.strip_prefix("i2c-")?.parse().ok()
}

/// Find the I2C bus number for a DRM connector dir: `i2c-N` entry first,
/// then `ddc` symlink resolved (kernel: HDMI connectors expose ddc, not i2c-N).
fn find_i2c_bus(dir: &Path) -> Option<u32> {
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(n) = name.strip_prefix("i2c-") {
            if n.parse::<u32>().is_ok() {
                return n.parse().ok();
            }
        }
    }
    let ddc = fs::read_link(dir.join("ddc")).ok()?;
    // Relative links resolve against the connector dir.
    let realpath = if ddc.is_absolute() { ddc } else { normalize(dir.join(ddc)) };
    parse_i2c_bus(&realpath)
}

/// Minimal lexical normalization (..\.. walks) without /proc dependence.
fn normalize(p: PathBuf) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// Does a backlight entry name indicate the laptop's internal panel?
fn is_laptop_panel(entry: &str, total: usize) -> bool {
    entry.contains("amdgpu") || total == 1
}
/// Best-effort monitor name from the connector's EDID (Descriptor tag 0xFC
/// "monitor name", falling back to 0xFE ASCII string). None = none found.
fn edid_name(dir: &Path) -> Option<String> {
    let data = fs::read(dir.join("edid")).ok()?;
    if data.len() < 128 {
        return None;
    }
    let mut fallback = None;
    for base in (0..data.len()).step_by(128) {
        let end = (base + 128).min(data.len());
        let blk = &data[base..end];
        if blk.len() < 128 {
            break;
        }
        for i in 0..4 {
            let d = &blk[54 + i * 18..72 + i * 18];
            if d[0] == 0 && d[1] == 0 && d[2] == 0 && d[4] == 0 && (d[3] == 0xFC || d[3] == 0xFE) {
                let nm = String::from_utf8_lossy(&d[5..18])
                    .trim_end_matches(['\n', '\x00', ' '])
                    .trim()
                    .to_string();
                if !nm.is_empty() {
                    if d[3] == 0xFC {
                        return Some(nm); // official name beats the ASCII fallback
                    }
                    fallback = fallback.or(Some(nm));
                }
            }
        }
    }
    fallback
}


pub fn discover() -> Vec<Display> {
    let mut out = Vec::new();

    // Backlight first.
    let bls: Vec<String> = fs::read_dir("/sys/class/backlight")
        .map(|rd| rd.flatten().filter_map(|e| e.file_name().into_string().ok()).collect())
        .unwrap_or_default();
    for name in &bls {
        let base = PathBuf::from("/sys/class/backlight").join(name);
        let max = fs::read_to_string(base.join("max_brightness"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(255);
        out.push(Display {
            kind: DisplayKind::Backlight,
            name: if is_laptop_panel(name, bls.len()) {
                // Prefer the panel's EDID product name (e.g. "INX FHD").
                fs::read_dir("/sys/class/drm")
                    .ok()
                    .map(|rd| {
                        rd.flatten()
                            .filter_map(|e| e.file_name().into_string().ok())
                            .find(|n| n.contains("-eDP"))
                            .and_then(|n| edid_name(&PathBuf::from("/sys/class/drm").join(n)))
                    })
                    .flatten()
                    .unwrap_or_else(|| "Laptop Panel".into())
            } else {
                name.clone()
            },
            key: format!("bl:{}", base.display()),
            max,
        });
    }

    // DRM connectors -> DDC.
    let mut drm: Vec<String> = fs::read_dir("/sys/class/drm")
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.starts_with("card") && n.contains('-'))
                .collect()
        })
        .unwrap_or_default();
    drm.sort();

    for cardconn in drm {
        let (card, conn) = match cardconn.split_once('-') {
            Some(x) => x,
            None => continue,
        };
        let _ = card;
        let dir = PathBuf::from("/sys/class/drm").join(&cardconn);
        if !conn.starts_with("eDP") {
            if let Ok(status) = fs::read_to_string(dir.join("status")) {
                if status.trim() != "connected" {
                    continue;
                }
            } else {
                continue;
            }
        } else {
            continue; // laptop panel handled by BacklightDisplay
        }

        let bus = match find_i2c_bus(&dir) {
            Some(b) => b,
            None => continue, // Writeback etc.
        };
        // Skip if the bus can't be opened.
        // ponytail: only require the device node to exist; open is retried
        // lazily by DdcHandle on EACCES (user not yet in i2c group).
        if !PathBuf::from(format!("/dev/i2c-{bus}")).exists() {
            continue;
        }

        out.push(Display {
            kind: DisplayKind::Ddc,
            name: edid_name(&dir).unwrap_or_else(|| conn.to_string()),
            key: format!("ddc:{bus}:{conn}"),
            max: 100,
        });
    }

    // Backlight already pushed first; keep DDC sorted by bus number.
    out.sort_by_key(|d| match d.kind {
        DisplayKind::Backlight => (0u8, 0u32),
        DisplayKind::Ddc => {
            let bus = d.key.split(':').nth(1).and_then(|s| s.parse().ok()).unwrap_or(u32::MAX);
            (1u8, bus)
        }
    });
    out
}

/// UI label for a DDC display: bare connector if openable, permission hint on EACCES.
pub fn ddc_label(conn: &str, bus: u16) -> String {
    match fs::OpenOptions::new().read(true).write(true).open(format!("/dev/i2c-{bus}")) {
        Ok(_) => conn.to_string(),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            format!("{conn} (no permission — run install.sh)")
        }
        Err(_) => conn.to_string(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn parse_i2c_bus_trailing_component() {
        assert_eq!(parse_i2c_bus(&PathBuf::from("/sys/devices/pci0/i2c-5")), Some(5));
        assert_eq!(parse_i2c_bus(&PathBuf::from("i2c-12")), Some(12));
        assert_eq!(parse_i2c_bus(&PathBuf::from("/i2c-x")), None);
        assert_eq!(parse_i2c_bus(&PathBuf::from("/dev/tty")), None);
    }

    #[test]
    fn ddc_label_hint() {
        // Nonexistent bus: label degrades to bare connector (no panic, no hint).
        assert_eq!(ddc_label("HDMI-A-1", 9999), "HDMI-A-1");
        let l = ddc_label("DP-1", 15999);
        assert!(l == "DP-1" || l.contains("no permission — run install.sh"));
    }

    #[test]
    fn laptop_panel_heuristic() {
        assert!(is_laptop_panel("amdgpu_bl0", 3));
        assert!(is_laptop_panel("intel_backlight", 1));
        assert!(!is_laptop_panel("acpi_video0", 2));
    }
}
