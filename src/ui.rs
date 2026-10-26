// FluxDeck tray + popup panel: x11rb only, no GUI toolkit.
//
// Theory of the old freeze (Python/Qt version): Qt.Popup grabs the pointer for
// the popup's lifetime; the ramp thread held the DDC I/O lock 100ms+ per op;
// QThread.terminate() mid-I2C could leave _io_lock held, so the GUI thread
// later blocked inside get(). This design: override-redirect popup (no pointer
// grab at all), no locks shared with the GUI thread (the state mutex is only
// ever held for microseconds around field copies), and ALL display I/O in the
// worker thread. The popup can never freeze the desktop.
use std::os::fd::AsRawFd;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::wrapper::ConnectionExt as WrapperConnExt;
use x11rb::{COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::*;
use x11rb::protocol::randr::ConnectionExt as RandrConnExt;
use x11rb::rust_connection::RustConnection;
use crate::{Command, SharedState};

const ICON: u16 = 24;
const ROW_H: i16 = 44;
const ROWS_Y: i16 = 48;
const TRACK_W: u16 = 210;
// Layout: label zone auto-sizes to the longest display name (dynamic width).
const PAD: i16 = 16; // outer padding + card side padding
const LABEL_ZONE_MIN: i16 = 118; // room for "All monitors" + long EDID names
// Modern dark palette: deep glass panel, lifted cards, single accent.
const PANEL_BG: u32 = 0x161820;
const PANEL_BORDER: u32 = 0x2c3040;
const CARD_BG: u32 = 0x1f2230;
const PANEL_FG: u32 = 0xffffff;
const ACCENT: u32 = 0x4a6fd4;
const ACCENT_HI: u32 = 0x7da2ff;
const SUN: u32 = 0xffc94d; // matches the tray logo mark
const SLIDER_BG: u32 = 0x333848;
const SLIDER_FILL: u32 = ACCENT;
const SLIDER_KNOB: u32 = 0xffffff;
// poll(2) via direct libc decl: lets the event loop wait on the X socket with
// a timeout without blocking syscalls that would stall our 250ms UI tick.
#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}
extern "C" {
    fn poll(fds: *mut PollFd, nfds: u64, timeout: i32) -> i32;
}
const POLLIN: i16 = 0x001;

pub struct Ui {
    conn: RustConnection,
    screen: Screen,
    screen_idx: usize,
    state: SharedState,
    cmd_tx: Sender<Command>,
    icon_win: Window,
    icon_pm: Pixmap,
    panel_win: Window,
    panel_pm: Pixmap,
    panel_w: u16,  // dynamic: grows to fit the longest display name
    track_x: i16,  // slider x inside the panel (follows panel_w)
    gc: Gcontext,
    panel_visible: bool,
    escape_kc: Option<Keycode>,
    drag: Option<usize>, // slider row being dragged (usize::MAX = master row)
    use_xembed_fallback: bool,
    docked: bool,
    sni: Option<std::sync::Arc<crate::sni::SniIcon>>,
    quit: bool,
}

impl Ui {
    pub fn new(
        state: SharedState,
        cmd_tx: Sender<Command>,
        sni: Option<std::sync::Arc<crate::sni::SniIcon>>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let (conn, screen_idx) = RustConnection::connect(None)?;
        let screen = conn.setup().roots[screen_idx].clone();

        let icon_win = conn.generate_id()?;
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            icon_win,
            screen.root,
            -1,
            -1,
            ICON,
            ICON,
            0,
            WindowClass::INPUT_OUTPUT,
            COPY_FROM_PARENT,
            &CreateWindowAux::new().event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS),
        )?;
        // WM_CLASS so xfce4-panel/systray can identify+name the icon
        let wm_class_atom = conn.intern_atom(false, b"WM_CLASS")?.reply()?.atom;
        conn.change_property8(
            PropMode::REPLACE,
            icon_win,
            wm_class_atom,
            AtomEnum::STRING,
            b"fluxdeck\0FluxDeck\0",
        )?;
        // _XEMBED_INFO: version 0, XEMBED_MAPPED=1
        let xembed = conn.intern_atom(false, b"_XEMBED_INFO")?.reply()?.atom;
        let mut data = [0u8; 8];
        data[..4].copy_from_slice(&0u32.to_ne_bytes()); // version 0
        data[4..].copy_from_slice(&1u32.to_ne_bytes()); // XEMBED_MAPPED=1
        conn.change_property(
            PropMode::REPLACE,
            icon_win,
            xembed,
            6u32, // Atom::CARDINAL
            32,
            2,
            &data,
        )?;

        let n_rows = state.lock().unwrap().keys.len();
        let panel_win = conn.generate_id()?;
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            panel_win,
            screen.root,
            -1,
            -1,
            380, // initial; resized in layout() at first redraw
            panel_h(n_rows),
            0,
            WindowClass::INPUT_OUTPUT,
            COPY_FROM_PARENT,
            // override-redirect: no WM frame, no pointer grab, no focus steal —
            // the anti-freeze property Qt.Popup lacked. We position ourselves.
            &CreateWindowAux::new()
                .override_redirect(1)
                .background_pixel(PANEL_BG)
                .event_mask(
                    EventMask::EXPOSURE
                        | EventMask::BUTTON_PRESS
                        | EventMask::BUTTON_RELEASE
                        | EventMask::POINTER_MOTION
                        | EventMask::KEY_PRESS,
                ),
        )?;

        let gc = conn.generate_id()?;
        conn.create_gc(gc, screen.root, &CreateGCAux::new())?;
        // Bitmap font (font8x8, public domain) rendered in software — no X
        // core fonts (server glyph tables proved unreliable here).
        let icon_pm = conn.generate_id()?;
        conn.create_pixmap(screen.root_depth, icon_pm, icon_win, ICON, ICON)?;
        let panel_pm = conn.generate_id()?;
        let panel_w: u16 = 380; // provisional; layout() resizes at first redraw
        conn.create_pixmap(screen.root_depth, panel_pm, panel_win, panel_w, panel_h(n_rows))?;

        Ok(Self {
            escape_kc: escape_keycode(&conn),
            conn,
            screen,
            screen_idx,
            state,
            cmd_tx,
            icon_win,
            icon_pm,
            panel_win,
            panel_pm,
            panel_w,
            track_x: 0, // set by layout() on first redraw
            gc,
            panel_visible: false,
            drag: None,
            docked: false,
            // XEmbed dock only used as fallback when SNI is unavailable.
            use_xembed_fallback: sni.is_none(),
            sni,
            quit: false,
        })
    }
    /// Non-blocking; safe no-op when SNI is absent/dead. The 24px logo pixmap
    /// carries the level color (see redraw_icon); nothing else to push here.
    fn sni_tick(&self) {}


    pub fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let mut last_ui_tick = Instant::now() - Duration::from_millis(250);
        let mut undock_since: Option<Instant> = None;
        while !self.quit {
            // Wait up to 100ms on the X socket (poll, not blocking read).
            let mut pfd = PollFd {
                fd: self.conn.stream().as_raw_fd(),
                events: POLLIN,
                revents: 0,
            };
            let r = unsafe { poll(&mut pfd, 1, 100) };
            if r < 0 && std::io::Error::last_os_error().raw_os_error() != Some(4) {
                // EINTR == 4 on Linux; retrying the loop is fine.
                return Err("poll(X socket) failed".into());
            }
            while let Ok(Some(ev)) = self.conn.poll_for_event() {
                self.handle_event(ev);
            }
            self.conn.flush()?;

            // Undocked: retry the XEmbed dock handshake every 10s. Runs inside
            // the event loop (no extra thread touching the connection).
            // ponytail: keyboard-only fallback between retries is "no icon";
            // upgrade path: grab a global brightness keysym via XGrabKey.
            if !self.use_xembed_fallback {
                // SNI active: panel talks to us over DBus; poll its signals.
                if let Some(sni) = &self.sni {
                    if sni.poll_secondary() {
                        self.quit = true;
                    }
                    if sni.poll_activate() {
                        self.toggle_panel();
                    }
                }
            } else if !self.docked {
                match undock_since {
                    None => {
                        self.dock_tray();
                        undock_since = Some(Instant::now());
                    }
                    Some(t) if t.elapsed() >= Duration::from_secs(10) => {
                        self.dock_tray();
                        undock_since = Some(Instant::now());
                    }
                    Some(_) => {}
                }
            } else if undock_since.map_or(true, |t| t.elapsed() >= Duration::from_secs(10)) && self.tray_gone() {
                self.docked = false; // manager died; icon will re-dock later
                undock_since = Some(Instant::now());
            }
            if last_ui_tick.elapsed() >= Duration::from_millis(250) {
                last_ui_tick = Instant::now();
                self.ui_tick();
                self.sni_tick();
            }
        }

        Ok(())
    }

    fn tray_gone(&self) -> bool {
        let sel = self
            .conn
            .intern_atom(false, format!("_NET_SYSTEM_TRAY_S{}", self.screen_idx).as_bytes())
            .ok()
            .and_then(|r| r.reply().ok())
            .map(|r| r.atom);
        match sel {
            Some(sel) => match self.conn.get_selection_owner(sel).ok().and_then(|r| r.reply().ok()) {
                Some(rep) => rep.owner == x11rb::NONE,
                None => true,
            },
            None => true,
        }
    }

    fn dock_tray(&mut self) {
        let Some(owner) = self.tray_owner() else { return };
        let Some(opcode) = self
            .conn
            .intern_atom(false, b"_NET_SYSTEM_TRAY_OPCODE")
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.atom)
        else {
            return;
        };
        // SYSTEM_TRAY_REQUEST_DOCK = 0; data = [timestamp, sel, 0, icon, 0]
        let sel = self
            .conn
            .intern_atom(false, format!("_NET_SYSTEM_TRAY_S{}", self.screen_idx).as_bytes())
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.atom)
            .unwrap_or(x11rb::NONE);
        let msg = ClientMessageEvent::new(
            32,
            owner,
            opcode,
            ClientMessageData::from([0u32, sel, 0, u32::from(self.icon_win), 0]),
        );
        if self.conn.send_event(false, owner, EventMask::NO_EVENT, msg).is_ok()
            && self.conn.map_window(self.icon_win).is_ok()
            && self.conn.flush().is_ok()
        {
            self.docked = true;
        }
    }

    fn tray_owner(&self) -> Option<Window> {
        let sel = self
            .conn
            .intern_atom(false, format!("_NET_SYSTEM_TRAY_S{}", self.screen_idx).as_bytes())
            .ok()?
            .reply()
            .ok()?
            .atom;
        let owner = self.conn.get_selection_owner(sel).ok()?.reply().ok()?.owner;
        (owner != x11rb::NONE).then_some(owner)
    }

    fn ui_tick(&mut self) {
        let (levels, avg) = {
            let st = self.state.lock().unwrap();
            (st.levels.clone(), st.avg)
        };
        self.redraw_icon(avg);
        // While the user drags a slider we own that row's value; don't stomp it.
        if self.panel_visible && self.drag.is_none() {
            let mut st = self.state.lock().unwrap();
            let mut changed = false;
            for (i, v) in levels.iter().enumerate() {
                if st.levels[i] != *v {
                    st.levels[i] = *v;
                    changed = true;
                }
            }
            drop(st);
            if changed {
                self.redraw_panel();
            }
        }
    }

    /// Miniature logo.svg: sun disc + slider track + knob. Disk color
    /// follows brightness so the tray still reads at a glance.
    fn redraw_icon(&self, avg: u32) {
        let level = if avg >= 40 {
            0xFFC94D
        } else if avg >= 15 {
            0xB0BEC5
        } else {
            0x78909C
        };
        // Same mark rendered in software for the SNI IconPixmap (hosts paint
        // this instead of resolving an icon theme name).
        if let Some(sni) = &self.sni {
            let px = logo_argb24(level);
            sni.set_icon_pixmap(px);
        }
        let c = &self.conn;
        let fg = |v: u32| ChangeGCAux::new().foreground(v);
        let _ = c.change_gc(self.gc, &fg(PANEL_BG));
        let _ = c.poly_fill_rectangle(self.icon_pm, self.gc, &[Rectangle { x: 0, y: 0, width: ICON, height: ICON }]);
        // sun disc r=7 centered (12,10)
        let _ = c.change_gc(self.gc, &fg(level));
        let _ = c.poly_fill_arc(self.icon_pm, self.gc, &[Arc { x: 5, y: 3, width: 14, height: 14, angle1: 0, angle2: 360 * 64 }]);
        // rays: top, upper-left, upper-right
        let _ = c.poly_segment(self.icon_pm, self.gc, &[
            Segment { x1: 12, y1: 1, x2: 12, y2: 3 },
            Segment { x1: 4, y1: 4, x2: 6, y2: 6 },
            Segment { x1: 20, y1: 4, x2: 18, y2: 6 },
        ]);
        // slider track + accent fill + white knob
        let _ = c.change_gc(self.gc, &fg(SLIDER_BG));
        let _ = c.poly_fill_rectangle(self.icon_pm, self.gc, &[Rectangle { x: 3, y: 19, width: 18, height: 3 }]);
        let _ = c.change_gc(self.gc, &fg(ACCENT));
        let _ = c.poly_fill_rectangle(self.icon_pm, self.gc, &[Rectangle { x: 3, y: 19, width: 10, height: 3 }]);
        let _ = c.change_gc(self.gc, &fg(0xFFFFFF));
        let _ = c.poly_fill_arc(self.icon_pm, self.gc, &[Arc { x: 11, y: 17, width: 6, height: 6, angle1: 0, angle2: 360 * 64 }]);
        let _ = c.copy_area(self.icon_pm, self.icon_win, self.gc, 0, 0, 0, 0, ICON, ICON);
        let _ = c.flush();
    }

    /// Render the panel offscreen, then blit. Pure X11 draws off SharedState
    /// copies — never calls backend get/set.
    #[allow(clippy::too_many_lines)]
    /// Panel width for the current display names: label zone auto-sizes to
    /// the longest name, so long EDID names never clip.
    fn layout(&mut self, names: &[String]) {
        let mut zone = LABEL_ZONE_MIN;
        for n in names {
            let w = crate::bitmapfont::str_width(n.as_bytes());
            zone = zone.max(i16::try_from(w).unwrap_or(LABEL_ZONE_MIN) + PAD);
        }
        let w = (PAD + zone + TRACK_W as i16 + 52 + PAD) as u16;
        self.track_x = PAD + zone;
        if w != self.panel_w {
            // Pixmap resize: free + recreate at the new width.
            let h = panel_h(self.state.lock().unwrap().keys.len());
            let _ = self.conn.free_pixmap(self.panel_pm);
            let pm = self.conn.generate_id().expect("pixmap id");
            self.conn
                .create_pixmap(self.screen.root_depth, pm, self.panel_win, w, h)
                .expect("panel pixmap");
            self.panel_pm = pm;
            self.panel_w = w;
        }
    }

    fn redraw_panel(&mut self) {
        let (names, levels) = {
            let st = self.state.lock().unwrap();
            (st.names.clone(), st.levels.clone())
        };
        self.layout(&names);
        let c = &self.conn;
        let w = self.panel_w;
        let h = panel_h(names.len());
        let _ = c.change_gc(self.gc, &ChangeGCAux::new().foreground(PANEL_BG));
        let _ = c.poly_fill_rectangle(self.panel_pm, self.gc, &[Rectangle { x: 0, y: 0, width: w, height: h }]);
        // Hairline border.
        let _ = c.change_gc(self.gc, &ChangeGCAux::new().foreground(PANEL_BORDER).line_width(1));
        let _ = c.poly_rectangle(self.panel_pm, self.gc, &[Rectangle {
            x: 0, y: 0, width: w - 1, height: h - 1,
        }]);
        // Header row: sun glyph + label + right-aligned current %.
        sun_glyph(c, self.gc, self.panel_pm, 24, 28);
        let _ = c.change_gc(self.gc, &ChangeGCAux::new().foreground(PANEL_FG));
        draw_text(c, self.panel_pm, self.gc, 40, 20, b"Brightness", 1);
        let avg = if levels.is_empty() { 0 } else { levels.iter().sum::<u32>() / levels.len() as u32 };
        let header_pct = format!("{}%", avg);
        let pw = crate::bitmapfont::str_width(header_pct.as_bytes());
        draw_text(c, self.panel_pm, self.gc, w as i16 - PAD - pw, 22, header_pct.as_bytes(), 1);
        for (i, name) in names.iter().enumerate() {
            let y = ROWS_Y + i as i16 * ROW_H;
            let v = levels.get(i).copied().unwrap_or(0);
            self.slider_row(y, name, v);
        }
        if names.len() > 1 {
            let y = ROWS_Y + names.len() as i16 * ROW_H;
            let avg = levels.iter().sum::<u32>() / levels.len().max(1) as u32;
            self.slider_row(y, "All monitors", avg);
        }
        self.blit_panel();
    }

    /// One slider row: card bg, bitmap label, track, % right of track.
    fn slider_row(&self, y: i16, name: &str, v: u32) {
        let c = &self.conn;
        let w = self.panel_w;
        // Card behind the row.
        rrect(c, self.gc, self.panel_pm, PAD - 8, y + 4, w - 2 * u16::try_from(PAD - 8).unwrap_or(8), 36, CARD_BG);
        let _ = c.change_gc(self.gc, &ChangeGCAux::new().foreground(PANEL_FG));
        draw_text(c, self.panel_pm, self.gc, PAD, y + 15, name.as_bytes(), 1);
        slider(c, self.gc, self.panel_pm, self.track_x, y, TRACK_W, v);
        let pct = format!("{}%", v);
        let pw = crate::bitmapfont::str_width(pct.as_bytes());
        draw_text(c, self.panel_pm, self.gc, w as i16 - PAD - pw, y + 15, pct.as_bytes(), 1);
    }

    fn blit_panel(&self) {
        let h = panel_h(self.state.lock().unwrap().keys.len());
        let _ = self
            .conn
            .copy_area(self.panel_pm, self.panel_win, self.gc, 0, 0, 0, 0, self.panel_w, h);
        let _ = self.conn.flush();
    }

    fn show_panel(&mut self) {
        let n = self.state.lock().unwrap().keys.len();
        // Anchor to the monitor the pointer is on (the tray icon lives in that
        // monitor's taskbar), clamped inside it. screen.width_in_pixels spans
        // the WHOLE root (all monitors), so the old right-edge math threw the
        // panel onto the other screen in multi-monitor setups.
        let (mut px, mut py) = (0i32, 32i32);
        if let Ok(r) = self.conn.query_pointer(self.screen.root) {
            if let Ok(reply) = r.reply() {
                let (mx, mw) = self.monitor_span(reply.root_x);
                px = mx + mw as i32 - i32::from(self.panel_w) - 24;
                py = 32;
            }
        }
        let _ = self.conn.configure_window(
            self.panel_win,
            &ConfigureWindowAux::new()
                .x(px)
                .y(py)
                .width(u32::from(self.panel_w))
                .height(u32::from(panel_h(n))),
        );
        self.redraw_panel();
        let _ = self.conn.map_window(self.panel_win);
        // take input focus so Escape reaches us (focus w/o grab — no freeze)
        let _ = self.conn.set_input_focus(InputFocus::PARENT, self.panel_win, 0u32);
        self.panel_visible = true;
        let _ = self.conn.flush();
    }


    /// (x_origin, width) of the monitor containing root coordinate `x`.
    /// Falls back to the full root when RANDR is unavailable.
    fn monitor_span(&self, x: i16) -> (i32, u32) {
        let full = (0, u32::from(self.screen.width_in_pixels));
        let Some(res) = self
            .conn
            .randr_get_screen_resources(self.screen.root)
            .ok()
            .and_then(|r| r.reply().ok())
        else {
            return full;
        };
        for crtc in &res.crtcs {
            let Some(info) = self
                .conn
                .randr_get_crtc_info(*crtc, res.config_timestamp)
                .ok()
                .and_then(|r| r.reply().ok())
            else {
                continue;
            };
            if info.num_outputs() == 0 {
                continue; // disabled CRTC
            }
            let x0 = i32::from(info.x);
            let x1 = x0 + i32::from(info.width);
            if i32::from(x) >= x0 && i32::from(x) < x1 {
                return (x0, u32::from(info.width));
            }
        }
        full
    }
    fn hide_panel(&mut self) {
        let _ = self.conn.unmap_window(self.panel_win);
        // drop focus so the user's apps resume receiving keys
        let _ = self
            .conn
            .set_input_focus(InputFocus::POINTER_ROOT, 0u32, 0u32);
        self.panel_visible = false;
        self.drag = None;
        let _ = self.conn.flush();
    }

    fn toggle_panel(&mut self) {
        if self.panel_visible {
            self.hide_panel();
        } else {
            self.show_panel();
        }
    }

    fn pct_from_x(&self, x: i16) -> u32 {
        (((x - self.track_x).max(0) as u32) * 100 / u32::from(TRACK_W)).min(100)
    }

    /// Slider row under the pointer: 0..n displays, n = master row (N>1 only).
    fn slider_row_at(&self, x: i16, y: i16) -> Option<usize> {
        let n = self.state.lock().unwrap().keys.len();
        let rows = if n > 1 { n + 1 } else { n }; // last extra row = "All monitors"
        (0..rows).find(|&i| {
            let top = ROWS_Y + i as i16 * ROW_H;
            y >= top && y < top + ROW_H && x >= self.track_x - 8 && x < self.track_x + TRACK_W as i16 + 8
        })
    }
    /// Handle one X event; sets self.quit on tray right-click.
    fn handle_event(&mut self, ev: Event) {
        match ev {
            Event::Expose(e) => {
                if e.window == self.panel_win {
                    self.blit_panel();
                } else if e.window == self.icon_win {
                    self.redraw_icon(self.state.lock().unwrap().avg);
                }
            }
            Event::ButtonPress(ev) => {
                if ev.event == self.icon_win {
                    match ev.detail {
                        1 => self.toggle_panel(),
                        // ponytail: no right-click menu; button3 quits directly.
                        // Upgrade path: draw a tiny menu window on button3.
                        _ => self.quit = true,
                    }
                } else if self.panel_visible {
                    if let Some(row) = self.slider_row_at(ev.event_x, ev.event_y) {
                        let pct = self.pct_from_x(ev.event_x);
                        // Last row with 2+ displays is the "All monitors" master;
                        // map it to the MASTER sentinel before any level math.
                        let row = if row == self.state.lock().unwrap().levels.len() { MASTER } else { row };
                        self.drag = Some(row);
                        self.set_drag_levels(row, pct);
                    }
                }
            }
            Event::MotionNotify(ev) => {
                if let Some(row) = self.drag {
                    let pct = self.pct_from_x(ev.event_x);
                    self.set_drag_levels(row, pct);
                    self.redraw_panel();
                }
            }
            Event::ButtonRelease(ev) => {
                if let Some(row) = self.drag.take() {
                    let pct = self.pct_from_x(ev.event_x);
                    let keys = self.state.lock().unwrap().keys.clone();
                    if row == MASTER {
                        for key in keys {
                            self.cmd_tx.send(Command::Msg { key, target_pct: pct }).ok();
                        }
                    } else if let Some(key) = keys.get(row) {
                        self.cmd_tx
                            .send(Command::Msg { key: key.clone(), target_pct: pct })
                            .ok();
                    }
                    self.redraw_panel();
                }
            }
            Event::KeyPress(ev) => {
                if Some(ev.detail) == self.escape_kc {
                    self.hide_panel();
                }
            }
            _ => {}
        }
    }

    fn set_drag_levels(&self, row: usize, pct: u32) {
        let mut st = self.state.lock().unwrap();
        if row == MASTER {
            for v in st.levels.iter_mut() {
                *v = pct;
            }
            st.avg = pct;
        } else if let Some(v) = st.levels.get_mut(row) {
            *v = pct;
            let n = st.levels.len();
            if n > 0 {
                st.avg = st.levels.iter().sum::<u32>() / n as u32;
            }
        }
    }
}

