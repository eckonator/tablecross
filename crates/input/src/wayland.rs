//! Linux with Wayland (GNOME 45+, KDE Plasma 6.1+): portals and libei.
//!
//! - Injecting: the RemoteDesktop portal hands out an EI connection; the permission is asked once and
//!   remembered with a restore token.
//! - Capturing: the InputCapture portal puts barriers on the outer screen edges. When the pointer
//!   hits one, the compositor stops local input and sends everything over EI until we release it.
//!   Outside of that, a Wayland session tells no program about input, so using this computer's own
//!   mouse while it is controlled cannot take control back.
//!
//! Portals are asynchronous: everything runs on a small Tokio runtime of its own; the traits talk
//! to it through a channel. Screen coordinates are the compositor's logical pixels.

use std::collections::HashSet;
use std::num::NonZeroU32;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ashpd::desktop::input_capture::{
    Activated, Barrier, Capabilities, ConnectToEISOptions as CaptureEisOptions,
    CreateSessionOptions as CaptureSessionOptions, DisableOptions, EnableOptions, GetZonesOptions, InputCapture,
    ReleaseOptions, SetPointerBarriersOptions,
};
use ashpd::desktop::remote_desktop::{
    ConnectToEISOptions, DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions,
};
use ashpd::desktop::{CreateSessionOptions, PersistMode, Session};
use futures_util::StreamExt;
use futures_util::stream::LocalBoxStream;
use reis::ei;
use reis::event::{Device, DeviceCapability, EiEvent};
use tc_core::{Captured, Point, Side};
use tc_protocol::{Message, MouseButton, PressedKeys, Screen};
use tokio::sync::mpsc;

use crate::keymap::linux as keymap;
use crate::{Backend, Capture, Handler, Health, Inject, InputError, Problem};

/// Linux button codes (input-event-codes.h).
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
const BTN_SIDE: u32 = 0x113;
const BTN_EXTRA: u32 = 0x114;
/// EI discrete scrolling counts 120 per wheel detent.
const DETENT: f32 = 120.0;

enum Cmd {
    StartCapture(Handler),
    Inject(Message),
    StopCapture,
    Sides(Vec<Side>),
    Grab,
    Ungrab(Point),
    ReleaseAll,
}

#[derive(Default)]
struct Shared {
    screens: Mutex<Vec<Screen>>,
    grabbed: AtomicBool,
    capturing: AtomicBool,
    problems: Mutex<Vec<Problem>>,
    /// Where a computer lies beyond: only these sides get barriers.
    sides: Mutex<Vec<Side>>,
    /// When the pointer was last moved for another computer.
    moved_for_peer: Mutex<Option<Instant>>,
}

pub fn backend() -> Result<Backend, InputError> {
    let (tx, rx) = mpsc::unbounded_channel();
    let shared = Arc::new(Shared::default());
    let task_shared = shared.clone();
    std::thread::Builder::new()
        .name("tablecross-wayland".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("no runtime for the portals: {e}");
                    return;
                }
            };
            runtime.block_on(run(rx, task_shared));
        })
        .map_err(|e| InputError::System(e.to_string()))?;
    Ok(Backend {
        capture: Box::new(WaylandCapture {
            tx: tx.clone(),
            shared: shared.clone(),
        }),
        inject: Arc::new(WaylandInject { tx, shared }),
    })
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}

/// Where the RemoteDesktop permission is remembered.
fn token_file() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("tablecross/remote-desktop-token"))
}

/// Injecting through the RemoteDesktop portal.
struct Sender {
    connection: reis::event::Connection,
    devices: Vec<Device>,
    resumed: HashSet<Device>,
    sequence: u32,
    keys: PressedKeys,
    buttons: Vec<MouseButton>,
    position: Option<(f32, f32)>,
    /// Top left corner of all screens (where a relative pointer is pushed to at first).
    origin: (f32, f32),
}

async fn open_sender() -> Result<(Sender, reis::tokio::EiConvertEventStream, Session<RemoteDesktop>), ashpd::Error> {
    let portal = RemoteDesktop::new().await?;
    let session = portal.create_session(CreateSessionOptions::default()).await?;
    let token = token_file().and_then(|f| std::fs::read_to_string(f).ok());
    portal
        .select_devices(
            &session,
            SelectDevicesOptions::default()
                .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(token.as_deref()),
        )
        .await?;
    let started = portal
        .start(&session, None, StartOptions::default())
        .await?
        .response()?;
    if let (Some(token), Some(file)) = (started.restore_token(), token_file()) {
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(file, token);
    }
    let fd = portal.connect_to_eis(&session, ConnectToEISOptions::default()).await?;
    let context = ei::Context::new(UnixStream::from(fd)).map_err(ashpd::Error::IO)?;
    let (connection, events) = context
        .handshake_tokio("tablecross", ei::handshake::ContextType::Sender)
        .await
        .map_err(|e| ashpd::Error::IO(std::io::Error::other(e.to_string())))?;
    Ok((
        Sender {
            connection,
            devices: Vec::new(),
            resumed: HashSet::new(),
            sequence: 0,
            keys: PressedKeys::new(),
            buttons: Vec::new(),
            position: None,
            origin: (0.0, 0.0),
        },
        events,
        session,
    ))
}

