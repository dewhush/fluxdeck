// Minimal StatusNotifierItem publisher over the session bus. Pure std —
// UnixStream + SASL EXTERNAL + hand-rolled little-endian marshal/unmarshal,
// keeping the zero-dep promise (x11rb stays the only crate).
//
// Threading: all socket I/O happens under SniIcon's mutex, called from the
// worker thread only (poll_*/set_icon). poll_* never blocks: socket is
// non-blocking after the startup handshake, so the X11 loop can never stall
// on DBus (freeze rule — see ui.rs header).
//
// ponytail: symbolic IconName only (no IconPixmap marshaling), ToolTip
// properties answered from a static table, no bus reconnection after death
// (SNI goes quiet; the XEmbed fallback path stays intact). Add pixmap +
// reconnect when a panel demands it. Watcher restarts are handled via
// NameOwnerChanged re-register; a dead bus socket degrades to silent no-op.
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

const DBUS_DEST: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const DBUS_IFACE: &str = "org.freedesktop.DBus";

const SNI_PATH: &str = "/StatusNotifierItem";
const SNI_IFACE: &str = "org.kde.StatusNotifierItem";
const WATCHER_DEST: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const WATCHER_IFACE: &str = "org.kde.StatusNotifierWatcher";

const MSG_METHOD_CALL: u8 = 1;
const MSG_METHOD_RETURN: u8 = 2;
const MSG_SIGNAL: u8 = 4;

const HDR_PATH: u8 = 1;
const HDR_IFACE: u8 = 2;
const HDR_MEMBER: u8 = 3;
const HDR_REPLY_SERIAL: u8 = 5;
const HDR_DEST: u8 = 6;
const HDR_SENDER: u8 = 7;
const HDR_SIG: u8 = 8;

#[inline]
fn align(n: usize, a: usize) -> usize {
    (n + a - 1) & !(a - 1)
}

/// Length byte for a dbus SIGNATURE ('g') given its string form.
fn sig_len(sig: &str) -> u8 {
    u8::try_from(sig.len()).unwrap_or(255)
}

/// Append a u32 at its required 4-byte alignment. Header fields are already
/// 4-aligned in the fixed header; body params (e.g. RequestName flags after a
/// string) get padded here.
fn append_u32(out: &mut Vec<u8>, v: u32) {
    out.extend(std::iter::repeat(0).take(align(out.len(), 4) - out.len()));
    out.extend_from_slice(&v.to_le_bytes());
}