/// Blocking X11 event loop; returns on tray right-click or X error.
pub fn run(
    state: crate::SharedState,
    cmd_tx: Sender<crate::Command>,
    sni: Option<std::sync::Arc<crate::sni::SniIcon>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut ui = Ui::new(state, cmd_tx, sni)?;
    // First dock attempt happens on the first loop iteration (undock_since=None).
    ui.run()
}

const MASTER: usize = usize::MAX;
fn panel_h(n_displays: usize) -> u16 {
    (ROWS_Y + n_displays as i16 * ROW_H + 44) as u16
}

/// Software bitmap text: top-left (x, y), `scale` (1 or 2), current GC fg.
fn draw_text(c: &RustConnection, pixmap: Pixmap, gc: Gcontext, x: i16, y: i16, s: &[u8], scale: u16) {
    for (i, &b) in s.iter().enumerate() {
        let rows = crate::bitmapfont::glyph(b);
        for (ry, &row) in rows.iter().enumerate() {
            if row == 0 {
                continue;
            }
            for bit in 0..8 {
                if row & (1 << bit) != 0 {
                    let px = x + i as i16 * crate::bitmapfont::CHAR_W as i16 * scale as i16 + bit as i16 * scale as i16;
                    let py = y + ry as i16 * scale as i16;
                    let _ = c.poly_fill_rectangle(pixmap, gc, &[Rectangle {
                        x: px, y: py, width: scale, height: scale,
                    }]);
                }
            }
        }
    }
}

