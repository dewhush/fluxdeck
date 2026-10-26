//! Backend trait + sysfs/logind backlight and DDC/CI handles.
//! All blocking I/O lives here; the GUI thread only ever calls via the worker thread.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use std::process::Command;

/// Serializes ALL DDC transactions (consistent with the Python version:
/// one DDC bus lane at a time, across every monitor handle).
static DDC_LOCK: Mutex<()> = Mutex::new(());
/// Instant of the last DDC I/O; pacing sleeps get() until last+50ms.
static LAST_IO: Mutex<Option<Instant>> = Mutex::new(None);
/// Panel's VCP brightness max scale (learned from GetVCP replies; default 100).
static MAX_VCP: Mutex<u32> = Mutex::new(100);
pub const DDC_ADDR: u16 = 0x37;
const I2C_SLAVE: u64 = 0x0703;
const PACE_MS: u64 = 50;
/// After a write: how long to wait before reading the reply; 80ms covers
/// slow panels that return mid-transition frames at shorter waits.
const WAIT_MS: u64 = 80;

pub trait Backend: Send {
    /// 0..=100 percent; -1 on failure.
    fn get(&self) -> i32;
    /// Clamped 0..=100; best-effort.
    fn set(&self, pct: u32);
}

extern "C" {
    fn ioctl(fd: i32, request: u64, ...) -> i32;
}

fn pace() {
    let last = LAST_IO.lock().unwrap();
    if let Some(t) = *last {
        drop(last);
        let elapsed = t.elapsed();
        if elapsed < Duration::from_millis(PACE_MS) {
            std::thread::sleep(Duration::from_millis(PACE_MS) - elapsed);
        }
    }
}

fn stamp_io() {
    *LAST_IO.lock().unwrap() = Some(Instant::now());
}

/// DDC/CI checksum: XOR of src(0x51), dst(0x6E) and every byte on the wire
/// after the source address (length-with-0x80-control byte + payload).
fn ddc_ck(frame: &[u8]) -> u8 {
    frame.iter().fold(0x51u8 ^ 0x6Eu8, |acc, b| acc ^ b)
}

pub struct BacklightHandle {
    name: String,
    base: String,
    max: u32,
}

impl BacklightHandle {
    pub fn new(name: String, base: String, max: u32) -> Self {
        Self { name, base, max }
    }
    fn read_pct(&self) -> Option<i32> {
        let cur: u32 = fs::read_to_string(format!("{}/brightness", self.base))
            .ok()
            .and_then(|s| s.trim().parse().ok())?;
        if self.max == 0 {
            return None;
        }
        Some((((cur * 100 + self.max / 2) / self.max).min(100)) as i32)
    }
}

impl Backend for BacklightHandle {
    fn get(&self) -> i32 {
        self.read_pct().unwrap_or(-1)
    }

    fn set(&self, pct: u32) {
        let pct = pct.min(100);
        let val = ((pct as u64 * self.max as u64 + 50) / 100) as u32;
        // Preferred: logind SetBrightness (no root needed); fallback: direct sysfs write.
        let xs = Command::new("busctl")
            .args([
                "call",
                "org.freedesktop.login1",
                "/org/freedesktop/login1/session/auto",
                "org.freedesktop.login1.Session",
                "SetBrightness",
                "ssu",
                "backlight",
                self.base.rsplit('/').next().unwrap_or(&self.name),
                &val.to_string(),
            ])
            .output();
        let ok = matches!(&xs, Ok(o) if o.status.success());
        if !ok {
            let _ = fs::write(format!("{}/brightness", self.base), format!("{val}\n"));
        }
    }
}

pub struct DdcHandle {
    bus: u16,
    file: Mutex<Option<File>>, // None while /dev/i2c-N is unopenable (e.g. EACCES)
}

impl DdcHandle {
    pub fn new(bus: u16, conn: String) -> Self {
        // ponytail: conn arg kept for stable ctor signature; label lives in discovery Display.key.
        let _ = conn;
        let file = Self::open(bus);
        Self { bus, file: Mutex::new(file) }
    }

    fn open(bus: u16) -> Option<File> {
        match OpenOptions::new().read(true).write(true).open(format!("/dev/i2c-{bus}")) {
            Ok(f) => {
                let fd = std::os::fd::AsRawFd::as_raw_fd(&f);
                unsafe { ioctl(fd, I2C_SLAVE, DDC_ADDR as u64) };
                Some(f)
            }
            Err(e) => {
                // First failure only: repeated retries would spam the log.
                if e.kind() == std::io::ErrorKind::PermissionDenied {
                    eprintln!("fluxdeck: /dev/i2c-{bus}: permission denied (add yourself to the i2c group — see install.sh)");
                }
                None
            }
        }
    }

    fn transact(&self, msg: &[u8]) -> Option<Vec<u8>> {
        let mut guard = self.file.lock().ok()?; // per-handle open/mutex
        if guard.is_none() {
            // Lazy retry: perms may have arrived (i2c group, udev rule) since launch.
            *guard = Self::open(self.bus);
        }
        let f = guard.as_mut()?;
        let _lane = DDC_LOCK.lock().unwrap(); // global DDC lane lock
        pace();
        if f.write_all(msg).is_err() {
            Self::device_gone(&mut guard);
            return None;
        }
        // DDC spec floor is 40ms; 80 avoids reading mid-transition frames
        // seen on some panels.
        std::thread::sleep(Duration::from_millis(WAIT_MS));
        let mut raw = [0u8; 40];
        if f.read(&mut raw).is_err() {
            Self::device_gone(&mut guard);
            return None;
        }
        stamp_io();
        let end = raw.iter().rposition(|&b| b != 0).map_or(11, |p| p + 1).max(11);
        Some(raw[..end.min(raw.len())].to_vec())
    }

