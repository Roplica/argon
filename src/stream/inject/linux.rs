use anyhow::{bail, Result};
use std::collections::HashMap;
use std::sync::Mutex;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ButtonPressEvent, ButtonReleaseEvent, ConnectionExt, EventMask, KeyPressEvent,
    KeyReleaseEvent, MotionNotifyEvent, Window, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT,
    KEY_PRESS_EVENT, KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT,
};
use x11rb::rust_connection::RustConnection;
use x11rb::x11_utils::Serialize;

use crate::stream::capture::InputEvent;

pub struct Injector {
    conn: RustConnection,
    root: Window,
    // (xid, win_root_x, win_root_y) — cached, invalidated on inject failure
    cached: Mutex<Option<(Window, i16, i16)>>,
    // keysym → keycode for the current keyboard layout
    keysym_to_keycode: HashMap<u32, u8>,
}

impl Injector {
    pub fn new() -> Result<Self> {
        let (conn, screen_num) = connect_to_x11()?;
        let root = conn.setup().roots[screen_num].root;
        let keysym_to_keycode = build_keysym_map(&conn, screen_num)?;
        Ok(Self { conn, root, cached: Mutex::new(None), keysym_to_keycode })
    }

    pub fn send(&self, event: &InputEvent) {
        if let Err(e) = self.try_send(event) {
            log::debug!("X11 inject: {e} — invalidating window cache");
            *self.cached.lock().unwrap() = None;
        }
    }

    fn try_send(&self, event: &InputEvent) -> Result<()> {
        let (xid, wx, wy) = match self.window_state()? {
            Some(s) => s,
            None => return Ok(()), // Roblox not found yet
        };

        match event {
            InputEvent::Keydown { key } => self.key_event(xid, key, true)?,
            InputEvent::Keyup { key } => self.key_event(xid, key, false)?,
            InputEvent::Mousemove { x, y } => {
                self.motion(xid, *x as i16, *y as i16, wx + *x as i16, wy + *y as i16)?;
            }
            InputEvent::Mousedown { button, x, y } => {
                let (ex, ey) = (*x as i16, *y as i16);
                let (rx, ry) = (wx + ex, wy + ey);
                // Send motion first so Roblox tracks cursor position at click site
                self.motion(xid, ex, ey, rx, ry)?;
                self.button(xid, *button + 1, ex, ey, rx, ry, true)?;
            }
            InputEvent::Mouseup { button, x, y } => {
                let (ex, ey) = (*x as i16, *y as i16);
                let (rx, ry) = (wx + ex, wy + ey);
                self.button(xid, *button + 1, ex, ey, rx, ry, false)?;
            }
            // Lock/Unlock/Mousedelta: camera rotation handled by the plugin
            _ => return Ok(()),
        }

        self.conn.flush()?;
        Ok(())
    }

    fn window_state(&self) -> Result<Option<(Window, i16, i16)>> {
        {
            let cached = self.cached.lock().unwrap();
            if cached.is_some() {
                return Ok(*cached);
            }
        }
        // Try to find Roblox window
        let xid = match self.find_roblox_xid() {
            Ok(id) => id,
            Err(_) => return Ok(None),
        };
        let trans = self.conn.translate_coordinates(xid, self.root, 0, 0)?.reply()?;
        let state = (xid, trans.dst_x, trans.dst_y);
        *self.cached.lock().unwrap() = Some(state);
        Ok(Some(state))
    }

    fn find_roblox_xid(&self) -> Result<Window> {
        let root = self.root;
        let net_client_list =
            self.conn.intern_atom(false, b"_NET_CLIENT_LIST")?.reply()?.atom;
        let net_wm_name = self.conn.intern_atom(false, b"_NET_WM_NAME")?.reply()?.atom;
        let utf8_string = self.conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
        let wm_name_atom = self.conn.intern_atom(false, b"WM_NAME")?.reply()?.atom;

        let list = self.conn
            .get_property(false, root, net_client_list, AtomEnum::WINDOW, 0, 4096)?
            .reply()?;
        let windows: Vec<Window> =
            list.value32().map(|v| v.collect()).unwrap_or_default();

        for win in windows {
            let net_name = self.conn
                .get_property(false, win, net_wm_name, utf8_string, 0, 512)?
                .reply()?;
            let title = if !net_name.value.is_empty() {
                String::from_utf8_lossy(&net_name.value).into_owned()
            } else {
                let wm_name = self.conn
                    .get_property(false, win, wm_name_atom, AtomEnum::STRING, 0, 512)?
                    .reply()?;
                String::from_utf8_lossy(&wm_name.value).into_owned()
            };
            if title.contains("Roblox") {
                log::debug!("X11 inject: found window XID=0x{:x} title={:?}", win, title);
                return Ok(win);
            }
        }
        bail!("Roblox window not found")
    }