fn slider(c: &RustConnection, gc: Gcontext, pixmap: Pixmap, x: i16, y: i16, w: u16, pct: u32) {
    // Slim rounded track h=10; two-tone fill (ACCENT_HI half → ACCENT) for depth.
    const TH: i16 = 10;
    let fill = (u32::from(w) * pct.min(100) / 100) as i16;
    rrect(c, gc, pixmap, x, y + 19, w, TH, SLIDER_BG);
    if fill > 0 {
        rrect(c, gc, pixmap, x, y + 19, fill as u16, TH, ACCENT);
        let half = ((fill + 1) / 2).min(fill);
        rrect(c, gc, pixmap, x, y + 19, half as u16, TH, ACCENT_HI);
    }
    knob(c, gc, pixmap, x + fill, y + 24);
}

/// White knob with soft accent ring.
fn knob(c: &RustConnection, gc: Gcontext, pixmap: Pixmap, cx: i16, cy: i16) {
    c.change_gc(gc, &ChangeGCAux::new().foreground(SLIDER_FILL)).ok();
    circle(c, gc, pixmap, cx, cy, 9);
    c.change_gc(gc, &ChangeGCAux::new().foreground(SLIDER_KNOB)).ok();
    circle(c, gc, pixmap, cx, cy, 7);
}

