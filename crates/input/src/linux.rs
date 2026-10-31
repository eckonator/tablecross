//! Linux with X11: capture with XInput2 raw events and grabs, inject with XTest (Wayland sessions: see `wayland.rs`).
//!
//! Raw events arrive no matter which window has the focus; while the pointer is on another computer
//! the keyboard and pointer are grabbed, so nothing reaches local programs. Movement is measured
//! from the pointer position: while grabbed the hidden pointer is parked in the middle of its screen
//! and put back after every move.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tc_core::{Captured, Point};
use tc_protocol::{Message, MouseButton, PressedKeys, Screen};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xinput::{self, ConnectionExt as _, XIEventMask};
use x11rb::protocol::xproto::{self, ConnectionExt as _, EventMask, GrabMode, GrabStatus, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use crate::keymap::linux as keymap;
use crate::{Backend, Capture, Handler, Health, Inject, InputError, Problem};

const ALL_MASTER_DEVICES: u16 = 1;
/// X11 key codes are evdev codes plus 8.
const KEYCODE_OFFSET: u32 = 8;

struct Shared {
    grabbed: AtomicBool,
    park_x: AtomicI32,
    park_y: AtomicI32,
    /// The pointer was put back at `park_x`/`park_y` on ungrab; motion is measured from there.
    returned: AtomicBool,
    running: AtomicBool,
    /// Injection: its own connection, so the capture thread can block on events.
    inject: Mutex<InjectState>,
}

struct InjectState {
    conn: RustConnection,
    root: Window,
    keys: PressedKeys,
    buttons: Vec<MouseButton>,
    wheel_rest: (f64, f64),
    cursor_hidden: bool,
    /// When the keyboard map of the XTest keyboard was last compared with the real one.
    keymap_checked: Option<Instant>,
}

fn connect() -> Result<(RustConnection, Window), InputError> {
    let (conn, screen) = x11rb::connect(None).map_err(|e| InputError::System(format!("no X11 display: {e}")))?;
    let root = conn.setup().roots[screen].root;
    Ok((conn, root))
}

pub fn backend() -> Result<Backend, InputError> {
    if std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland") {
        return crate::wayland::backend();
    }
    let (conn, root) = connect()?;
    conn.xtest_get_version(2, 2)
        .ok()
        .and_then(|c| c.reply().ok())
        .ok_or_else(|| InputError::System("the X server lacks the XTEST extension".into()))?;
    let _ = conn.xfixes_query_version(4, 0).ok().and_then(|c| c.reply().ok());
    let shared = Arc::new(Shared {
        grabbed: AtomicBool::new(false),
        park_x: AtomicI32::new(0),
        returned: AtomicBool::new(false),
        park_y: AtomicI32::new(0),
        running: AtomicBool::new(false),
        inject: Mutex::new(InjectState {
            conn,
            root,
            keys: PressedKeys::new(),
            buttons: Vec::new(),
            wheel_rest: (0.0, 0.0),
            cursor_hidden: false,
            keymap_checked: Some(Instant::now()),
        }),
    });
    xtest_keymap::follow_in_background();
    Ok(Backend {
        capture: Box::new(X11Capture { shared: shared.clone() }),
        inject: Arc::new(X11Inject { shared }),
    })
}

/// How often the keyboard map of the XTest keyboard is compared with the real one, at most.
const KEYMAP_CHECK: Duration = Duration::from_secs(10);

/// Injected keys take the keyboard map of the XTest keyboard. The layout chosen for the system
/// (Debian: `/etc/default/keyboard`, applied by udev to real keyboards only) does not reach it, so
/// it would type as US English. Its map is set to the one of the real keyboard with `xkbcomp`,
/// which comes with every X server.
mod xtest_keymap {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use x11rb::protocol::xinput::{ConnectionExt as _, DeviceType};

    /// The map last given to the XTest keyboard, and whether a thread is at it.
    static APPLIED: Mutex<Option<Vec<u8>>> = Mutex::new(None);
    static BUSY: AtomicBool = AtomicBool::new(false);