impl Sender {
    fn event(&mut self, event: EiEvent, shared: &Shared) {
        match event {
            EiEvent::SeatAdded(e) => {
                e.seat.bind_capabilities(
                    DeviceCapability::PointerAbsolute
                        | DeviceCapability::Pointer
                        | DeviceCapability::Keyboard
                        | DeviceCapability::Scroll
                        | DeviceCapability::Button,
                );
            }
            EiEvent::DeviceAdded(e) => {
                if e.device.has_capability(DeviceCapability::PointerAbsolute) {
                    let screens = e
                        .device
                        .regions()
                        .iter()
                        .map(|r| Screen {
                            x: r.x as i32,
                            y: r.y as i32,
                            width: r.width,
                            height: r.height,
                            scale: 1.0,
                        })
                        .collect();
                    *shared.screens.lock().expect("screens lock") = screens;
                }
                self.devices.push(e.device);
            }
            EiEvent::DeviceRemoved(e) => {
                self.devices.retain(|d| *d != e.device);
                self.resumed.remove(&e.device);
            }
            EiEvent::DeviceResumed(e) => {
                e.device.device().start_emulating(e.serial, self.sequence);
                self.sequence = self.sequence.wrapping_add(1);
                self.resumed.insert(e.device);
            }
            EiEvent::DevicePaused(e) => {
                self.resumed.remove(&e.device);
            }
            _ => {}
        }
        let _ = self.connection.flush();
    }

    fn device(&self, capability: DeviceCapability) -> Option<&Device> {
        self.devices
            .iter()
            .find(|d| self.resumed.contains(*d) && d.has_capability(capability))
    }

    fn frame(&self, device: &Device) {
        device.device().frame(self.connection.serial(), now_us());
        let _ = self.connection.flush();
    }

    fn inject(&mut self, message: &Message) {
        match *message {
            Message::Enter { x, y } | Message::MouseMove { x, y, .. } => {
                if let Some(device) = self.device(DeviceCapability::PointerAbsolute) {
                    if let Some(pointer) = device.interface::<ei::PointerAbsolute>() {
                        pointer.motion_absolute(x, y);
                        self.frame(device);
                    }
                } else if let Some(device) = self.device(DeviceCapability::Pointer) {
                    // Only relative motion offered: move by the difference. Where the pointer is
                    // is not known at first, so it goes into the top left corner and from there.
                    if let Some(pointer) = device.interface::<ei::Pointer>() {
                        let (px, py) = match self.position {
                            Some(p) if !matches!(message, Message::Enter { .. }) => p,
                            _ => {
                                pointer.motion_relative(-100_000.0, -100_000.0);
                                self.frame(device);
                                self.origin
                            }
                        };
                        pointer.motion_relative(x - px, y - py);
                        self.frame(device);
                    }
                }
                self.position = Some((x, y));
            }
            Message::Key { usage, down } => {
                let Some(code) = keymap::from_hid(usage) else { return };
                let changed = if down {
                    self.keys.press(usage)
                } else {
                    self.keys.release(usage)
                };
                if !changed {
                    return;
                }
                if let Some(device) = self.device(DeviceCapability::Keyboard)
                    && let Some(keyboard) = device.interface::<ei::Keyboard>()
                {
                    let state = if down {
                        ei::keyboard::KeyState::Press
                    } else {
                        ei::keyboard::KeyState::Released
                    };
                    keyboard.key(u32::from(code), state);
                    self.frame(device);
                }
            }
            Message::Media { key } => {
                if let Some(device) = self.device(DeviceCapability::Keyboard)
                    && let Some(keyboard) = device.interface::<ei::Keyboard>()
                {
                    let code = u32::from(keymap::media_code(key));
                    keyboard.key(code, ei::keyboard::KeyState::Press);
                    self.frame(device);
                    keyboard.key(code, ei::keyboard::KeyState::Released);
                    self.frame(device);
                }
            }
            Message::MouseButton { button, down } => {
                if down == self.buttons.contains(&button) {
                    return;
                }
                if down {
                    self.buttons.push(button);
                } else {
                    self.buttons.retain(|b| *b != button);
                }
                let code = match button {
                    MouseButton::Left => BTN_LEFT,
                    MouseButton::Right => BTN_RIGHT,
                    MouseButton::Middle => BTN_MIDDLE,
                    MouseButton::Back => BTN_SIDE,
                    MouseButton::Forward => BTN_EXTRA,
                };
                if let Some(device) = self.device(DeviceCapability::Button)
                    && let Some(buttons) = device.interface::<ei::Button>()
                {
                    let state = if down {
                        ei::button::ButtonState::Press
                    } else {
                        ei::button::ButtonState::Released
                    };
                    buttons.button(code, state);
                    self.frame(device);
                }
            }
            Message::Wheel { dx, dy, precise } => {
                if let Some(device) = self.device(DeviceCapability::Scroll)
                    && let Some(scroll) = device.interface::<ei::Scroll>()
                {
                    // EI counts down and right as positive
                    if precise {
                        scroll.scroll(dx, -dy);
                    } else {
                        scroll.scroll_discrete((dx * DETENT) as i32, (-dy * DETENT) as i32);
                    }
                    self.frame(device);
                }
            }
            _ => {}
        }
    }

