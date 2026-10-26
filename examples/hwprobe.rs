use fluxdeck::discovery::{self, DisplayKind};
use fluxdeck::display::*;
use std::time::Duration;
fn main() {
    for d in discovery::discover() {
        println!("found: kind={:?} name={} key={}", d.kind, d.name, d.key);
        match d.kind {
            DisplayKind::Backlight => {
                let base = d.key.trim_start_matches("bl:").to_string();
                let h = BacklightHandle::new(d.name.clone(), base, d.max);
                let orig = h.get();
                h.set(35); std::thread::sleep(Duration::from_millis(250));
                let mid = h.get();
                if orig >= 0 {
                    h.set(orig as u32);
                }
                std::thread::sleep(Duration::from_millis(250));
                println!("  roundtrip: orig={} set35={} restored={}", orig, mid, h.get());
            }
            DisplayKind::Ddc => {
                let bus = d.key.split(':').nth(1).unwrap().parse().unwrap();
                let h = DdcHandle::new(bus, d.name.clone());
                let orig = h.get();
                if orig < 0 {
                    println!("  ddc get failed (perm? see install.sh)");
                    continue;
                }
                h.set(35); std::thread::sleep(Duration::from_millis(250));
                let mid = h.get();
                h.set(orig as u32); std::thread::sleep(Duration::from_millis(250));
                println!("  roundtrip: orig={} set35={} restored={}", orig, mid, h.get());
            }
        }
    }
}
