// FluxDeck brightness tray — Rust/X11 port of the Python/Qt original.
// See ui.rs for the freeze analysis (override-redirect, no grabs, worker-only I/O).
use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

mod bitmapfont;
mod display;
mod discovery;
mod sni;
mod ui;

use display::Backend;
use discovery::{ddc_label, discover, DisplayKind};

/// GUI-visible state: names/keys/levels + mirrored average + config flag.
/// Lock hold times are microseconds (field copies only) — never I/O.
pub struct DisplayState {
    pub backends: Vec<Arc<Mutex<dyn Backend>>>,
    pub names: Vec<String>,
    pub keys: Vec<String>,
    pub levels: Vec<u32>,
    pub avg: u32,
    pub apply_at_login: bool,
}

pub type SharedState = Arc<Mutex<DisplayState>>;

pub enum Command {
    /// Set one display to a target percent.
    Msg { key: String, target_pct: u32 },
    /// Persist config: apply-at-login flag + current levels snapshot.
    SaveConfig { apply: bool, levels: HashMap<String, u32> },
}

fn main() {
    if let Err(e) = run() {
        eprintln!("fluxdeck: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let displays = discover();
    if displays.is_empty() {
        return Err("no controllable displays found (no backlight, no DDC)".into());
    }

    let mut backends: Vec<Arc<Mutex<dyn Backend>>> = Vec::new();
    let mut names = Vec::new();
    let mut keys = Vec::new();
    for d in &displays {
        match d.kind {
            DisplayKind::Backlight => {
                backends.push(Arc::new(Mutex::new(display::BacklightHandle::new(
                    d.name.clone(),
                    // key = "bl:{sysfs base}"; recover the base path.
                    d.key.strip_prefix("bl:").unwrap_or(&d.key).to_string(),
                    d.max,
                ))));
            }
            DisplayKind::Ddc => {
                let bus: u16 = d
                    .key
                    .split(':')
                    .nth(1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(u16::MAX);
                if bus == u16::MAX {
                    eprintln!("fluxdeck: skipping malformed DDC key {:?}", d.key);
                    continue;
                }
                // Open may fail (EACCES); still list it with a hint label.
                let name = ddc_label(&d.name, bus);
                backends.push(Arc::new(Mutex::new(display::DdcHandle::new(bus, name.clone()))));
            }
        }
        names.push(d.name.clone());
        keys.push(d.key.clone());
    }

    let cfg = config::load();
    let initial: Vec<u32> = backends
        .iter()
        .map(|b| b.lock().unwrap().get().max(0) as u32)
        .collect();
    let avg = initial.iter().sum::<u32>() / initial.len().max(1) as u32;
    let state: SharedState = Arc::new(Mutex::new(DisplayState {
        backends,
        names,
        keys,
        levels: initial,
        avg,
        apply_at_login: cfg.apply_at_login,
    }));

    let (tx, rx) = mpsc::channel::<Command>();

    // Apply-at-login restore: queue saved levels; worker applies shortly after
    // DDC lines wake up (pacing in DdcHandle handles the rest).
    if cfg.apply_at_login {
        for (key, val) in cfg.levels.iter() {
            tx.send(Command::Msg { key: key.clone(), target_pct: *val }).ok();
        }
    }

    // Worker thread: ALL backend I/O lives here (get, set, config save).
    {
        let state = state.clone();
        thread::spawn(move || worker(rx, state));
    }

    // Primary tray: SNI/DBus (XFCE 4.20 dropped XEmbed from the strip).
    // XEmbed stays as fallback for DEs without a StatusNotifierWatcher.
    let sni = match sni::SniIcon::new("fluxdeck", "FluxDeck Brightness") {
        Ok(s) => {
            s.set_icon("fluxdeck");
            Some(std::sync::Arc::new(s))
        }
        Err(e) => {
            eprintln!("fluxdeck: SNI unavailable ({e}); using XEmbed tray fallback");
            None
        }
    };

    // Blocking X11 event loop; returns when the tray icon is right-clicked.
    let loop_result = ui::run(state, tx, sni);
    if let Err(e) = &loop_result {
        eprintln!("fluxdeck: UI error: {e}");
    }
    loop_result
}

fn worker(rx: mpsc::Receiver<Command>, state: SharedState) {
    // Hotplug: re-run discovery every 30s in the same loop as command handling —
    // still worker-thread-only I/O, GUI never blocks.
    const RESCAN: Duration = Duration::from_secs(30);
    loop {
        match rx.recv_timeout(RESCAN) {
            Ok(cmd) => handle(cmd, &state),
            Err(mpsc::RecvTimeoutError::Timeout) => rescan(&state),
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn handle(cmd: Command, state: &SharedState) {
    match cmd {
        Command::Msg { key, target_pct } => {
            let backend = {
                let st = state.lock().unwrap();
                st.keys
                    .iter()
                    .position(|k| *k == key)
                    .and_then(|i| st.backends.get(i).cloned())
            };
            let Some(b) = backend else { return };
            let pct = target_pct.min(100);
            b.lock().unwrap().set(pct);
            // Read back for UI truth; DDC may lag one frame. get() returns
            // -1 on failure — keep last known value in that case.
            thread::sleep(Duration::from_millis(120));
            let got = b.lock().unwrap().get();
            if got >= 0 {
                let mut st = state.lock().unwrap();
                if let Some(i) = st.keys.iter().position(|k| *k == key) {
                    st.levels[i] = got as u32;
                }
                st.avg = st.levels.iter().sum::<u32>() / st.levels.len().max(1) as u32;
            }
        }
        Command::SaveConfig { apply, levels } => {
            config::save(apply, levels);
        }
    }
}

/// Re-run discovery; add newly connected DDC displays, drop disconnected ones.
/// Backlight entries are stable in practice — rebuild everything anyway (cheap,
/// sysfs reads only) so keys stay a pure function of the current connector set.
fn rescan(state: &SharedState) {
    let fresh = discover();
    let mut st = state.lock().unwrap();
    let same = st.keys.len() == fresh.len()
        && st.keys.iter().zip(&fresh).all(|(k, d)| k == &d.key);
    if same {
        return;
    }
    let mut backends = Vec::new();
    let mut names = Vec::new();
    let mut keys = Vec::new();
    for d in &fresh {
        match d.kind {
            DisplayKind::Backlight => backends.push(Arc::new(Mutex::new(
                display::BacklightHandle::new(
                    d.name.clone(),
                    d.key.strip_prefix("bl:").unwrap_or(&d.key).to_string(),
                    d.max,
                ),
            )) as Arc<Mutex<dyn Backend>>),
            DisplayKind::Ddc => {
                let bus: u16 = d.key.split(':').nth(1).and_then(|s| s.parse().ok()).unwrap_or(u16::MAX);
                if bus == u16::MAX {
                    continue;
                }
                backends.push(Arc::new(Mutex::new(display::DdcHandle::new(bus, d.name.clone())))
                    as Arc<Mutex<dyn Backend>>);
            }
        }
        names.push(d.name.clone());
        keys.push(d.key.clone());
    }
    if keys.is_empty() {
        return; // transient empty scan; keep current set
    }
    let levels: Vec<u32> = keys
        .iter()
        .map(|k| {
            st.keys
                .iter()
                .position(|old| old == k)
                .and_then(|i| st.levels.get(i).copied())
                .unwrap_or(st.avg)
        })
        .collect();
    let avg = levels.iter().sum::<u32>() / levels.len().max(1) as u32;
    st.backends = backends;
    st.names = names;
    st.keys = keys;
    st.levels = levels;
    st.avg = avg;
}

mod config {
    use std::collections::HashMap;

    #[derive(Default)]
    pub struct Config {
        pub apply_at_login: bool,
        pub levels: HashMap<String, u32>,
    }

    pub fn load() -> Config {
        let Ok(s) = std::fs::read_to_string(path()) else {
            return Config::default();
        };
        // ponytail: hand-rolled flat JSON ({"apply_at_login":bool,"level":"key":pct,...})
        // — serde_json for two fields pulls in 30 crates; replace if format grows.
        parse(&s).unwrap_or_default()
    }

    pub fn save(apply: bool, levels: HashMap<String, u32>) {
        let mut s = String::from("{\n  \"apply_at_login\": ");
        s.push_str(if apply { "true" } else { "false" });
        s.push_str(",\n  \"levels\": {");
        let mut first = true;
        for (k, v) in &levels {
            if !first {
                s.push(',');
            }
            first = false;
            s.push_str(&format!("\n    \"{}\": {}", k, v));
        }
        s.push_str("\n  }\n}\n");
        let p = path();
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, s).is_ok() {
            let _ = std::fs::rename(&tmp, p);
        }
    }

    fn path() -> std::path::PathBuf {
        let home = std::env::var("HOME").unwrap_or_default();
        std::path::Path::new(&home).join(".config/fluxdeck/brightness.json")
    }

    fn parse(s: &str) -> Option<Config> {
        let json = s.trim().trim_start_matches('{').trim_end_matches('}');
        let mut cfg = Config::default();
        for part in split_top(&json.to_string()) {
            let (k, v) = part.split_once(':')?;
            let k = k.trim().trim_matches('"');
            let v = v.trim().trim_end_matches(',').trim();
            match k {
                "apply_at_login" => cfg.apply_at_login = v == "true",
                "levels" => {
                    for kv in split_top(&v.trim().trim_start_matches('{').trim_end_matches('}').to_string()) {
                        let Some((lk, lv)) = kv.rsplit_once(':') else { continue; };
                        if let Ok(n) = lv.trim().trim_end_matches(',').parse() {
                            cfg.levels.insert(lk.trim().trim_matches('"').to_string(), n);
                        }
                    }
                }
                _ => {}
            }
        }
        Some(cfg)
    }

    /// Split "a":1,"b":2 on top-level commas (inside quotes don't count).
    fn split_top(s: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut in_str = false;
        let mut depth = 0;
        for ch in s.chars() {
            match ch {
                '"' => {
                    in_str = !in_str;
                    cur.push(ch);
                }
                '{' | '[' => {
                    depth += 1;
                    cur.push(ch);
                }
                '}' | ']' => {
                    depth -= 1;
                    cur.push(ch);
                }
                ',' if !in_str && depth == 0 => {
                    out.push(std::mem::take(&mut cur));
                }
                _ => cur.push(ch),
            }
        }
        if !cur.trim().is_empty() {
            out.push(cur);
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn roundtrip() {
            let mut levels = HashMap::new();
            levels.insert("bl:/sys/class/backlight/amdgpu_bl0".into(), 42u32);
            levels.insert("ddc:5:HDMI-A-1".into(), 77u32);
            save(true, levels.clone());
            let cfg = load();
            assert!(cfg.apply_at_login);
            assert_eq!(cfg.levels, levels);
        }
    }
}