    fn key_event(&self, xid: Window, code: &str, press: bool) -> Result<()> {
        let keysym = match browser_code_to_keysym(code) {
            Some(k) => k,
            None => {
                log::debug!("X11 inject: unknown key code {code:?}");
                return Ok(());
            }
        };
        let keycode = match self.keysym_to_keycode.get(&keysym).copied() {
            Some(k) => k,
            None => {
                log::debug!("X11 inject: no keycode for keysym 0x{keysym:04x} ({code})");
                return Ok(());
            }
        };

        let mask = if press { EventMask::KEY_PRESS } else { EventMask::KEY_RELEASE };

        if press {
            let ev = KeyPressEvent {
                response_type: KEY_PRESS_EVENT,
                detail: keycode,
                sequence: 0,
                time: x11rb::CURRENT_TIME,
                root: self.root,
                event: xid,
                child: 0,
                root_x: 0,
                root_y: 0,
                event_x: 0,
                event_y: 0,
                state: 0u16.into(),
                same_screen: true,
            };
            self.conn.send_event(false, xid, mask, ev.serialize())?;
        } else {
            let ev = KeyReleaseEvent {
                response_type: KEY_RELEASE_EVENT,
                detail: keycode,
                sequence: 0,
                time: x11rb::CURRENT_TIME,
                root: self.root,
                event: xid,
                child: 0,
                root_x: 0,
                root_y: 0,
                event_x: 0,
                event_y: 0,
                state: 0u16.into(),
                same_screen: true,
            };
            self.conn.send_event(false, xid, mask, ev.serialize())?;
        }
        Ok(())
    }

    fn motion(&self, xid: Window, ex: i16, ey: i16, rx: i16, ry: i16) -> Result<()> {
        let ev = MotionNotifyEvent {
            response_type: MOTION_NOTIFY_EVENT,
            detail: x11rb::protocol::xproto::Motion::NORMAL,
            sequence: 0,
            time: x11rb::CURRENT_TIME,
            root: self.root,
            event: xid,
            child: 0,
            root_x: rx,
            root_y: ry,
            event_x: ex,
            event_y: ey,
            state: 0u16.into(),
            same_screen: true,
        };
        self.conn.send_event(false, xid, EventMask::POINTER_MOTION, ev.serialize())?;
        Ok(())
    }

    fn button(
        &self,
        xid: Window,
        button: u8,
        ex: i16,
        ey: i16,
        rx: i16,
        ry: i16,
        press: bool,
    ) -> Result<()> {
        let mask = if press { EventMask::BUTTON_PRESS } else { EventMask::BUTTON_RELEASE };
        if press {
            let ev = ButtonPressEvent {
                response_type: BUTTON_PRESS_EVENT,
                detail: button,
                sequence: 0,
                time: x11rb::CURRENT_TIME,
                root: self.root,
                event: xid,
                child: 0,
                root_x: rx,
                root_y: ry,
                event_x: ex,
                event_y: ey,
                state: 0u16.into(),
                same_screen: true,
            };
            self.conn.send_event(false, xid, mask, ev.serialize())?;
        } else {
            let ev = ButtonReleaseEvent {
                response_type: BUTTON_RELEASE_EVENT,
                detail: button,
                sequence: 0,
                time: x11rb::CURRENT_TIME,
                root: self.root,
                event: xid,
                child: 0,
                root_x: rx,
                root_y: ry,
                event_x: ex,
                event_y: ey,
                state: 0u16.into(),
                same_screen: true,
            };
            self.conn.send_event(false, xid, mask, ev.serialize())?;
        }
        Ok(())
    }
}