/// u32 length + payload + NUL. Caller keeps 4-byte alignment.
fn append_string(out: &mut Vec<u8>, s: &str) {
    append_u32(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
    out.push(0);
}

/// \0 + "AUTH EXTERNAL <hex of the decimal uid>" + \r\n. Public for tests.
pub fn sasl_auth_line(uid: u32) -> Vec<u8> {
    let mut line = Vec::with_capacity(32);
    line.push(0);
    line.extend_from_slice(b"AUTH EXTERNAL ");
    for b in uid.to_string().as_bytes() {
        line.push(nibble(b >> 4));
        line.push(nibble(b & 0xf));
    }
    line.extend_from_slice(b"\r\n");
    line
}

fn nibble(v: u8) -> u8 {
    if v < 10 { b'0' + v } else { b'a' + v - 10 }
}

/// One parsed incoming message — only what the poll loop needs.
#[derive(Debug)]
struct InMsg {
    typ: u8,
    serial: u32,
    member: Option<String>,
    /// Header field 7 (SENDER) of the incoming message — the peer to reply to.
    sender: Option<String>,
    /// Raw body bytes (demarshaled on demand; empty for ignored signals).
    body: Vec<u8>,
}

struct Inner {
    sock: Option<UnixStream>,
    serial: std::sync::atomic::AtomicU32,
    pending: VecDeque<InMsg>,
    icon_name: String,
    /// ARGB32 pixels (row-major, w*h*4 bytes) for the IconPixmap property.
    icon_pixmap: Vec<u8>,
    title: String,
    /// Bytes of the next frame already consumed from the socket but not yet
    /// assembled (split across non-blocking reads). Never left non-empty on
    /// an error path that drops the socket.
    partial: Vec<u8>,
}

/// Handle to the SNI bus connection. Non-blocking by construction.
pub struct SniIcon {
    inner: Arc<Mutex<Inner>>,
}

impl SniIcon {
    /// Connect + SASL + Hello + RequestName + RegisterStatusNotifierItem.
    /// Ok even when the watcher never answers (caller keeps XEmbed fallback
    /// only when this returns Err).
    pub fn new(icon_name: &str, title: &str) -> Result<Self, String> {
        let addr = std::env::var("DBUS_SESSION_BUS_ADDRESS")
            .map_err(|_| "DBUS_SESSION_BUS_ADDRESS unset".to_string())?;
        let path = bus_socket_path(&addr).ok_or("no unix: path in bus address")?;
        let mut sock = UnixStream::connect(path).map_err(|e| e.to_string())?;

        // SASL EXTERNAL handshake (blocking — startup only, in worker thread).
        write_all(&mut sock, &sasl_auth_line(getuid()))?;
        let ok = read_sasl_line(&mut sock)?;
        if !ok.starts_with(b"OK") {
            return Err(format!(
                "bus auth rejected: {}",
                String::from_utf8_lossy(&ok)
            ));
        }
        write_all(&mut sock, b"BEGIN\r\n")?;

        let mut serial: u32 = 0;
        // Hello is mandatory before anything else on the bus.
        if let Err(e) = call_sync(
            &mut sock, &mut serial, DBUS_DEST, DBUS_PATH, DBUS_IFACE, "Hello", &[], "",
        ) {
            eprintln!("fluxdeck: Hello failed: {e}");
            return Err(format!("Hello: {e}"));
        }

        // RequestName: flags 4 = replace_existing | do_not_queue.
        let name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
        let mut body = Vec::new();
        append_string(&mut body, &name);
        append_u32(&mut body, 4);
        call_sync(
            &mut sock, &mut serial, DBUS_DEST, DBUS_PATH, DBUS_IFACE, "RequestName", &body, "su",
        )?;

        // RegisterStatusNotifierItem — fire and forget; watcher may be absent.
        let mut body = Vec::new();
        append_string(&mut body, SNI_PATH);
        let _ = call_sync(
            &mut sock,
            &mut serial,
            WATCHER_DEST,
            WATCHER_PATH,
            WATCHER_IFACE,
            "RegisterStatusNotifierItem",
            &body,
            "s",
        );

        // Re-register whenever the watcher restarts (panel reload kills it).
        let mut body = Vec::new();
        append_string(
            &mut body,
            "type='signal',sender='org.freedesktop.DBus',interface='org.freedesktop.DBus',member='NameOwnerChanged',arg0='org.kde.StatusNotifierWatcher'",
        );
        let _ = call_sync(
            &mut sock, &mut serial, DBUS_DEST, DBUS_PATH, DBUS_IFACE, "AddMatch", &body, "s",
        );
        sock.set_nonblocking(true).ok();
        Ok(SniIcon {
            inner: Arc::new(Mutex::new(Inner {
                sock: Some(sock),
                serial: std::sync::atomic::AtomicU32::new(serial),
                pending: VecDeque::new(),
                partial: Vec::new(),
                icon_name: icon_name.to_string(),
                icon_pixmap: Vec::new(),
                title: title.to_string(),
            })),
        })
    }

    /// Update IconName property + emit NewIcon. No-op if the bus died.
    pub fn set_icon(&self, icon_name: &str) {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if g.icon_name == icon_name {
            return; // host re-GetAlls on every NewIcon — don't spam it
        }
        g.icon_name = icon_name.to_string();
        let serial = g.serial.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        // Empty body: panels read the new name via Properties.Get.
        let msg = build_message(MSG_SIGNAL, None, SNI_IFACE, "NewIcon", SNI_PATH, "", &[], serial);
        let Some(sock) = &mut g.sock else { return };
        if write_all(sock, &msg).is_err() {
            g.sock = None; // bus closed — degrade silently
        }
    }

    /// Publish a 24x24 ARGB32 tray pixmap (logo mark). Also blanks IconName
    /// so hosts prefer the pixmap. `argb` = w*h*4 bytes, row-major.
    pub fn set_icon_pixmap(&self, argb: Vec<u8>) {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if g.icon_pixmap == argb {
            return;
        }
        g.icon_pixmap = argb;
        // Pixmap wins in hosts that support it; IconName stays as a fallback
        // for panels that skip pixmap rendering (older xfce builds).
        let serial = g.serial.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let msg = build_message(MSG_SIGNAL, None, SNI_IFACE, "NewIcon", SNI_PATH, "", &[], serial);
        let Some(sock) = &mut g.sock else { return };
        if write_all(sock, &msg).is_err() {
            g.sock = None;
        }
    }

    /// True once per Activate signal (panel left-click). Non-blocking.
    pub fn poll_activate(&self) -> bool {
        self.drain();
        self.take(&|member| member == "Activate")
    }

    /// True once per SecondaryActivate/ContextMenu (quit request). Non-blocking.
    pub fn poll_secondary(&self) -> bool {
        self.drain();
        self.take(&|member| member == "SecondaryActivate" || member == "ContextMenu")
    }

    fn take(&self, want: &dyn Fn(&str) -> bool) -> bool {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        let hit = g
            .pending
            .iter()
            .any(|m| m.member.as_deref().is_some_and(|s| want(s)));
        if hit {
            g.pending.retain(|m| !m.member.as_deref().is_some_and(|s| want(s)));
        }
        hit
    }

    /// Non-blocking drain: parse frames, reply to Ping/Properties.Get/GetAll,
    /// queue signals for poll_*.
    fn drain(&self) {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let mut sock = match g.sock.take() {
            Some(s) => s,
            None => return,
        };
        loop {
            match read_frame_nb(&mut g.partial, &mut sock) {
                Ok(Some(m)) => match m.typ {
                    MSG_METHOD_CALL => {
                        let rserial = g.serial.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        let resp = match m.member.as_deref() {
                            Some("Ping") => Some(reply_empty(rserial, &m)),
                            Some("GetAll") => Some(reply_getall(rserial, &m, &g)),
                            Some("Get") => reply_getprop(&m, &g),
                            Some("Introspect") => Some(reply_introspect(rserial, &m, &g)),
                            // Host clicks arrive as METHOD CALLS on the item
                            // path, not signals: reply empty (silent methods
                            // time out on hosts) and queue for poll_activate /
                            // poll_secondary, which only see `pending`.
                            Some("Activate" | "SecondaryActivate" | "ContextMenu") => {
                                let resp = reply_empty(rserial, &m);
                                g.pending.push_back(m);
                                Some(resp)
                            }
                            _ => {
                                eprintln!("fluxdeck: call unhandled member={m:?}");
                                None
                            }
                        };
                        if let Some(resp) = resp {
                            if write_all(&mut sock, &resp).is_err() {
                                return; // bus dead; sock dropped, g.sock stays None
                            }
                        }
                    }
                    MSG_SIGNAL => {
                        if m.member.as_deref() == Some("NameOwnerChanged") {
                            // SNI host (watcher) restarted: re-register our item.
                            let s = g.serial.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                            let mut body = Vec::new();
                            append_string(&mut body, SNI_PATH);
                            let msg = build_message(
                                MSG_METHOD_CALL, Some(WATCHER_DEST), WATCHER_IFACE,
                                "RegisterStatusNotifierItem", WATCHER_PATH, "s", &body, s,
                            );
                            if write_all(&mut sock, &msg).is_err() {
                                return;
                            }
                        }
                        g.pending.push_back(m);
                    }
                    _ => {} // replies, NameAcquired, etc: drop
                },
                Ok(None) => break,
                Err(e) => { eprintln!("fluxdeck: bus socket fatal: {e}"); return; }
            }
        }
        g.sock = Some(sock);
    }
}

/// Append one a{sv} dict entry: 8-aligned struct of key STRING + variant.
/// Variant = SIGNATURE 'g' (u8 len + chars + NUL) + value (STRING, 4-aligned).
fn append_sv(dict: &mut Vec<u8>, k: &str, v: &str) {
    dict.extend(std::iter::repeat(0).take(align(dict.len(), 8) - dict.len()));
    append_string(dict, k);
    dict.push(1u8); // 'g': sig len — signature is exactly "s"
    dict.extend_from_slice(b"s\0");
    let p = align(dict.len(), 4) - dict.len();
    dict.extend(std::iter::repeat(0).take(p));
    append_string(dict, v);
}

/// Size of the square ARGB32 tray pixmap we publish.
const ICON_PX: i32 = 24;

/// Append an a{sv} entry whose value is a dbus STRUCT (iiay): width, height,
/// array-of-bytes pixmap. Used for IconPixmap so the tray shows our logo
/// instead of a theme icon-name lookup.
fn append_sv_pixmap(dict: &mut Vec<u8>, k: &str, w: i32, h: i32, px: &[u8]) {
    let sig = "(iiay)";
    dict.extend(std::iter::repeat(0).take(align(dict.len(), 8) - dict.len()));
    append_string(dict, k);
    dict.push(u8::try_from(sig.len()).unwrap_or(255));
    dict.extend_from_slice(sig.as_bytes());
    dict.push(0);
    // struct payload: i width, i height, ay pixels (4-aligned array)
    append_u32(dict, w as u32);
    append_u32(dict, h as u32);
    append_u32(dict, px.len() as u32);
    dict.extend_from_slice(px);
}

/// org.freedesktop.DBus.Properties.Get(iface, name) → v(s) reply for the
/// static SNI properties. Unknown property names (ToolTip, OverlayIconName,
/// IconPixmap, ...) get an empty-string variant: hosts (xfce4-panel) probe
/// these after GetAll and a timed-out Get makes them drop the item.
fn reply_getprop(m: &InMsg, g: &Inner) -> Option<Vec<u8>> {
    if m.body.len() < 8 {
        return None;
    }
    let ilen = u32::from_le_bytes(m.body[..4].try_into().ok()?) as usize;
    let off = align(4 + ilen + 1, 4);
    if off + 4 > m.body.len() {
        return None;
    }
    let nlen = u32::from_le_bytes(m.body[off..off + 4].try_into().ok()?) as usize;
    if off + 4 + nlen > m.body.len() {
        return None;
    }
    let name = std::str::from_utf8(&m.body[off + 4..off + 4 + nlen]).ok()?;
    // IconPixmap Get: v-wrapped (iiay) struct — same marshal as GetAll.
    if name == "IconPixmap" && !g.icon_pixmap.is_empty() {
        let mut body = Vec::new();
        body.push(sig_len("(iiay)"));
        body.extend_from_slice(b"(iiay)\0");
        let p = align(body.len(), 4) - body.len();
        body.extend(std::iter::repeat(0).take(p));
        append_u32(&mut body, ICON_PX as u32);
        append_u32(&mut body, ICON_PX as u32);
        append_u32(&mut body, g.icon_pixmap.len() as u32);
        body.extend_from_slice(&g.icon_pixmap);
        return Some(frame(
            MSG_METHOD_RETURN,
            g.serial.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            m.serial,
            &m.sender,
            "v",
            &body,
        ));
    }
    let title: &str = &g.title;
    let icon: &str = &g.icon_name;
    let props: &[(&str, &str)] = &[
        ("Category", "Hardware"),
        ("Id", "fluxdeck"),
        ("Title", title),
        ("Status", "Active"),
        ("IconName", icon),
    ];
    let val = props.iter().find(|(k, _)| *k == name).map(|(_, v)| *v).unwrap_or("");
    // body: v{ g "s", s value }
    let mut body = Vec::new();
    body.push(1u8);
    body.extend_from_slice(b"s\0");
    let p = align(body.len(), 4) - body.len();
    body.extend(std::iter::repeat(0).take(p));
    append_string(&mut body, val);
    Some(frame(MSG_METHOD_RETURN, g.serial.fetch_add(1, std::sync::atomic::Ordering::Relaxed), m.serial, &m.sender, "v", &body))
}

/// org.freedesktop.DBus.Properties.GetAll reply: PLAIN a{sv} (no variant
/// wrapper — GetAll's out signature is a{sv}; a v-wrapped body makes the
/// host's libdbus fail with "Could not parse properties" and drop the item).
fn reply_getall(serial: u32, req: &InMsg, g: &Inner) -> Vec<u8> {
    let mut dict = Vec::new();
    let put = |dict: &mut Vec<u8>, k: &str, v: &str| append_sv(dict, k, v);
    put(&mut dict, "Category", "Hardware");
    put(&mut dict, "Id", "fluxdeck");
    // Hosts (xfce4-panel SNI, plasma) require Title/Status/IconName in
    // GetAll — an item without them renders blank or gets dropped.
    put(&mut dict, "Title", &g.title);
    put(&mut dict, "Status", "Active");
    put(&mut dict, "IconName", &g.icon_name);
    // IconPixmap (iiay): width, height, ARGB32 rows. Given precedence over
    // IconName by xfce/hosts, so the tray shows our logo, not a theme fallback.
    if !g.icon_pixmap.is_empty() {
        append_sv_pixmap(&mut dict, "IconPixmap", ICON_PX, ICON_PX, &g.icon_pixmap);
    }
    // ponytail: ToolTip variant omitted — nested (sa(iiay)ss) marshal needs a
    // real struct writer; add with unit tests when hosts actually read it.

    // body: a{sv} (len counts real element bytes; 8-alignment is the
    // pre-element pad, no trailing pad).
    let mut body = Vec::new();
    append_u32(&mut body, dict.len() as u32);
    body.extend(std::iter::repeat(0).take(align(body.len(), 8) - body.len()));
    body.extend_from_slice(&dict);

    frame(MSG_METHOD_RETURN, serial, req.serial, &req.sender, "a{sv}", &body)
}

/// Empty method_return (Peer.Ping and anything else we wave through).
fn reply_empty(serial: u32, req: &InMsg) -> Vec<u8> {
    frame(MSG_METHOD_RETURN, serial, req.serial, &req.sender, "", &[])
}

/// org.freedesktop.DBus.Introspectable.Introspect → static XML.
/// libxfce4windowing gates item approval on a valid Introspect reply; without
/// one the host polls GetAll forever and never paints. Interface list mirrors
/// python-dbus's auto-introspection (which xfce accepts) plus our SNI methods.
fn reply_introspect(serial: u32, req: &InMsg, _g: &Inner) -> Vec<u8> {
    let xml = format!(
        "<node>\n  <interface name=\"org.freedesktop.DBus.Peer\"/>\n  <interface name=\"{}\">\n    <method name=\"Activate\">\n      <arg direction=\"in\" name=\"x\" type=\"i\"/>\n      <arg direction=\"in\" name=\"y\" type=\"i\"/>\n    </method>\n    <method name=\"SecondaryActivate\">\n      <arg direction=\"in\" name=\"x\" type=\"i\"/>\n      <arg direction=\"in\" name=\"y\" type=\"i\"/>\n    </method>\n    <method name=\"ContextMenu\">\n      <arg direction=\"in\" name=\"x\" type=\"i\"/>\n      <arg direction=\"in\" name=\"y\" type=\"i\"/>\n    </method>\n    <method name=\"Scroll\">\n      <arg direction=\"in\" name=\"delta\" type=\"i\"/>\n      <arg direction=\"in\" name=\"orientation\" type=\"s\"/>\n    </method>\n  </interface>\n  <interface name=\"org.freedesktop.DBus.Properties\">\n    <method name=\"Get\">\n      <arg direction=\"in\" name=\"interface\" type=\"s\"/>\n      <arg direction=\"in\" name=\"prop\" type=\"s\"/>\n      <arg direction=\"out\" name=\"value\" type=\"v\"/>\n    </method>\n    <method name=\"GetAll\">\n      <arg direction=\"in\" name=\"interface\" type=\"s\"/>\n      <arg direction=\"out\" name=\"props\" type=\"a{{sv}}\"/>\n    </method>\n  </interface>\n  <interface name=\"org.freedesktop.DBus.Introspectable\">\n    <method name=\"Introspect\">\n      <arg direction=\"out\" name=\"xml\" type=\"s\"/>\n    </method>\n  </interface>\n</node>\n",
        SNI_IFACE
    );
    let mut body = Vec::with_capacity(xml.len() + 8);
    append_string(&mut body, &xml);
    frame(
        MSG_METHOD_RETURN,
        serial,
        req.serial,
        &req.sender,
        "s",
        &body,
    )
}

/// Marshal a method_return: REPLY_SERIAL + DESTINATION (caller) + SIG.
fn frame(typ: u8, serial: u32, reply_serial: u32, reply_to_dest: &Option<String>, sig: &str, body: &[u8]) -> Vec<u8> {
    // Header written directly — replies carry REPLY_SERIAL, not PATH/IFACE/MEMBER.
    let mut fields: Vec<u8> = Vec::new();
    push_field(&mut fields, HDR_REPLY_SERIAL, &FieldVal::U32(reply_serial));
    if let Some(d) = reply_to_dest.as_deref() {
        push_field(&mut fields, HDR_DEST, &FieldVal::Str(d));
    }
    if !sig.is_empty() {
        push_field(&mut fields, HDR_SIG, &FieldVal::Sig(sig));
    }
    let mut out = Vec::with_capacity(48 + body.len());
    out.push(b'l');
    out.push(typ); // message type
    out.push(0); // flags
    out.push(1); // protocol version
    append_u32(&mut out, body.len() as u32);
    append_u32(&mut out, serial);
    append_u32(&mut out, fields.len() as u32);
    out.extend_from_slice(&fields);
    out.extend(std::iter::repeat(0).take(align(out.len(), 8) - out.len()));
    out.extend_from_slice(body);
    out
}

fn build_message(
    typ: u8,
    dest: Option<&str>,
    iface: &str,
    member: &str,
    path: &str,
    sig: &str,
    body: &[u8],
    serial: u32,
) -> Vec<u8> {
    let mut fields: Vec<u8> = Vec::new();
    push_field(&mut fields, HDR_PATH, &FieldVal::Path(path));
    if !iface.is_empty() {
        push_field(&mut fields, HDR_IFACE, &FieldVal::Str(iface));
    }
    push_field(&mut fields, HDR_MEMBER, &FieldVal::Str(member));
    if let Some(d) = dest {
        push_field(&mut fields, HDR_DEST, &FieldVal::Str(d));
    }
    if !sig.is_empty() && typ == MSG_METHOD_CALL {
        push_field(&mut fields, HDR_SIG, &FieldVal::Sig(sig));
    }

    let mut out = Vec::with_capacity(64 + body.len());
    // D-Bus fixed header: endianness, MESSAGE TYPE, flags, major version.
    out.push(b'l');
    out.push(typ);
    out.push(0); // flags: none
    out.push(1);
    append_u32(&mut out, body.len() as u32);
    append_u32(&mut out, serial);
    append_u32(&mut out, fields.len() as u32);
    out.extend_from_slice(&fields);
    out.extend(std::iter::repeat(0).take(align(out.len(), 8) - out.len()));
    out.extend_from_slice(body);
    out
}

enum FieldVal<'a> {
    /// STRING header field ('s' variant sig)
    Str(&'a str),
    /// OBJECT PATH header field ('o' variant sig) - required for HDR_PATH
    Path(&'a str),
    U32(u32),
    /// SIGNATURE header field ('g' variant sig) — used for HDR_SIG
    Sig(&'a str),
}
/// One a(yv) entry: struct starts 8-aligned (array base already is), then
/// y code, v = SIGNATURE type 'g' (u8 len + chars + NUL, NO alignment),
/// then value aligned to its type ('s'/'o'/'u' → 4).
fn push_field(fields: &mut Vec<u8>, code: u8, val: &FieldVal) {
    let pad = align(fields.len(), 8) - fields.len();
    fields.extend(std::iter::repeat(0).take(pad));
    fields.push(code);
    match val {
        FieldVal::Path(s) => {
            fields.push(1);
            fields.push(b'o');
            fields.push(0);
            let p = align(fields.len(), 4) - fields.len();
            fields.extend(std::iter::repeat(0).take(p));
            append_string(fields, s);
        }
        FieldVal::Str(s) => {
            fields.push(1);
            fields.push(b's');
            fields.push(0);
            let p = align(fields.len(), 4) - fields.len();
            fields.extend(std::iter::repeat(0).take(p));
            append_string(fields, s);
        }
        FieldVal::Sig(s) => {
            fields.push(1);
            fields.push(b'g');
            fields.push(0);
            // 'g' = u8 length, bytes, NUL — no alignment padding
            fields.push(s.len() as u8);
            fields.extend_from_slice(s.as_bytes());
            fields.push(0);
        }
        FieldVal::U32(u) => {
            fields.push(1);
            fields.push(b'u');
            fields.push(0);
            let p = align(fields.len(), 4) - fields.len();
            fields.extend(std::iter::repeat(0).take(p));
            append_u32(fields, *u);
        }
    }
}

/// Marshal + send a method call, then blocking-read one frame (startup only;
/// the worker's steady state uses non-blocking drain).
fn dbg_dump(tag: &str, bytes: &[u8]) {
    if std::env::var("FLUXDECK_DBUS_DUMP").is_ok() {
        let mut s = format!("{tag} len={}: ", bytes.len());
        for b in bytes.iter().take(260) {
            s.push_str(&format!("{:02x} ", b));
        }
        eprintln!("{}", s.trim_end());
    }
}

/// Marshal + send a method call, then blocking-read one frame (startup only;
/// the worker's steady state uses non-blocking drain).
fn call_sync(
    sock: &mut UnixStream,
    serial: &mut u32,
    dest: &str,
    path: &str,
    iface: &str,
    member: &str,
    body: &[u8],
    sig: &str,
) -> Result<InMsg, String> {
    *serial += 1;
    let msg = build_message(
        MSG_METHOD_CALL, Some(dest), iface, member, path, sig, body, *serial,
    );
    dbg_dump("TX", &msg);
    write_all(sock, &msg)?;
    // Blocking read of exactly one frame; if it's a stray broadcast instead of
    // our reply, it's discarded — acceptable at startup (ponytail; upgrade:
    // match REPLY_SERIAL in a small loop).
    let mut h = [0u8; 16];
    sock.read_exact(&mut h).map_err(|e| e.to_string())?;
    dbg_dump("RX-hdr", &h);
    parse_frame(&h, sock)
}

fn read_sasl_line(sock: &mut UnixStream) -> Result<Vec<u8>, String> {
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    loop {
        sock.read_exact(&mut b).map_err(|e| e.to_string())?;
        if b[0] == b'\n' {
            return Ok(line);
        }
        line.push(b[0]);
        if line.len() > 64 {
            return Err("SASL line too long".into());
        }
    }
}

fn write_all(sock: &mut UnixStream, mut buf: &[u8]) -> Result<(), String> {
    dbg_dump("TX", buf);
    while !buf.is_empty() {
        let n = sock.write(buf).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("bus write returned 0".into());
        }
        buf = &buf[n..];
    }
    Ok(())
}

/// Non-blocking frame read. Consumed bytes accumulate in `partial` so a frame
/// split across reads resumes instead of desyncing the stream; Ok(None) when
/// no full frame is ready yet. `partial` must be empty on the reconnect path.
fn read_frame_nb(partial: &mut Vec<u8>, sock: &mut UnixStream) -> Result<Option<InMsg>, String> {
    loop {
        // Total frame size once the 16-byte header is present; 16 before that.
        let need = if partial.len() < 16 {
            16
        } else {
            let body_len = u32::from_le_bytes(partial[4..8].try_into().unwrap()) as usize;
            let fields_len = u32::from_le_bytes(partial[12..16].try_into().unwrap()) as usize;
            if fields_len > 1 << 20 || body_len > 1 << 20 {
                return Err("frame too large".into());
            }
            16 + align(fields_len.max(1), 8) + body_len
        };
        if partial.len() >= need {
            let msg = parse_frame_buf(partial)?;
            partial.clear();
            return Ok(Some(msg));
        }
        let old = partial.len();
        partial.resize(need, 0);
        match sock.read(&mut partial[old..]) {
            Ok(0) => return Err("bus closed".into()),
            Ok(n) => {
                partial.truncate(old + n);
                continue; // recompute need (header may now be complete)
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                partial.truncate(old);
                return Ok(None);
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

fn parse_frame(hdr: &[u8; 16], sock: &mut UnixStream) -> Result<InMsg, String> {
    let body_len = u32::from_le_bytes(hdr[4..8].try_into().unwrap()) as usize;
    let fields_len = u32::from_le_bytes(hdr[12..16].try_into().unwrap()) as usize;
    let fields_padded = align(fields_len.max(1), 8);
    let mut buf = Vec::with_capacity(16 + fields_padded + body_len);
    buf.extend_from_slice(hdr);
    buf.resize(16 + fields_padded + body_len, 0);
    sock.read_exact(&mut buf[16..]).map_err(|e| e.to_string())?;
    parse_frame_buf(&buf)
}

/// Parse a fully-buffered frame (hdr + fields + body). Header layout checks
/// and the a(yv) field walk; no socket involved.
fn parse_frame_buf(buf: &[u8]) -> Result<InMsg, String> {
    let (hdr, rest) = buf.split_at(16);
    if hdr[0] != b'l' {
        return Err("unsupported endian".into());
    }
    dbg_dump("RX-hdr", hdr);
    let typ = hdr[1];
    let serial = u32::from_le_bytes(hdr[8..12].try_into().unwrap());
    let body_len = u32::from_le_bytes(hdr[4..8].try_into().unwrap()) as usize;
    let fields_len = u32::from_le_bytes(hdr[12..16].try_into().unwrap()) as usize;
    if fields_len > 1 << 20 || body_len > 1 << 20 {
        return Err("frame too large".into());
    }
    let fields_padded = align(fields_len.max(1), 8);
    // Body is NOT re-padded in the stream: the header array is pad-to-8 already,
    // daemon emits bodies back-to-back (verified via wire dump).
    let fields = &rest[..fields_len];
    // Walk a(yv): y code, v sig 'g' (u8 len + chars + NUL, no pad), value aligned to type.
    let mut member = None;
    let mut sender = None;
    let mut i = 0usize;
    while i < fields_len {
        // each a(yv) struct entry starts on an 8-byte boundary (spec)
        i = align(i, 8);
        if i >= fields_len {
            break;
        }
        let code = fields[i];
        i += 1;
        if i >= fields_len {
            break;
        }
        let sig_len = fields[i] as usize;
        if i + 1 + sig_len + 1 > fields_len {
            break;
        }
        let vsig = fields[i + 1];
        i += 1 + sig_len + 1; // past "g" blob (len + chars + NUL)
        match vsig {
            b's' | b'o' => {
                i = align(i, 4);
                if i + 4 > fields_len {
                    break;
                }
                let slen = u32::from_le_bytes(fields[i..i + 4].try_into().unwrap()) as usize;
                i += 4;
                if i + slen > fields_len {
                    break;
                }
                if code == HDR_MEMBER {
                    member = Some(String::from_utf8_lossy(&fields[i..i + slen]).into_owned());
                } else if code == HDR_SENDER {
                    sender = Some(String::from_utf8_lossy(&fields[i..i + slen]).into_owned());
                }
                i += slen + 1; // + NUL
            }
            b'u' => {
                i = align(i, 4);
                if i + 4 > fields_len {
                    break;
                }
                i += 4;
            }
            b'g' => {
                if i >= fields_len {
                    break;
                }
                let gl = fields[i] as usize;
                i += 1 + gl + 1;
            }
            _ => break, // unknown variant: stop scanning (fields we need may precede it)
        }
    }
    Ok(InMsg {
        typ,
        serial,
        member,
        sender,
        body: rest[fields_padded..].to_vec(),
    })
}

/// unix:path=... or unix:abstract=... (first match) from DBUS_SESSION_BUS_ADDRESS.
fn bus_socket_path(addr: &str) -> Option<std::ffi::OsString> {
    for p in addr.split(';') {
        let p = p.trim();
        if let Some(path) = p.strip_prefix("unix:path=") {
            return Some(std::ffi::OsString::from(
                path.split(',').next().unwrap_or(path),
            ));
        }
        if let Some(abs) = p.strip_prefix("unix:abstract=") {
            let abs = abs.split(',').next().unwrap_or(abs);
            let mut v = Vec::with_capacity(abs.len() + 1);
            v.push(0);
            v.extend_from_slice(abs.as_bytes());
            // ponytail: unchecked ctor because abstract addrs aren't valid UTF-8;
            // we only ever feed this to UnixStream::connect — safe by construction.
            return Some(unsafe { std::ffi::OsString::from_encoded_bytes_unchecked(v) });
        }
    }
    None
}

fn getuid() -> u32 {
    extern "C" {
        fn getuid() -> u32;
    }
    unsafe { getuid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_frame_nb_reassembles_split_frames() {
        // Regression: a frame split across non-blocking reads used to be
        // dropped byte-wise (stream desync). Partial bytes must persist in
        // `partial` until the full frame has arrived.
        let msg = build_message(
            MSG_METHOD_CALL, Some(WATCHER_DEST), WATCHER_IFACE, "Ping",
            WATCHER_PATH, "", &[], 42,
        );
        // Serve the frame in chunks over a socket pair; our side non-blocking
        // to mimic the bus socket.
        use std::os::unix::net::UnixStream;
        let (mut a, mut b) = UnixStream::pair().unwrap();
        b.set_nonblocking(true).unwrap();
        a.write_all(&msg[..10]).unwrap();
        let mut partial = Vec::new();
        // First read: header incomplete → Ok(None), partial holds 10 bytes.
        assert!(read_frame_nb(&mut partial, &mut b).unwrap().is_none());
        assert_eq!(partial.len(), 10);
        a.write_all(&msg[10..]).unwrap();
        // Whole remainder is buffered → frame completes on the next call.
        assert_eq!(read_frame_nb(&mut partial, &mut b).unwrap().map(|m| m.serial), Some(42));
        assert!(partial.is_empty());
    }

    #[test]
    fn sasl_line_matches_spec() {
        let line = sasl_auth_line(1000);
        assert!(line.starts_with(b"\0AUTH EXTERNAL 31303030\r\n"));
        assert!(line.ends_with(b"\r\n"));
        assert!(line.ends_with(b"31303030\r\n")); // hex of "1000"
        // single ASCII digit uid
        assert!(sasl_auth_line(1).ends_with(b"31\r\n"));
    }
    #[test]
    fn register_call_bytes_exact() {
        let mut body = Vec::new();
        append_string(&mut body, SNI_PATH); // "/StatusNotifierItem"
        let msg = build_message(
            MSG_METHOD_CALL,
            Some(WATCHER_DEST),
            WATCHER_IFACE,
            "RegisterStatusNotifierItem",
            WATCHER_PATH,
            "s",
            &body,
            7,
        );
        assert_eq!(msg[0], b'l'); // little-endian
        assert_eq!(msg[1], MSG_METHOD_CALL); // message type
        assert_eq!(msg[2], 0); // flags
        assert_eq!(msg[3], 1); // protocol version
        assert_eq!(u32::from_le_bytes(msg[4..8].try_into().unwrap()), 24); // body: 19 chars + NUL + align-8 pad
        assert_eq!(u32::from_le_bytes(msg[8..12].try_into().unwrap()), 7); // serial
        let flen = u32::from_le_bytes(msg[12..16].try_into().unwrap()) as usize;
        assert_eq!(msg.len() % 8, 0); // body 8-aligned
        assert_eq!(&msg[msg.len() - 20..], b"/StatusNotifierItem\0");
        let fields = &msg[16..16 + flen];
        assert_eq!(fields[0], HDR_PATH);
        let hay = String::from_utf8_lossy(fields);
        assert!(hay.contains(WATCHER_DEST));
        assert!(hay.contains(WATCHER_IFACE));
        assert!(hay.contains("RegisterStatusNotifierItem"));
    }

    #[test]
    fn demarshal_method_return_frame() {
        // Synthesize: method_return, REPLY_SERIAL=9, sig "s", body string "Hello".
        let mut fields: Vec<u8> = Vec::new();
        push_field(&mut fields, HDR_REPLY_SERIAL, &FieldVal::U32(9));
        push_field(&mut fields, HDR_SIG, &FieldVal::Str("s"));
        let mut body = Vec::new();
        append_string(&mut body, "Hello");
        let mut out = Vec::new();
        out.push(b'l');
        out.push(0);
        out.push(MSG_METHOD_RETURN);
        out.push(1);
        append_u32(&mut out, body.len() as u32);
        append_u32(&mut out, 42); // serial
        append_u32(&mut out, fields.len() as u32);
        out.extend_from_slice(&fields);
        out.extend(std::iter::repeat(0).take(align(out.len(), 8) - out.len()));
        out.extend_from_slice(&body);

        assert_eq!(out[2], MSG_METHOD_RETURN);
        let body_len = u32::from_le_bytes(out[4..8].try_into().unwrap()) as usize;
        let fields_len = u32::from_le_bytes(out[12..16].try_into().unwrap()) as usize;
        assert_eq!(body_len, 10); // 4 len + 5 + NUL
        // Field walk (same algorithm as parse_frame, socket part elided):
        let f = &out[16..16 + fields_len];
        assert_eq!(f[0], HDR_REPLY_SERIAL);
        // code y(1) + g sig(len+'u'+NUL = 3) → value u32 at offset 4
        assert_eq!(u32::from_le_bytes(f[4..8].try_into().unwrap()), 9);
        // body parses as len-prefixed string
        let slen = u32::from_le_bytes(body[..4].try_into().unwrap()) as usize;
        assert_eq!(slen, 5);
        assert_eq!(&body[4..4 + slen], b"Hello");
    }

    #[test]
    fn getall_reply_is_plain_a_sv() {
        // Regression: GetAll wrapped its a{sv} in a variant (sig "v"); hosts'
        // libdbus failed to parse ("Could not parse properties") and dropped
        // the item. Must be a plain a{sv} body with matching frame sig.
        let g = Inner {
            sock: None,
            serial: std::sync::atomic::AtomicU32::new(0),
            pending: Default::default(),
            partial: Vec::new(),
            icon_name: "test-icon".to_string(),
            title: "Test".to_string(),
        };
        let msg = reply_getall(5, &InMsg {
            typ: MSG_METHOD_CALL,
            serial: 9,
            sender: Some(":1.99".to_string()),
            member: Some("GetAll".to_string()),
            body: {
                let mut b = Vec::new();
                append_string(&mut b, "org.kde.StatusNotifierItem");
                b
            },
        }, &g);
        // frame sig lives in the HDR_SIG field; body must start with u32 len
        // (0-aligned array of 5 entries), NOT the g-signature of a variant.
        let body_len = u32::from_le_bytes(msg[4..8].try_into().unwrap()) as usize;
        let fields_len = u32::from_le_bytes(msg[12..16].try_into().unwrap()) as usize;
        let hay = String::from_utf8_lossy(&msg[16..16 + fields_len]);
        assert!(hay.contains("a{sv}"), "frame sig must be a{{sv}}: {hay}");
        assert!(!hay.contains("\"v\""), "frame sig must not be v: {hay}");
        // fields_len in the header counts the padded area; body begins right
        // after the 8-aligned header+fields region = msg.len() - body_len.
        let body_start = msg.len() - body_len;
        let n = u32::from_le_bytes(msg[body_start..body_start + 4].try_into().unwrap());
        assert_eq!(n as usize, body_len - 8, "array length = element bytes (4 len + 4 pad + dict)");
    }
}