/// Rounded-rect illusion: rect + 2 half-circle side caps.
fn rrect(c: &RustConnection, gc: Gcontext, pm: Pixmap, x: i16, y: i16, w: u16, h: i16, col: u32) {
    c.change_gc(gc, &ChangeGCAux::new().foreground(col)).ok();
    let hw = u16::try_from(h).unwrap_or(u16::MAX);
    c.poly_fill_rectangle(pm, gc, &[Rectangle {
        x: x + h / 2,
        y,
        width: w.saturating_sub(hw),
        height: hw,
    }]).ok();
    if h >= 2 {
        c.poly_fill_arc(pm, gc, &[Arc { x, y, width: hw, height: hw, angle1: 90 * 64, angle2: 180 * 64 }]).ok();
        c.poly_fill_arc(pm, gc, &[Arc { x: x + i16::try_from(w).unwrap_or(i16::MAX) - h, y, width: hw, height: hw, angle1: -90 * 64, angle2: 180 * 64 }]).ok();
    }
}

fn circle(c: &RustConnection, gc: Gcontext, pixmap: Pixmap, cx: i16, cy: i16, r: i16) {
    c.poly_fill_arc(pixmap, gc, &[Arc {
        x: cx - r, y: cy - r, width: (r * 2) as u16, height: (r * 2) as u16, angle1: 0, angle2: 360 * 64,
    }]).ok();
}