    fn release_all(&mut self) {
        for usage in self.keys.clone().release_all() {
            self.inject(&Message::Key { usage, down: false });
        }
        for button in self.buttons.clone() {
            self.inject(&Message::MouseButton { button, down: false });
        }
    }
}

/// Capturing through the InputCapture portal.
struct Receiver {
    portal: InputCapture,
    session: Session<InputCapture>,
    /// Requests (like binding the seat) only leave when flushed.
    connection: reis::event::Connection,
    zones: Vec<(i32, i32, i32, i32)>,
    activation: Option<u32>,
    /// The pointer is captured (between an activation and its release).
    active: bool,
    park: (f64, f64),
    handler: Handler,
    /// Capturing is switched on (barriers may only be set while it is off: GNOME refuses them
    /// otherwise, and a second switching on as well).
    enabled: bool,
    /// Until then a "Disabled" signal is the echo of our own change (KWin sends one for every
    /// change of the barriers, and keeps them), not the system switching capturing off.
    own_change: Instant,
    /// When the system switched capturing off lately: if it keeps doing so, it is meant.
    switched_off: Vec<Instant>,
    sides: Vec<Side>,
    /// The barriers have to be set anew once the pointer is given back.
    refresh_later: bool,
    /// Since when input comes over EI that no computer takes (the activation was missed, or giving
    /// the pointer back failed): the compositor holds the pointer for nobody.
    unwanted_since: Option<Instant>,
}

/// How long after changing the barriers a "Disabled" signal is taken for its echo.
const OWN_CHANGE: Duration = Duration::from_secs(2);
/// After the system switched capturing off (a screen change on GNOME, which says nothing else),
/// the screens get this long to settle before the barriers are set anew.
const SETTLE_AFTER_DISABLED: Duration = Duration::from_secs(1);
/// Switched off this often within `SWITCHED_OFF_WINDOW`, capturing stays off.
const SWITCHED_OFF_LIMIT: usize = 5;
const SWITCHED_OFF_WINDOW: Duration = Duration::from_secs(60);
/// The compositor takes injected motion for a push against a barrier as well: an activation this
/// soon after moving the pointer for another computer came from there, not from the user.
const INJECTED_LATELY: Duration = Duration::from_millis(500);
/// Input over EI that no computer takes for this long: the pointer is given back.
const UNWANTED_FOR: Duration = Duration::from_millis(500);

/// Barriers on every zone edge on one of `sides` that has no other zone behind it.
fn barriers(zones: &[(i32, i32, i32, i32)], sides: &[Side]) -> Vec<Barrier> {
    let touches = |a: &(i32, i32, i32, i32), b: &(i32, i32, i32, i32), side: u8| {
        let (ax, ay, aw, ah) = *a;
        let (bx, by, bw, bh) = *b;
        let overlap_y = ay < by + bh && by < ay + ah;
        let overlap_x = ax < bx + bw && bx < ax + aw;
        match side {
            0 => overlap_y && bx + bw == ax,
            1 => overlap_y && bx == ax + aw,
            2 => overlap_x && by + bh == ay,
            _ => overlap_x && by == ay + ah,
        }
    };
    let mut out = Vec::new();
    for (i, zone) in zones.iter().enumerate() {
        let (x, y, w, h) = *zone;
        for side in 0..4u8 {
            let facing = [Side::Left, Side::Right, Side::Top, Side::Bottom][usize::from(side)];
            if !sides.contains(&facing) {
                continue;
            }
            if zones
                .iter()
                .enumerate()
                .any(|(j, other)| j != i && touches(zone, other, side))
            {
                continue;
            }
            let position = match side {
                0 => (x, y, x, y + h - 1),
                1 => (x + w, y, x + w, y + h - 1),
                2 => (x, y, x + w - 1, y),
                _ => (x, y + h, x + w - 1, y + h),
            };
            if let Some(id) = NonZeroU32::new(out.len() as u32 + 1) {
                out.push(Barrier::new(id, position));
            }
        }
    }
    out
}