    /// Cached monitor max for VCP brightness; learned from first GetVCP reply.
    /// Most panels use 100; a few use 0xFFFF. Defaults to 100 until known.
    fn max_value(&self) -> Option<u32> {
        MAX_VCP.lock().ok().map(|m| *m)
    }

    /// Device vanished mid-run (unplug, driver reload): drop the fd so the
    /// next transact lazily reopens /dev/i2c-N.
    fn device_gone(guard: &mut Option<File>) {
        *guard = None;
    }
}
impl Backend for DdcHandle {
    fn get(&self) -> i32 {
        // GetVCP request: [0x51][len=0x82][0x01][vcp][ck]. Multi-probe format
        // (0x01 = "one VCP code follows"); ck covers dst+len+payload.
        let ck = ddc_ck(&[0x82, 0x01, 0x10]);
        let msg = [0x51, 0x82, 0x01, 0x10, ck];
        let raw = match self.transact(&msg) {
            Some(r) => r,
            None => return -1,
        };
        parse_get_reply(&raw).unwrap_or(-1)
    }

    fn set(&self, pct: u32) {
        // SetVCP request: [0x51][len=0x84][0x03][vcp][hi][lo][ck]. Value is in
        // the monitor's natural 0..max scale (max usually 100), NOT 0xFFFF.
        let v = (u32::from(pct.min(100)) * self.max_value().unwrap_or(100) / 100) as u16;
        let (hi, lo) = ((v >> 8) as u8, (v & 0xFF) as u8);
        let ck = ddc_ck(&[0x84, 0x03, 0x10, hi, lo]);
        let msg = [0x51, 0x84, 0x03, 0x10, hi, lo, ck];
        let _ = self.transact(&msg); // best-effort; verify by get()
    }
}

/// Reply frame: [0x6E, len, 0x02, result, vcp, type, max_hi, max_lo, cur_hi,
/// cur_lo, ck(, pad)] => cur = reply[8..10], max = reply[6..8].
fn parse_get_reply(raw: &[u8]) -> Option<i32> {
    if raw.len() < 11 {
        return None;
    }
    if raw[0] != 0x6E || raw[2] != 0x02 {
        return None;
    }
    if raw[3] != 0 {
        return None; // unsupported/REJECT
    }
    // Reply ck variant observed on real panels (Xiaomi et al.): XOR of all
    // frame bytes [0..9] plus 0x50, vs the canonical XOR(0x51, 0x6E, rest).
    // Accept either; both are XOR distances of 0x50 from each other on 0x88
    // headers. Requests we SEND always use the canonical form.
    let body_xor = raw[..10].iter().fold(0u8, |a, b| a ^ b);
    if body_xor ^ 0x50 != raw[10] && ddc_ck(&raw[1..10]) != raw[10] {
        return None;
    }
    let mx = ((raw[6] as u32) << 8) | raw[7] as u32;
    let cur = ((raw[8] as u32) << 8) | raw[9] as u32;
    if mx == 0 || mx > 0xFFFF {
        return None;
    }
    // Learn the panel's real max for set() scaling.
    if let Ok(mut m) = MAX_VCP.lock() {
        *m = mx;
    }
    let pct = ((cur * 100 + mx / 2) / mx).min(100) as i32;
    Some(pct)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddc_frame_formats_match_ddcutil() {
        // Captured from a working ddcutil session (kernel i2c trace):
        // GetVCP 0x10 request [51 82 01 10 ac], ck = XOR(51,6E,82,01,10).
        assert_eq!([0x51, 0x82, 0x01, 0x10, ddc_ck(&[0x82, 0x01, 0x10])][4], 0xAC);
        // SetVCP 0x10 = 33 request [51 84 03 10 00 21 89], ck = XOR(51,6E,84,03,10,00,21).
        assert_eq!([0x51, 0x84, 0x03, 0x10, 0x00, 0x21, ddc_ck(&[0x84, 0x03, 0x10, 0x00, 0x21])][6], 0x89);
    }

    #[test]
    fn get_reply_offsets() {
        // Real reply (cur=70, max=100): 6e 88 02 00 10 00 [00 64] [00 46] 86 5c
        let raw = [0x6E, 0x88, 0x02, 0x00, 0x10, 0x00, 0x00, 0x64, 0x00, 0x46, 0x86, 0x5C];
        if let Ok(mut m) = MAX_VCP.lock() {
            *m = 100;
        }
        assert_eq!(parse_get_reply(&raw), Some(70));
    }

    #[test]
    fn get_reply_rejects_bad_result_and_ck() {
        let mut raw = [0x6E, 0x88, 0x02, 0x00, 0x10, 0x00, 0x00, 0x64, 0x00, 0x46, 0x86, 0x5C];
        raw[10] ^= 0xFF;
        assert_eq!(parse_get_reply(&raw), None); // bad ck
        raw[10] ^= 0xFF;
        raw[3] = 0x01;
        assert_eq!(parse_get_reply(&raw), None); // rejected VCP
    }

    #[test]
    fn get_reply_rejects_bad_header() {
        assert_eq!(parse_get_reply(&[0x00; 12]), None);
        assert_eq!(parse_get_reply(&[0x6E]), None);
    }
}