    pub fn follow_in_background() {
        if BUSY.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = std::thread::Builder::new().name("tablecross-keymap".into()).spawn(|| {
            if let Err(e) = follow() {
                tracing::warn!("injected keys may not follow the keyboard layout: {e}");
            }
            BUSY.store(false, Ordering::SeqCst);
        });
    }

    fn follow() -> Result<(), String> {
        let (conn, _) = super::connect().map_err(|e| e.to_string())?;
        let infos = conn
            .xinput_xi_query_device(0u16)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?
            .infos;
        let devices: Vec<_> = infos
            .iter()
            .map(|d| {
                (
                    d.deviceid,
                    d.type_,
                    d.attachment,
                    String::from_utf8_lossy(&d.name).into_owned(),
                )
            })
            .collect();
        let Some((from, to)) = pick(&devices) else {
            return Ok(());
        };
        let map = xkbcomp(&["-w", "0", "-xkb", "-i", &from.to_string(), &display(), "-"], None)?;
        let mut applied = APPLIED.lock().expect("keymap lock");
        if applied.as_ref() == Some(&map) {
            return Ok(());
        }
        xkbcomp(&["-w", "0", "-i", &to.to_string(), "-", &display()], Some(&map))?;
        tracing::info!("injected keys type like keyboard {from}: its map was given to the XTest keyboard {to}");
        *applied = Some(map);
        Ok(())
    }

    fn display() -> String {
        std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into())
    }

    fn xkbcomp(args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>, String> {
        let mut child = Command::new("xkbcomp")
            .args(args)
            .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("xkbcomp: {e}"))?;
        if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
            stdin.write_all(input).map_err(|e| format!("xkbcomp: {e}"))?;
        }
        let out = child.wait_with_output().map_err(|e| format!("xkbcomp: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "xkbcomp failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(out.stdout)
    }

    /// The real keyboard whose map to take and the XTest keyboard of the same master keyboard:
    /// from `(id, type, master, name)`. One called a keyboard beats buttons and hotkeys.
    pub(super) fn pick(devices: &[(u16, DeviceType, u16, String)]) -> Option<(u16, u16)> {
        let slaves = || devices.iter().filter(|d| d.1 == DeviceType::SLAVE_KEYBOARD);
        let xtest = |d: &&(u16, DeviceType, u16, String)| d.3.contains("XTEST");
        let to = slaves().find(xtest)?;
        let real: Vec<_> = slaves().filter(|d| !xtest(d) && d.2 == to.2).collect();
        let from = real
            .iter()
            .find(|d| d.3.to_lowercase().contains("keyboard"))
            .or(real.first())?;
        Some((from.0, to.0))
    }
}

pub struct X11Capture {
    shared: Arc<Shared>,
}

/// Devices XTest uses for injected input; their events are ours and are ignored.
fn xtest_devices(conn: &RustConnection) -> Vec<u16> {
    conn.xinput_xi_query_device(0u16)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| {
            r.infos
                .iter()
                .filter(|d| String::from_utf8_lossy(&d.name).contains("XTEST"))
                .map(|d| d.deviceid)
                .collect()
        })
        .unwrap_or_default()
}

fn button_of(detail: u32) -> Option<MouseButton> {
    Some(match detail {
        1 => MouseButton::Left,
        2 => MouseButton::Middle,
        3 => MouseButton::Right,
        8 => MouseButton::Back,
        9 => MouseButton::Forward,
        _ => return None,
    })
}

/// Wheel "buttons": 4 up, 5 down, 6 left, 7 right.
fn wheel_of(detail: u32) -> Option<(f64, f64)> {
    Some(match detail {
        4 => (0.0, 1.0),
        5 => (0.0, -1.0),
        6 => (-1.0, 0.0),
        7 => (1.0, 0.0),
        _ => return None,
    })
}