/// Where the pointer hit a barrier, on the screens, and the push over that edge as a motion. KWin
/// may report a quick push far beyond the barrier: the nearest zone counts, and the side the
/// pointer was pushed out of.
fn pushed_out(zones: &[(i32, i32, i32, i32)], x: f64, y: f64) -> (f64, f64, f64, f64) {
    let distance = |&(zx, zy, zw, zh): &(i32, i32, i32, i32)| {
        let dx = (f64::from(zx) - x).max(x - f64::from(zx + zw - 1)).max(0.0);
        let dy = (f64::from(zy) - y).max(y - f64::from(zy + zh - 1)).max(0.0);
        dx.hypot(dy)
    };
    let zone = zones.iter().min_by(|a, b| distance(a).total_cmp(&distance(b))).copied();
    match zone {
        Some((zx, zy, zw, zh)) => {
            let (right_x, bottom_y) = (f64::from(zx + zw - 1), f64::from(zy + zh - 1));
            let (cx, cy) = (x.clamp(f64::from(zx), right_x), y.clamp(f64::from(zy), bottom_y));
            let (dx, dy) = if x < f64::from(zx) {
                (-2.0, 0.0)
            } else if x > right_x {
                (2.0, 0.0)
            } else if y < f64::from(zy) {
                (0.0, -2.0)
            } else if y > bottom_y {
                (0.0, 2.0)
            } else {
                let (left, right) = (cx - f64::from(zx), right_x - cx);
                let (top, bottom) = (cy - f64::from(zy), bottom_y - cy);
                let nearest = left.min(right).min(top).min(bottom);
                if nearest == left {
                    (-2.0, 0.0)
                } else if nearest == right {
                    (2.0, 0.0)
                } else if nearest == top {
                    (0.0, -2.0)
                } else {
                    (0.0, 2.0)
                }
            };
            (cx, cy, dx, dy)
        }
        None => (x, y, 0.0, 0.0),
    }
}

async fn open_receiver(
    handler: Handler,
    sides: Vec<Side>,
) -> Result<(Receiver, reis::tokio::EiConvertEventStream), ashpd::Error> {
    let portal = InputCapture::new().await?;
    let (session, _) = portal
        .create_session(
            None,
            CaptureSessionOptions::default().set_capabilities(Capabilities::Keyboard | Capabilities::Pointer),
        )
        .await?;
    let zones_reply = portal.zones(&session, GetZonesOptions::default()).await?.response()?;
    let zones: Vec<_> = zones_reply
        .regions()
        .iter()
        .map(|r| (r.x_offset(), r.y_offset(), r.width() as i32, r.height() as i32))
        .collect();
    let barriers = barriers(&zones, &sides);
    portal
        .set_pointer_barriers(
            &session,
            &barriers,
            zones_reply.zone_set(),
            SetPointerBarriersOptions::default(),
        )
        .await?
        .response()?;
    let fd = portal.connect_to_eis(&session, CaptureEisOptions::default()).await?;
    let context = ei::Context::new(UnixStream::from(fd)).map_err(ashpd::Error::IO)?;
    let (connection, events) = context
        .handshake_tokio("tablecross", ei::handshake::ContextType::Receiver)
        .await
        .map_err(|e| ashpd::Error::IO(std::io::Error::other(e.to_string())))?;
    // Without a barrier capturing stays off: nothing to hit, nothing held
    let enabled = !barriers.is_empty();
    if enabled {
        portal.enable(&session, EnableOptions::default()).await?;
    }
    tracing::info!("input capture ready, barriers on {sides:?} of {zones:?}");
    Ok((
        Receiver {
            portal,
            session,
            connection,
            zones,
            activation: None,
            active: false,
            park: (0.0, 0.0),
            handler,
            enabled,
            own_change: Instant::now() + OWN_CHANGE,
            switched_off: Vec::new(),
            sides,
            refresh_later: false,
            unwanted_since: None,
        },
        events,
    ))
}