/// Sun glyph: warm-yellow disc + 5 rays (matches the tray logo), header row.
fn sun_glyph(c: &RustConnection, gc: Gcontext, pixmap: Pixmap, cx: i16, cy: i16) {
    c.change_gc(gc, &ChangeGCAux::new().foreground(SUN)).ok();
    circle(c, gc, pixmap, cx, cy, 5);
    c.change_gc(gc, &ChangeGCAux::new().line_width(2)).ok();
    for (dx, dy) in [(0i16, -10i16), (-7, -7), (7, -7), (-7, 7), (7, 7)] {
        c.poly_segment(pixmap, gc, &[Segment {
            x1: cx + dx * 3 / 5, y1: cy + dy * 3 / 5, x2: cx + dx, y2: cy + dy,
        }]).ok();
    }
    c.change_gc(gc, &ChangeGCAux::new().line_width(1)).ok();
}


/// Software-render the logo mark at 24x24 ARGB32 (for the SNI IconPixmap):
/// sun disc + 3 rays + slider track + accent fill + white knob.
fn logo_argb24(_level: u32) -> Vec<u8> {
    const S: usize = 24;
    let mut px = vec![0u8; S * S * 4]; // 0x00000000 = fully transparent
    let put = |px: &mut [u8], x: i32, y: i32, argb: u32| {
        if (0..S as i32).contains(&x) && (0..S as i32).contains(&y) {
            let i = (y as usize * S + x as usize) * 4;
            px[i] = (argb >> 24) as u8;
            px[i + 1] = (argb >> 16) as u8;
            px[i + 2] = (argb >> 8) as u8;
            px[i + 3] = argb as u8;
        }
    };
    let disc = |px: &mut [u8], cx: i32, cy: i32, r: i32, argb: u32| {
        for dy in -r..=r {
            for dx in -r..=r {
                if dx * dx + dy * dy <= r * r {
                    put(px, cx + dx, cy + dy, argb);
                }
            }
        }
    };
    let rect = |px: &mut [u8], x0: i32, y0: i32, w: i32, h: i32, argb: u32| {
        for y in y0..y0 + h {
            for x in x0..x0 + w {
                put(px, x, y, argb);
            }
        }
    };
    let opaque = 0xFF00_0000u32;
    let sun = opaque | 0xFFC94D & 0x00FF_FFFF; // warm sun yellow — always vivid
    // Sun: disc r=7 at (12,9), 5 rays (up + 4 diagonals) — reads clean at 24px.
    disc(&mut px, 12, 9, 7, sun);
    for (dx, dy) in [(0i32, -1i32), (1, -1), (-1, -1), (1, 1), (-1, 1)] {
        for t in 9..=11 {
            put(&mut px, 12 + dx * t, 9 + dy * t, sun);
        }
    }
    // erase a slider lane through the sun, then draw track+fill+knob
    for y in 17..=19 {
        for x in 0..24 {
            let i = (y * S + x) * 4;
            px[i] = 0; px[i + 1] = 0; px[i + 2] = 0; px[i + 3] = 0;
        }
    }
    rect(&mut px, 2, 17, 20, 3, opaque | 0x4A6FD4 & 0x00FF_FFFF);
    rect(&mut px, 2, 17, 11, 3, opaque | 0x7DA2FF & 0x00FF_FFFF);
    disc(&mut px, 14, 18, 3, opaque | 0x00FF_FFFF);
    px
}
fn escape_keycode(conn: &RustConnection) -> Option<Keycode> {
    let setup = conn.setup();
    let min = setup.min_keycode;
    let count = u8::try_from(u32::from(setup.max_keycode) - u32::from(min) + 1).unwrap_or(u8::MAX);
    let res = conn.get_keyboard_mapping(min, count).ok()?.reply().ok()?;
    let per = res.keysyms_per_keycode.max(1) as usize;
    for (i, chunk) in res.keysyms.chunks(per).enumerate() {
        if chunk.contains(&0xFF1B) && i <= u8::MAX as usize {
            return Some(min + i as u8);
        }
    }
    None
}