impl Capture for X11Capture {
    fn start(&mut self, mut handler: Handler) -> Result<(), InputError> {
        let (conn, root) = connect()?;
        conn.xinput_xi_query_version(2, 2)
            .ok()
            .and_then(|c| c.reply().ok())
            .ok_or_else(|| InputError::System("the X server lacks XInput 2".into()))?;
        let mask = XIEventMask::RAW_KEY_PRESS
            | XIEventMask::RAW_KEY_RELEASE
            | XIEventMask::RAW_BUTTON_PRESS
            | XIEventMask::RAW_BUTTON_RELEASE
            | XIEventMask::RAW_MOTION;
        xinput::xi_select_events(
            &conn,
            root,
            &[xinput::EventMask {
                deviceid: ALL_MASTER_DEVICES,
                mask: vec![mask],
            }],
        )
        .map_err(|e| InputError::System(e.to_string()))?;
        conn.flush().map_err(|e| InputError::System(e.to_string()))?;
        let ignored = xtest_devices(&conn);
        let shared = self.shared.clone();
        shared.running.store(true, Ordering::SeqCst);
        std::thread::Builder::new()
            .name("tablecross-capture".into())
            .spawn(move || {
                let mut last = conn
                    .query_pointer(root)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .map_or((0, 0), |p| (i32::from(p.root_x), i32::from(p.root_y)));
                while shared.running.load(Ordering::SeqCst) {
                    let event = match conn.wait_for_event() {
                        Ok(event) => event,
                        Err(e) => {
                            tracing::warn!("X11 input capture stopped: {e}");
                            shared.running.store(false, Ordering::SeqCst);
                            break;
                        }
                    };
                    let captured = match event {
                        Event::XinputRawMotion(e) if !ignored.contains(&e.sourceid) => {
                            let Some(p) = conn.query_pointer(root).ok().and_then(|c| c.reply().ok()) else {
                                continue;
                            };
                            let pos = (i32::from(p.root_x), i32::from(p.root_y));
                            if shared.grabbed.load(Ordering::SeqCst) {
                                let park = (
                                    shared.park_x.load(Ordering::SeqCst),
                                    shared.park_y.load(Ordering::SeqCst),
                                );
                                let delta = (pos.0 - park.0, pos.1 - park.1);
                                if delta == (0, 0) {
                                    continue;
                                }
                                let _ = conn.warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, park.0 as i16, park.1 as i16);
                                let _ = conn.flush();
                                last = park;
                                Some(Captured::Motion {
                                    x: f64::from(park.0),
                                    y: f64::from(park.1),
                                    dx: f64::from(delta.0),
                                    dy: f64::from(delta.1),
                                })
                            } else {
                                if shared.returned.swap(false, Ordering::SeqCst) {
                                    // Measured from the parking spot, the first motion would look like
                                    // a jump across the screen and switch straight back
                                    last = (
                                        shared.park_x.load(Ordering::SeqCst),
                                        shared.park_y.load(Ordering::SeqCst),
                                    );
                                }
                                let delta = (pos.0 - last.0, pos.1 - last.1);
                                last = pos;
                                // At the edge the pointer does not move, but the push still counts
                                let (dx, dy) = if delta == (0, 0) {
                                    raw_delta(&e.valuator_mask, &e.axisvalues)
                                } else {
                                    delta_f(delta)
                                };
                                Some(Captured::Motion {
                                    x: f64::from(pos.0),
                                    y: f64::from(pos.1),
                                    dx,
                                    dy,
                                })
                            }
                        }
                        Event::XinputRawKeyPress(e) if !ignored.contains(&e.sourceid) => key(e.detail, true),
                        Event::XinputRawKeyRelease(e) if !ignored.contains(&e.sourceid) => key(e.detail, false),
                        Event::XinputRawButtonPress(e) if !ignored.contains(&e.sourceid) => button(e.detail, true),
                        Event::XinputRawButtonRelease(e) if !ignored.contains(&e.sourceid) => button(e.detail, false),
                        _ => None,
                    };
                    if let Some(captured) = captured {
                        // Suppression happens through the grab; the answer is not needed here
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(captured)));
                    }
                }
            })
            .map_err(|e| InputError::System(e.to_string()))?;
        Ok(())
    }

    fn stop(&mut self) {
        self.shared.running.store(false, Ordering::SeqCst);
    }

    fn health(&self) -> Health {
        let capturing = self.shared.running.load(Ordering::SeqCst);
        Health {
            capturing,
            problems: if capturing {
                Vec::new()
            } else {
                vec![Problem::CaptureStopped]
            },
        }
    }
}