impl Receiver {
    /// The pointer hit a barrier: tell the engine as a push over that edge.
    async fn activated(&mut self, activation: Option<u32>, at: (f32, f32), shared: &Shared) {
        self.activation = activation;
        self.active = true;
        let moved = *shared.moved_for_peer.lock().expect("moved lock");
        if moved.is_some_and(|t| t.elapsed() < INJECTED_LATELY) {
            tracing::debug!("the injected pointer hit an edge (activation {activation:?}), giving it back");
            self.release(None).await;
            return;
        }
        let (x, y) = (f64::from(at.0), f64::from(at.1));
        let (x, y, dx, dy) = pushed_out(&self.zones, x, y);
        self.park = (x, y);
        self.unwanted_since = None;
        let _ = (self.handler)(Captured::Motion { x, y, dx, dy });
        let grabbed = shared.grabbed.load(Ordering::SeqCst);
        tracing::info!(
            "the pointer hit an edge at ({x:.0}, {y:.0}), activation {activation:?}, {}",
            if grabbed { "it goes over" } else { "nothing behind it" }
        );
        if !grabbed {
            // No computer behind this edge: give the pointer back at once
            self.release(Some((x, y))).await;
        }
    }

    /// Gives the pointer back to the compositor. If it refuses twice, capturing is switched off,
    /// which ends the capture as well, and on again with fresh barriers.
    async fn release(&mut self, at: Option<(f64, f64)>) {
        let mut refused = None;
        for attempt in 0..2 {
            let options = ReleaseOptions::default()
                .set_activation_id(self.activation)
                .set_cursor_position(at);
            match self.portal.release(&self.session, options).await {
                Ok(_) => {
                    refused = None;
                    break;
                }
                Err(e) => {
                    tracing::warn!("cannot release the pointer (attempt {}): {e}", attempt + 1);
                    refused = Some(e);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
        self.activation = None;
        self.active = false;
        self.unwanted_since = None;
        if refused.is_some() {
            tracing::warn!("switching capturing off to free the pointer");
            self.refresh_later = false;
            self.refresh_zones().await;
        } else if std::mem::take(&mut self.refresh_later) {
            self.refresh_zones().await;
        }
    }

    /// Other computers came or went: barriers only where one is now. While the pointer is
    /// captured, once it is given back.
    async fn set_sides(&mut self, sides: Vec<Side>) {
        if sides == self.sides {
            return;
        }
        self.sides = sides;
        if self.active {
            self.refresh_later = true;
        } else {
            self.refresh_zones().await;
        }
    }

    /// Input came over EI. If no computer takes it for a while, the compositor holds the pointer
    /// for nobody: give it back.
    async fn check_wanted(&mut self, shared: &Shared) {
        if shared.grabbed.load(Ordering::SeqCst) {
            self.unwanted_since = None;
            return;
        }
        let now = Instant::now();
        let since = *self.unwanted_since.get_or_insert(now);
        if now.duration_since(since) >= UNWANTED_FOR {
            tracing::warn!(
                "the system holds the pointer, but no computer takes it (activation {:?}): giving it back",
                self.activation
            );
            let at = Some(self.park);
            self.release(at).await;
        }
    }

    /// Capturing ends: the pointer goes back, the barriers away.
    async fn close(mut self) {
        if self.active {
            let at = Some(self.park);
            self.release(at).await;
        }
        if self.enabled
            && let Err(e) = self.portal.disable(&self.session, DisableOptions::default()).await
        {
            tracing::warn!("cannot switch capturing off: {e}");
        }
        let _ = self.session.close().await;
        tracing::info!("input capture stopped");
    }

    /// The screens changed: barriers on the new outer edges. Capturing is switched off around it.
    async fn refresh_zones(&mut self) {
        self.own_change = Instant::now() + OWN_CHANGE;
        if self.enabled {
            if let Err(e) = self.portal.disable(&self.session, DisableOptions::default()).await {
                tracing::warn!("cannot switch capturing off to renew the screen edges: {e}");
            }
            self.enabled = false;
        }
        let result = async {
            let reply = self
                .portal
                .zones(&self.session, GetZonesOptions::default())
                .await?
                .response()?;
            let zones: Vec<_> = reply
                .regions()
                .iter()
                .map(|r| (r.x_offset(), r.y_offset(), r.width() as i32, r.height() as i32))
                .collect();
            let barriers = barriers(&zones, &self.sides);
            self.portal
                .set_pointer_barriers(
                    &self.session,
                    &barriers,
                    reply.zone_set(),
                    SetPointerBarriersOptions::default(),
                )
                .await?
                .response()?;
            self.zones = zones;
            if !barriers.is_empty() {
                self.portal.enable(&self.session, EnableOptions::default()).await?;
                self.enabled = true;
            }
            Ok::<(), ashpd::Error>(())
        }
        .await;
        match result {
            Ok(()) => tracing::info!(
                "screen edges for capturing renewed: barriers on {:?} of {:?}{}",
                self.sides,
                self.zones,
                if self.enabled { "" } else { ", capturing stays off" }
            ),
            Err(e) => tracing::warn!("cannot renew the screen edges for capturing: {e}"),
        }
    }

    /// The compositor switched capturing off: after a screen change (GNOME says nothing else), or
    /// its shortcut to cancel. Barriers on the screens as they are now, and on again.
    async fn disabled(&mut self) {
        let now = Instant::now();
        if now < self.own_change {
            return;
        }
        // Switching off ends a capture as well
        self.enabled = false;
        self.active = false;
        self.activation = None;
        self.switched_off
            .retain(|at| now.duration_since(*at) < SWITCHED_OFF_WINDOW);
        self.switched_off.push(now);
        if self.switched_off.len() > SWITCHED_OFF_LIMIT {
            tracing::warn!("capturing was switched off by the system again and again: it stays off");
            return;
        }
        tracing::info!("capturing was switched off by the system, switching it on again");
        tokio::time::sleep(SETTLE_AFTER_DISABLED).await;
        self.refresh_zones().await;
    }

    /// Hands input on to the handler; whether it was input (and not housekeeping of EI).
    fn event(&mut self, event: EiEvent) -> bool {
        let captured = match event {
            EiEvent::SeatAdded(e) => {
                e.seat.bind_capabilities(
                    DeviceCapability::Pointer
                        | DeviceCapability::Keyboard
                        | DeviceCapability::Scroll
                        | DeviceCapability::Button,
                );
                None
            }
            EiEvent::PointerMotion(e) => Some(Captured::Motion {
                x: self.park.0,
                y: self.park.1,
                dx: f64::from(e.dx),
                dy: f64::from(e.dy),
            }),
            EiEvent::Button(e) => {
                let button = match e.button {
                    BTN_LEFT => Some(MouseButton::Left),
                    BTN_RIGHT => Some(MouseButton::Right),
                    BTN_MIDDLE => Some(MouseButton::Middle),
                    BTN_SIDE => Some(MouseButton::Back),
                    BTN_EXTRA => Some(MouseButton::Forward),
                    _ => None,
                };
                button.map(|button| Captured::Button {
                    button,
                    down: e.state == ei::button::ButtonState::Press,
                })
            }
            EiEvent::ScrollDiscrete(e) => Some(Captured::Wheel {
                dx: f64::from(e.discrete_dx) / f64::from(DETENT),
                dy: -f64::from(e.discrete_dy) / f64::from(DETENT),
                precise: false,
            }),
            EiEvent::ScrollDelta(e) => Some(Captured::Wheel {
                dx: f64::from(e.dx),
                dy: -f64::from(e.dy),
                precise: true,
            }),
            EiEvent::KeyboardKey(e) => {
                let down = e.state == ei::keyboard::KeyState::Press;
                u16::try_from(e.key).ok().and_then(|code| match keymap::media(code) {
                    Some(key) => Some(Captured::Media { key, down }),
                    None => keymap::to_hid(code).map(|usage| Captured::Key { usage, down }),
                })
            }
            EiEvent::Disconnected(e) => {
                tracing::warn!("input capture connection ended: {:?}", e.reason);
                None
            }
            _ => None,
        };
        let _ = self.connection.flush();
        let input = captured.is_some();
        if let Some(captured) = captured {
            let _ = (self.handler)(captured);
        }
        input
    }
}

/// Signals of the input capture portal about the session: the screens changed (`true`) or the
/// compositor switched capturing off (`false`).
async fn session_signals() -> Option<LocalBoxStream<'static, bool>> {
    let proxy: &'static InputCapture = Box::leak(Box::new(InputCapture::new().await.ok()?));
    let zones = proxy.receive_zones_changed().await.ok()?.map(|_| true);
    let disabled = proxy.receive_disabled().await.ok()?.map(|_| false);
    Some(Box::pin(futures_util::stream::select(zones, disabled)))
}

/// The "Activated" signals of the input capture portal (the pointer hit a barrier). The stream
/// borrows its proxy, so the proxy lives for the rest of the program (one per capture start).
async fn activations() -> Option<LocalBoxStream<'static, Activated>> {
    let proxy: &'static InputCapture = Box::leak(Box::new(InputCapture::new().await.ok()?));
    let stream = proxy.receive_activated().await.ok()?;
    Some(Box::pin(stream))
}

async fn run(mut rx: mpsc::UnboundedReceiver<Cmd>, shared: Arc<Shared>) {
    let (mut sender, mut sender_events, _sender_session) = match open_sender().await {
        Ok((s, e, session)) => (Some(s), Some(e), Some(session)),
        Err(e) => {
            tracing::warn!("no remote control portal: {e}");
            shared
                .problems
                .lock()
                .expect("problems lock")
                .push(Problem::ControlUnavailable);
            (None, None, None)
        }
    };
    let mut receiver: Option<Receiver> = None;
    let mut receiver_events: Option<reis::tokio::EiConvertEventStream> = None;
    let mut activated: Option<LocalBoxStream<'static, Activated>> = None;
    let mut session_events: Option<LocalBoxStream<'static, bool>> = None;
    loop {
        tokio::select! {
            command = rx.recv() => {
                let Some(command) = command else { break };
                match command {
                    Cmd::StartCapture(handler) => {
                        if let Some(old) = receiver.take() {
                            old.close().await;
                        }
                        // Listening before capturing is switched on: an activation must not get lost
                        activated = activations().await;
                        session_events = session_signals().await;
                        let sides = shared.sides.lock().expect("sides lock").clone();
                        match open_receiver(handler, sides).await {
                            Ok((r, events)) => {
                                // Without an absolute pointer the screens come from the capture zones
                                {
                                    let mut screens = shared.screens.lock().expect("screens lock");
                                    if screens.is_empty() {
                                        *screens = r
                                            .zones
                                            .iter()
                                            .map(|&(x, y, w, h)| Screen {
                                                x,
                                                y,
                                                width: w.max(0) as u32,
                                                height: h.max(0) as u32,
                                                scale: 1.0,
                                            })
                                            .collect();
                                    }
                                }
                                receiver = Some(r);
                                receiver_events = Some(events);
                                shared.capturing.store(true, Ordering::SeqCst);
                            }
                            Err(e) => {
                                tracing::warn!("no input capture portal: {e}");
                                shared.problems.lock().expect("problems lock").push(Problem::CaptureStopped);
                            }
                        }
                    }
                    Cmd::StopCapture => {
                        activated = None;
                        session_events = None;
                        receiver_events = None;
                        if let Some(r) = receiver.take() {
                            r.close().await;
                        }
                        shared.capturing.store(false, Ordering::SeqCst);
                    }
                    Cmd::Sides(sides) => {
                        if let Some(r) = receiver.as_mut() {
                            r.set_sides(sides).await;
                        }
                    }
                    Cmd::Inject(message) => {
                        if let Some(sender) = sender.as_mut() {
                            let screens = shared.screens.lock().expect("screens lock");
                            sender.origin = screens.iter().fold((f32::MAX, f32::MAX), |(x, y), s| {
                                (x.min(s.x as f32), y.min(s.y as f32))
                            });
                            if screens.is_empty() {
                                sender.origin = (0.0, 0.0);
                            }
                            drop(screens);
                            sender.inject(&message);
                        }
                    }
                    Cmd::Grab => shared.grabbed.store(true, Ordering::SeqCst),
                    Cmd::Ungrab(at) => {
                        shared.grabbed.store(false, Ordering::SeqCst);
                        if let Some(r) = receiver.as_mut()
                            && r.active
                        {
                            r.release(Some((at.x, at.y))).await;
                        }
                    }
                    Cmd::ReleaseAll => {
                        if let Some(sender) = sender.as_mut() {
                            sender.release_all();
                        }
                    }
                }
            }
            Some(Ok(event)) = async { match sender_events.as_mut() { Some(s) => s.next().await, None => None } } => {
                if let Some(sender) = sender.as_mut() {
                    sender.event(event, &shared);
                }
            }
            Some(Ok(event)) = async { match receiver_events.as_mut() { Some(s) => s.next().await, None => None } } => {
                if let Some(r) = receiver.as_mut()
                    && r.event(event)
                {
                    r.check_wanted(&shared).await;
                }
            }
            Some(zones) = async { match session_events.as_mut() { Some(s) => s.next().await, None => None } } => {
                if let Some(r) = receiver.as_mut() {
                    if zones {
                        r.refresh_zones().await;
                    } else {
                        r.disabled().await;
                    }
                }
            }
            Some(signal) = async { match activated.as_mut() { Some(s) => s.next().await, None => None } } => {
                if let Some(r) = receiver.as_mut() {
                    let at = signal.cursor_position().unwrap_or((0.0, 0.0));
                    r.activated(signal.activation_id(), at, &shared).await;
                }
            }
        }
    }
}

pub struct WaylandCapture {
    tx: mpsc::UnboundedSender<Cmd>,
    shared: Arc<Shared>,
}

impl Capture for WaylandCapture {
    fn single_push_at_edges(&self) -> bool {
        true
    }