/// Connect to X11, trying $DISPLAY first then scanning /tmp/.X11-unix/ for XWayland sockets.
/// This lets the injector work even when VS Code's environment has no DISPLAY set
/// (common when Wine is launched with a per-app DISPLAY override in Vinegar config).
fn connect_to_x11() -> Result<(RustConnection, usize)> {
    // $DISPLAY first (may already be correct on X11 sessions or if the user set it)
    if let Ok(pair) = RustConnection::connect(None) {
        log::debug!("X11 inject: connected via $DISPLAY");
        return Ok(pair);
    }

    // Scan for XWayland sockets in order :0, :1, ..., :9
    for n in 0u8..10 {
        let socket = format!("/tmp/.X11-unix/X{n}");
        if std::path::Path::new(&socket).exists() {
            let display = format!(":{n}");
            if let Ok(pair) = RustConnection::connect(Some(&display)) {
                log::info!("X11 inject: connected to XWayland on display {display}");
                return Ok(pair);
            }
        }
    }

    anyhow::bail!("No X11/XWayland display found — is XWayland running?")
}

fn build_keysym_map(conn: &RustConnection, screen_num: usize) -> Result<HashMap<u32, u8>> {
    let setup = conn.setup();
    let min_kc = setup.min_keycode;
    let max_kc = setup.max_keycode;
    let mapping = conn.get_keyboard_mapping(min_kc, max_kc - min_kc + 1)?.reply()?;
    let per = mapping.keysyms_per_keycode as usize;
    let mut map = HashMap::new();
    for (i, chunk) in mapping.keysyms.chunks(per).enumerate() {
        let kc = min_kc + i as u8;
        for &sym in chunk {
            if sym != 0 {
                map.entry(sym).or_insert(kc);
            }
        }
    }
    Ok(map)
}

/// Map a browser KeyboardEvent.code string to an X11 keysym value.
fn browser_code_to_keysym(code: &str) -> Option<u32> {
    Some(match code {
        // Letters — lowercase keysyms (layout-independent on standard QWERTY)
        "KeyA" => 0x61, "KeyB" => 0x62, "KeyC" => 0x63, "KeyD" => 0x64,
        "KeyE" => 0x65, "KeyF" => 0x66, "KeyG" => 0x67, "KeyH" => 0x68,
        "KeyI" => 0x69, "KeyJ" => 0x6a, "KeyK" => 0x6b, "KeyL" => 0x6c,
        "KeyM" => 0x6d, "KeyN" => 0x6e, "KeyO" => 0x6f, "KeyP" => 0x70,
        "KeyQ" => 0x71, "KeyR" => 0x72, "KeyS" => 0x73, "KeyT" => 0x74,
        "KeyU" => 0x75, "KeyV" => 0x76, "KeyW" => 0x77, "KeyX" => 0x78,
        "KeyY" => 0x79, "KeyZ" => 0x7a,
        // Digits
        "Digit0" => 0x30, "Digit1" => 0x31, "Digit2" => 0x32, "Digit3" => 0x33,
        "Digit4" => 0x34, "Digit5" => 0x35, "Digit6" => 0x36, "Digit7" => 0x37,
        "Digit8" => 0x38, "Digit9" => 0x39,
        // Whitespace / control
        "Space" => 0x0020, "Enter" => 0xff0d, "Backspace" => 0xff08,
        "Tab" => 0xff09, "Escape" => 0xff1b, "Delete" => 0xffff,
        // Arrows
        "ArrowUp" => 0xff52, "ArrowDown" => 0xff54,
        "ArrowLeft" => 0xff51, "ArrowRight" => 0xff53,
        // Modifiers
        "ShiftLeft" => 0xffe1,  "ShiftRight" => 0xffe2,
        "ControlLeft" => 0xffe3, "ControlRight" => 0xffe4,
        "AltLeft" => 0xffe9,    "AltRight" => 0xffea,
        // Function keys
        "F1" => 0xffbe, "F2" => 0xffbf, "F3" => 0xffc0, "F4" => 0xffc1,
        "F5" => 0xffc2, "F6" => 0xffc3, "F7" => 0xffc4, "F8" => 0xffc5,
        "F9" => 0xffc6, "F10" => 0xffc7, "F11" => 0xffc8, "F12" => 0xffc9,
        // Punctuation
        "Minus" => 0x002d,      "Equal" => 0x003d,
        "BracketLeft" => 0x005b, "BracketRight" => 0x005d,
        "Backslash" => 0x005c,  "Semicolon" => 0x003b,
        "Quote" => 0x0027,      "Comma" => 0x002c,
        "Period" => 0x002e,     "Slash" => 0x002f,
        "Backquote" => 0x0060,
        _ => return None,
    })
}