fn delta_f(d: (i32, i32)) -> (f64, f64) {
    (f64::from(d.0), f64::from(d.1))
}

/// Relative axes 0 and 1 of a raw motion (used only to notice a push against the screen edge). The
/// values come only for the axes set in the mask, in order, so a purely vertical push has a single
/// value, and it belongs to axis 1.
fn raw_delta(mask: &[u32], values: &[xinput::Fp3232]) -> (f64, f64) {
    let value = |f: &xinput::Fp3232| f64::from(f.integral) + f64::from(f.frac) / 4_294_967_296.0;
    let mut out = (0.0, 0.0);
    let mut next = values.iter();
    for axis in 0..mask.len() * 32 {
        if mask[axis / 32] & (1 << (axis % 32)) == 0 {
            continue;
        }
        let Some(v) = next.next() else { break };
        match axis {
            0 => out.0 = value(v),
            1 => out.1 = value(v),
            _ => {}
        }
    }
    out
}

fn key(detail: u32, down: bool) -> Option<Captured> {
    let code = u16::try_from(detail.checked_sub(KEYCODE_OFFSET)?).ok()?;
    if let Some(key) = keymap::media(code) {
        return Some(Captured::Media { key, down });
    }
    Some(Captured::Key {
        usage: keymap::to_hid(code)?,
        down,
    })
}

fn button(detail: u32, down: bool) -> Option<Captured> {
    if let Some(button) = button_of(detail) {
        return Some(Captured::Button { button, down });
    }
    // A wheel step is a press and a release; count the press
    let (dx, dy) = wheel_of(detail)?;
    down.then_some(Captured::Wheel { dx, dy, precise: false })
}

pub struct X11Inject {
    shared: Arc<Shared>,
}

impl InjectState {
    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) {
        let _ = self
            .conn
            .xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, self.root, x, y, 0);
    }

    fn flush(&self) {
        let _ = self.conn.flush();
        // The grabs send their events to this connection; nobody needs them, but unread they would
        // pile up in memory
        while let Ok(Some(_)) = self.conn.poll_for_event() {}
    }

    fn key(&mut self, usage: u16, down: bool) {
        let Some(code) = keymap::from_hid(usage) else { return };
        let changed = if down {
            self.keys.press(usage)
        } else {
            self.keys.release(usage)
        };
        if !changed {
            return;
        }
        if down && self.keymap_checked.is_none_or(|t| t.elapsed() > KEYMAP_CHECK) {
            // Someone starts typing: the layout may have changed since
            self.keymap_checked = Some(Instant::now());
            xtest_keymap::follow_in_background();
        }
        let kind = if down {
            xproto::KEY_PRESS_EVENT
        } else {
            xproto::KEY_RELEASE_EVENT
        };
        self.fake(kind, (u32::from(code) + KEYCODE_OFFSET) as u8, 0, 0);
        self.flush();
    }

    fn button(&mut self, button: MouseButton, down: bool) {
        if down == self.buttons.contains(&button) {
            return;
        }
        if down {
            self.buttons.push(button);
        } else {
            self.buttons.retain(|b| *b != button);
        }
        let detail = match button {
            MouseButton::Left => 1,
            MouseButton::Middle => 2,
            MouseButton::Right => 3,
            MouseButton::Back => 8,
            MouseButton::Forward => 9,
        };
        let kind = if down {
            xproto::BUTTON_PRESS_EVENT
        } else {
            xproto::BUTTON_RELEASE_EVENT
        };
        self.fake(kind, detail, 0, 0);
        self.flush();
    }

    fn wheel(&mut self, dx: f64, dy: f64, precise: bool) {
        // Precise scrolling in points: about 40 points per wheel step
        let factor = if precise { 1.0 / 40.0 } else { 1.0 };
        let (rx, ry) = self.wheel_rest;
        let (tx, ty) = (rx + dx * factor, ry + dy * factor);
        let (wx, wy) = (tx.trunc(), ty.trunc());
        self.wheel_rest = (tx - wx, ty - wy);
        let steps = |n: f64, positive: u8, negative: u8| {
            let detail = if n > 0.0 { positive } else { negative };
            (0..n.abs() as u32).map(move |_| detail)
        };
        for detail in steps(wy, 4, 5).chain(steps(wx, 7, 6)) {
            self.fake(xproto::BUTTON_PRESS_EVENT, detail, 0, 0);
            self.fake(xproto::BUTTON_RELEASE_EVENT, detail, 0, 0);
        }
        self.flush();
    }

    fn move_to(&self, x: f64, y: f64) {
        self.fake(xproto::MOTION_NOTIFY_EVENT, 0, x.round() as i16, y.round() as i16);
        self.flush();
    }
}