    fn start(&mut self, handler: Handler) -> Result<(), InputError> {
        self.tx
            .send(Cmd::StartCapture(handler))
            .map_err(|_| InputError::System("the portal thread stopped".into()))
    }

    fn stop(&mut self) {
        let _ = self.tx.send(Cmd::StopCapture);
    }

    fn open_sides(&self, sides: &[Side]) {
        *self.shared.sides.lock().expect("sides lock") = sides.to_vec();
        let _ = self.tx.send(Cmd::Sides(sides.to_vec()));
    }

    fn health(&self) -> Health {
        Health {
            capturing: self.shared.capturing.load(Ordering::SeqCst),
            problems: self.shared.problems.lock().expect("problems lock").clone(),
        }
    }
}

pub struct WaylandInject {
    tx: mpsc::UnboundedSender<Cmd>,
    shared: Arc<Shared>,
}

impl Inject for WaylandInject {
    fn grab(&self, _park: Point) -> Result<(), InputError> {
        // The compositor already holds the input since the barrier was hit
        self.shared.grabbed.store(true, Ordering::SeqCst);
        let _ = self.tx.send(Cmd::Grab);
        Ok(())
    }

    fn ungrab(&self, at: Point) -> Result<(), InputError> {
        self.shared.grabbed.store(false, Ordering::SeqCst);
        let _ = self.tx.send(Cmd::Ungrab(at));
        Ok(())
    }