impl Inject for X11Inject {
    fn grab(&self, park: Point) -> Result<(), InputError> {
        let screens = self.screens();
        let center = screens
            .iter()
            .find(|s| {
                park.x >= f64::from(s.x)
                    && park.x < f64::from(s.x) + f64::from(s.width)
                    && park.y >= f64::from(s.y)
                    && park.y < f64::from(s.y) + f64::from(s.height)
            })
            .or(screens.first())
            .map_or((park.x as i32, park.y as i32), |s| {
                (s.x + s.width as i32 / 2, s.y + s.height as i32 / 2)
            });
        self.shared.park_x.store(center.0, Ordering::SeqCst);
        self.shared.park_y.store(center.1, Ordering::SeqCst);
        let mut state = self.shared.inject.lock().expect("inject lock");
        let root = state.root;
        let conn = &state.conn;
        let pointer = conn
            .grab_pointer(
                false,
                root,
                EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                x11rb::NONE,
                x11rb::NONE,
                x11rb::CURRENT_TIME,
            )
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|r| r.status == GrabStatus::SUCCESS);
        let keyboard = conn
            .grab_keyboard(false, root, x11rb::CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|r| r.status == GrabStatus::SUCCESS);
        if !pointer || !keyboard {
            // Another program holds a grab (a menu is open, a game): stay local
            let _ = conn.ungrab_pointer(x11rb::CURRENT_TIME);
            let _ = conn.ungrab_keyboard(x11rb::CURRENT_TIME);
            let _ = conn.flush();
            return Err(InputError::System("another program holds mouse or keyboard".into()));
        }
        let _ = conn.warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, center.0 as i16, center.1 as i16);
        if !state.cursor_hidden {
            let _ = conn.xfixes_hide_cursor(root);
            state.cursor_hidden = true;
        }
        self.shared.grabbed.store(true, Ordering::SeqCst);
        state.flush();
        Ok(())
    }

    fn ungrab(&self, at: Point) -> Result<(), InputError> {
        self.shared.grabbed.store(false, Ordering::SeqCst);
        let mut state = self.shared.inject.lock().expect("inject lock");
        let root = state.root;
        let _ = state.conn.ungrab_pointer(x11rb::CURRENT_TIME);
        let _ = state.conn.ungrab_keyboard(x11rb::CURRENT_TIME);
        let _ = state
            .conn
            .warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, at.x.round() as i16, at.y.round() as i16);
        if state.cursor_hidden {
            let _ = state.conn.xfixes_show_cursor(root);
            state.cursor_hidden = false;
        }
        state.flush();
        self.shared.park_x.store(at.x.round() as i32, Ordering::SeqCst);
        self.shared.park_y.store(at.y.round() as i32, Ordering::SeqCst);
        self.shared.returned.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn inject(&self, message: &Message) -> Result<(), InputError> {
        let mut state = self.shared.inject.lock().expect("inject lock");
        match *message {
            Message::Enter { x, y } | Message::MouseMove { x, y, .. } => state.move_to(f64::from(x), f64::from(y)),
            Message::MouseButton { button, down } => state.button(button, down),
            Message::Key { usage, down } => state.key(usage, down),
            Message::Media { key } => {
                let code = (u32::from(keymap::media_code(key)) + KEYCODE_OFFSET) as u8;
                state.fake(xproto::KEY_PRESS_EVENT, code, 0, 0);
                state.fake(xproto::KEY_RELEASE_EVENT, code, 0, 0);
                state.flush();
            }
            Message::Wheel { dx, dy, precise } => state.wheel(f64::from(dx), f64::from(dy), precise),
            _ => {}
        }
        Ok(())
    }

    fn release_all(&self) {
        let mut state = self.shared.inject.lock().expect("inject lock");
        for usage in state.keys.clone().release_all() {
            state.key(usage, false);
        }
        for button in state.buttons.clone() {
            state.button(button, false);
        }
        if state.cursor_hidden {
            let root = state.root;
            let _ = state.conn.xfixes_show_cursor(root);
            state.cursor_hidden = false;
            state.flush();
        }
    }

    fn screens(&self) -> Vec<Screen> {
        let state = self.shared.inject.lock().expect("inject lock");
        state.flush();
        let monitors = state
            .conn
            .randr_get_monitors(state.root, true)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.monitors)
            .unwrap_or_default();
        if monitors.is_empty() {
            let screen = &state.conn.setup().roots[0];
            return vec![Screen {
                x: 0,
                y: 0,
                width: u32::from(screen.width_in_pixels),
                height: u32::from(screen.height_in_pixels),
                scale: 1.0,
            }];
        }
        monitors
            .iter()
            .map(|m| Screen {
                x: i32::from(m.x),
                y: i32::from(m.y),
                width: u32::from(m.width),
                height: u32::from(m.height),
                scale: 1.0,
            })
            .collect()
    }

    fn sizes(&self) -> Vec<(u32, u32)> {
        let state = self.shared.inject.lock().expect("inject lock");
        state
            .conn
            .randr_get_monitors(state.root, true)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.monitors)
            .unwrap_or_default()
            .iter()
            .map(|m| (m.width_in_millimeters, m.height_in_millimeters))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_buttons() {
        // X key code 38 is evdev 30, the A key
        assert_eq!(
            key(38, true),
            Some(Captured::Key {
                usage: 0x04,
                down: true
            })
        );
        assert_eq!(key(3, true), None);
        assert_eq!(
            button(3, true),
            Some(Captured::Button {
                button: MouseButton::Right,
                down: true
            })
        );
        assert_eq!(
            button(5, true),
            Some(Captured::Wheel {
                dx: 0.0,
                dy: -1.0,
                precise: false
            })
        );
        assert_eq!(button(5, false), None);
    }
}

#[cfg(test)]
mod raw_tests {
    use super::*;

    fn fp(v: i32) -> xinput::Fp3232 {
        xinput::Fp3232 { integral: v, frac: 0 }
    }

    #[test]
    fn the_xtest_keyboard_takes_the_map_of_a_real_keyboard() {
        use xinput::DeviceType as T;
        let d = |id, t, master, name: &str| (id, t, master, name.to_string());
        let devices = [
            d(3, T::MASTER_KEYBOARD, 2, "Virtual core keyboard"),
            d(5, T::SLAVE_KEYBOARD, 3, "Virtual core XTEST keyboard"),
            d(6, T::SLAVE_KEYBOARD, 3, "Power Button"),
            d(9, T::SLAVE_KEYBOARD, 3, "AT Translated Set 2 keyboard"),
            d(4, T::SLAVE_POINTER, 2, "Virtual core XTEST pointer"),
        ];
        assert_eq!(xtest_keymap::pick(&devices), Some((9, 5)));
        assert_eq!(xtest_keymap::pick(&devices[..3]), Some((6, 5)));
        assert_eq!(xtest_keymap::pick(&devices[..2]), None);
    }

    #[test]
    fn raw_axes_follow_the_mask() {
        // Only axis 1 (vertical) set: its value comes first
        assert_eq!(raw_delta(&[0b10], &[fp(-3)]), (0.0, -3.0));
        assert_eq!(raw_delta(&[0b11], &[fp(2), fp(-3)]), (2.0, -3.0));
        assert_eq!(raw_delta(&[0b01], &[fp(4)]), (4.0, 0.0));
        // Other axes (a wheel) in between do not count
        assert_eq!(raw_delta(&[0b101], &[fp(1), fp(9)]), (1.0, 0.0));
    }
}