    fn inject(&self, message: &Message) -> Result<(), InputError> {
        if matches!(message, Message::Enter { .. } | Message::MouseMove { .. }) {
            *self.shared.moved_for_peer.lock().expect("moved lock") = Some(Instant::now());
        }
        let _ = self.tx.send(Cmd::Inject(message.clone()));
        Ok(())
    }

    fn release_all(&self) {
        let _ = self.tx.send(Cmd::ReleaseAll);
    }

    fn screens(&self) -> Vec<Screen> {
        self.shared.screens.lock().expect("screens lock").clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_push_far_beyond_the_barrier_goes_over_the_nearest_edge() {
        let zones = [(0, 0, 1920, 1080), (1920, 0, 1280, 1024)];
        // On the barrier at the right of the second screen, and far beyond it
        assert_eq!(pushed_out(&zones, 3200.0, 500.0), (3199.0, 500.0, 2.0, 0.0));
        assert_eq!(pushed_out(&zones, 3290.0, 500.0), (3199.0, 500.0, 2.0, 0.0));
        // Below the first screen, a little beside it
        assert_eq!(pushed_out(&zones, 100.0, 1150.0), (100.0, 1079.0, 0.0, 2.0));
        // On the screen right at its left edge
        assert_eq!(pushed_out(&zones, 0.0, 300.0), (0.0, 300.0, -2.0, 0.0));
    }

    #[test]
    fn barriers_only_on_open_edges() {
        // Two screens side by side: the shared edge gets no barrier
        let zones = [(0, 0, 1920, 1080), (1920, 0, 1280, 1024)];
        let all = [Side::Left, Side::Right, Side::Top, Side::Bottom];
        let positions: Vec<_> = barriers(&zones, &all).iter().map(|b| b.position().as_tuple()).collect();
        assert_eq!(positions.len(), 6);
        assert!(positions.contains(&(0, 0, 0, 1079)));
        assert!(positions.contains(&(3200, 0, 3200, 1023)));
        assert!(!positions.contains(&(1920, 0, 1920, 1079)));
        assert!(positions.contains(&(0, 1080, 1919, 1080)));
    }

    #[test]
    fn barriers_only_where_a_computer_lies_beyond() {
        let zones = [(0, 0, 1440, 900)];
        assert!(barriers(&zones, &[]).is_empty());
        let positions: Vec<_> = barriers(&zones, &[Side::Right])
            .iter()
            .map(|b| b.position().as_tuple())
            .collect();
        assert_eq!(positions, vec![(1440, 0, 1440, 899)]);
    }
}
