//! The leader/target state machine.
//!
//! Exactly one machine is in control at a time. The machine whose own mouse and keyboard are used
//! is the *leader*: as long as its pointer is on its own screens, nothing special happens. When the
//! pointer crosses an edge towards a neighbor, the leader grabs its input (suppresses it locally) and
//! tracks the pointer on the neighbor (the *target*), sending it absolute positions, keys, buttons and
//! wheel. The target only injects what it gets. If someone uses the target's own mouse or keyboard,
//! the target takes control back.
//!
//! Stuck keys are prevented by routing: every pressed key and button remembers where its "down" went
//! (the local system and/or a remote machine), and its "up" goes exactly there, wherever the pointer
//! is by then. Modifiers held while crossing are pressed on the new side as well, so Shift-click and
//! similar work across the edge; a mouse button held while crossing is released (a drag ends at the
//! edge). Whenever a link ends, everything the remote side got is released.
//!
//! The engine does no I/O: it takes events and returns [`Action`]s, so all of this is tested without
//! an operating system.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tc_protocol::{MediaKey, Message, MouseButton, PressedKeys, Screen};

use crate::geometry::{Point, Screens, Side};
use crate::layout::Layout;

pub type MachineId = String;

/// How long the pointer must be pushed against a top or bottom edge before it moves over, when that
/// is switched on (`set_edge_dwell`): the Dock and the menu bar sit at these edges. By default all
/// edges switch at once.
pub const EDGE_DWELL: Duration = Duration::from_millis(250);
/// A pause in pushing longer than this starts the dwell anew. Long enough for lifting a finger
/// between strokes on a trackpad, which never pushes a quarter second in one stroke.
const EDGE_PAUSE: Duration = Duration::from_millis(400);
/// Moving farther from the edge than this (logical points) ends a push.
const EDGE_LEAVE: f64 = 24.0;
/// One event counts as pushing for at most this long (the pause before it is no pushing).
const EDGE_STEP: Duration = Duration::from_millis(50);

/// Where the time comes from (tests use their own).
#[derive(Clone)]
pub struct Clock(Arc<dyn Fn() -> Instant + Send + Sync>);

impl std::fmt::Debug for Clock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Clock")
    }
}

/// How top and bottom edges switch (left and right always switch at once).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EdgeRule {
    #[default]
    AtOnce,
    /// After pushing against the edge for a moment (`EDGE_DWELL`).
    Push,
    /// After touching the edge twice in a short time.
    DoubleTap,
}

/// The second touch of a double tap must come within this time of the first.
const TAP_WINDOW: Duration = Duration::from_millis(500);
/// Moving this far from the edge (logical points) between two touches counts as letting go.
const TAP_AWAY: f64 = 2.0;

/// The pointer touched an edge once (for a double tap).
#[derive(Debug, Clone)]
struct EdgeTap {
    machine: MachineId,
    side: Side,
    /// Last time it was at the edge.
    at: Instant,
    /// It left the edge since.
    away: bool,
}

/// The pointer is pushed against an edge that leads to a neighbor.
#[derive(Debug, Clone)]
struct EdgePush {
    machine: MachineId,
    side: Side,
    /// Time spent pushing so far, pauses left out.
    pushed: Duration,
    last: Instant,
}

/// Logical points the local mouse must move between two ticks (about 250 ms) while controlled by
/// a remote machine before this machine takes control back. Counting per tick keeps a resting
/// mouse that jitters now and then from taking over. Keys, buttons and wheel never take over: they
/// simply work here as well (the mouse of one computer and the keyboard of another is a common
/// setup).
const TAKEOVER_MOTION: f64 = 8.0;

/// Input from this machine's own devices.
#[derive(Debug, Clone, PartialEq)]
pub enum Captured {
    /// Pointer at `x`/`y` (meaningless while grabbed), moved by `dx`/`dy`; native units.
    Motion {
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
    },
    Button {
        button: MouseButton,
        down: bool,
    },
    /// USB HID usage (page 7).
    Key {
        usage: u16,
        down: bool,
    },
    /// Volume, playback, brightness; only presses (and auto-repeats) go anywhere.
    Media {
        key: MediaKey,
        down: bool,
    },
    /// Same meaning as `Message::Wheel`.
    Wheel {
        dx: f64,
        dy: f64,
        precise: bool,
    },
}

/// What the host has to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Suppress all local input from now on, hide the pointer and keep it at `park`; report motion as
    /// deltas.
    Grab {
        park: Point,
    },
    /// Stop suppressing; show the pointer at `at`.
    Ungrab {
        at: Point,
    },
    Send {
        to: MachineId,
        message: Message,
    },
    /// Inject into the local system: `Enter` (show the pointer there), `MouseMove`, `Key`,
    /// `MouseButton`, `Wheel`.
    Inject(Message),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Dest {
    /// The local system got the "down" (natively, or injected).
    Local,
    Remote(MachineId),
}

#[derive(Debug, Clone, PartialEq)]
enum Mode {
    /// Local input drives the local pointer.
    Local,
    /// Local input drives the pointer on `target`, currently at `pos` (target's native units).
    Remote {
        target: MachineId,
        pos: Point,
        /// Where the local pointer left; it comes back there if the link ends.
        exit: Point,
        /// Native units per logical point of the local screen the pointer left from.
        scale: f64,
    },
    /// A remote machine drives this one.
    Controlled { by: MachineId, last_seq: Option<u32> },
}

/// What the engine is doing, for status displays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    Idle,
    Controlling(MachineId),
    ControlledBy(MachineId),
}

#[derive(Debug)]
pub struct Engine {
    me: MachineId,
    layout: Layout,
    /// Screens of this machine and of every connected peer.
    machines: HashMap<MachineId, Screens>,
    mode: Mode,
    held_keys: BTreeMap<u16, BTreeSet<Dest>>,
    held_buttons: BTreeMap<MouseButton, BTreeSet<Dest>>,
    injected_keys: PressedKeys,
    injected_buttons: BTreeSet<MouseButton>,
    seq: u32,
    resend: bool,
    takeover_motion: f64,
    /// Last known position of the local pointer (from motion while not grabbed).
    local_pointer: Point,
    clock: Clock,
    edge_dwell: Duration,
    /// The system reports only one push at an edge (Wayland): no dwell possible.
    single_push: bool,
    double_tap: bool,
    edge_push: Option<EdgePush>,
    edge_tap: Option<EdgeTap>,
    /// The pointer stays on the computer it is on: no edge switches.
    pinned: bool,
    /// No switching this close to a corner (logical points); `None`: as in the layout.
    corner: Option<f64>,
    /// Past computers that are not connected to the next one that is.
    skip_offline: bool,
    /// A corner kept the pointer from switching and nobody has asked about it yet.
    corner_block: Option<CornerBlock>,
    /// When a corner block was last reported, per machine and side (reported again only after a pause).
    corner_reported: Option<(MachineId, Side, Instant)>,
}

/// The pointer was pushed over an edge with a neighbor, but too close to a corner to switch.
#[derive(Debug, Clone, PartialEq)]
pub struct CornerBlock {
    pub machine: MachineId,
    pub side: Side,
    pub at: Point,
}

/// A corner block is reported once, and again only when it happens after this long.
const CORNER_REPORT_PAUSE: Duration = Duration::from_secs(5);

pub fn is_modifier(usage: u16) -> bool {
    (0xE0..=0xE7).contains(&usage)
}

/// Swaps Ctrl and ⌘/Win on both sides.
fn swap_cmd_ctrl(usage: u16) -> u16 {
    match usage {
        0xE0 => 0xE3,
        0xE3 => 0xE0,
        0xE4 => 0xE7,
        0xE7 => 0xE4,
        other => other,
    }
}

/// `seq` is newer than `last`, with wrap-around.
fn newer(seq: u32, last: u32) -> bool {
    (seq.wrapping_sub(last) as i32) > 0
}

impl Engine {
    pub fn new(me: impl Into<MachineId>, screens: &[Screen], layout: Layout) -> Self {
        let me = me.into();
        let mut machines = HashMap::new();
        if let Some(s) = Screens::new(screens) {
            machines.insert(me.clone(), s);
        }
        Self {
            me,
            layout,
            machines,
            mode: Mode::Local,
            held_keys: BTreeMap::new(),
            held_buttons: BTreeMap::new(),
            injected_keys: PressedKeys::new(),
            injected_buttons: BTreeSet::new(),
            seq: 0,
            resend: false,
            takeover_motion: 0.0,
            local_pointer: Point::default(),
            clock: Clock(Arc::new(Instant::now)),
            edge_dwell: Duration::ZERO,
            single_push: false,
            double_tap: false,
            edge_tap: None,
            pinned: false,
            corner: None,
            skip_offline: true,
            edge_push: None,
            corner_block: None,
            corner_reported: None,
        }
    }

    /// The last time a corner kept the pointer from switching (for the log).
    pub fn take_corner_block(&mut self) -> Option<CornerBlock> {
        self.corner_block.take()
    }

    pub fn set_clock(&mut self, clock: impl Fn() -> Instant + Send + Sync + 'static) {
        self.clock = Clock(Arc::new(clock));
    }

    /// Zero switches at every edge at once (for systems that report only one push, like Wayland).
    pub fn set_edge_dwell(&mut self, dwell: Duration) {
        self.edge_dwell = if self.single_push { Duration::ZERO } else { dwell };
    }

    /// For systems that report only one push at an edge (Wayland): every edge switches at once.
    pub fn set_single_push(&mut self) {
        self.single_push = true;
        self.edge_dwell = Duration::ZERO;
        self.double_tap = false;
    }

    /// How top and bottom edges switch.
    pub fn set_edge_rule(&mut self, rule: EdgeRule) {
        self.set_edge_dwell(if rule == EdgeRule::Push {
            EDGE_DWELL
        } else {
            Duration::ZERO
        });
        self.double_tap = rule == EdgeRule::DoubleTap && !self.single_push;
        self.edge_push = None;
        self.edge_tap = None;
    }

    /// Keeps the pointer on the computer it is on (edges do not switch; shortcuts still do).
    pub fn set_pinned(&mut self, pinned: bool) {
        self.pinned = pinned;
    }

    pub fn pinned(&self) -> bool {
        self.pinned
    }

    /// No switching at left and right edges this close to a corner (logical points).
    pub fn set_corner(&mut self, corner: f64) {
        self.corner = (corner.is_finite() && corner >= 0.0).then_some(corner);
    }

    /// Whether the pointer goes past computers that are not connected to the next one that is.
    pub fn set_skip_offline(&mut self, skip: bool) {
        self.skip_offline = skip;
    }

    fn corner(&self) -> f64 {
        self.corner.unwrap_or(self.layout.corner)
    }

    pub fn role(&self) -> Role {
        match &self.mode {
            Mode::Local => Role::Idle,
            Mode::Remote { target, .. } => Role::Controlling(target.clone()),
            Mode::Controlled { by, .. } => Role::ControlledBy(by.clone()),
        }
    }

    pub fn set_layout(&mut self, layout: Layout) {
        self.layout = layout;
    }

    /// The sides of this machine the pointer can leave over: a machine that is online lies beyond
    /// them somewhere along the edge (past ones that are not, if allowed).
    pub fn open_sides(&self) -> Vec<Side> {
        let online = |m: &str| self.machines.contains_key(m);
        [Side::Left, Side::Right, Side::Top, Side::Bottom]
            .into_iter()
            .filter(|&side| {
                self.online_neighbor(&self.me, side).is_some()
                    || (0..=20).any(|i| {
                        let along = f64::from(i) / 20.0;
                        self.layout
                            .across_online(&self.me, side, along, self.skip_offline, online)
                            .is_some()
                    })
            })
            .collect()
    }

    pub fn set_local_screens(&mut self, screens: &[Screen]) {
        match Screens::new(screens) {
            Some(s) => self.machines.insert(self.me.clone(), s),
            None => self.machines.remove(&self.me),
        };
    }

    /// A peer connected (or sent new screens).
    pub fn peer_online(&mut self, id: &str, screens: &[Screen]) {
        if id == self.me {
            return;
        }
        match Screens::new(screens) {
            Some(s) => self.machines.insert(id.to_string(), s),
            None => self.machines.remove(id),
        };
    }

    /// A peer's link ended: everything involving it is released.
    pub fn peer_offline(&mut self, id: &str) -> Vec<Action> {
        let mut actions = Vec::new();
        self.machines.remove(id);
        let remote = Dest::Remote(id.to_string());
        for dests in self.held_keys.values_mut().chain(self.held_buttons.values_mut()) {
            dests.remove(&remote);
        }
        match &self.mode {
            Mode::Remote { target, exit, .. } if target == id => {
                let exit = *exit;
                self.return_home(exit, &mut actions);
            }
            Mode::Controlled { by, .. } if by == id => {
                self.release_injected(&mut actions);
                self.mode = Mode::Local;
            }
            _ => {}
        }
        actions
    }

    /// Input from this machine's own devices.
    pub fn captured(&mut self, event: Captured) -> Vec<Action> {
        let mut actions = Vec::new();
        if let Mode::Controlled { by, .. } = &self.mode {
            let by = by.clone();
            let Captured::Motion { x, y, dx, dy } = event else {
                // Local keys and clicks just reach the local system, as usual
                return match event {
                    Captured::Key { usage, down } => {
                        self.key(usage, down, &mut actions);
                        actions
                    }
                    Captured::Button { button, down } => {
                        self.button(button, down, &mut actions);
                        actions
                    }
                    _ => actions,
                };
            };
            let scale = self
                .machines
                .get(&self.me)
                .map_or(1.0, |s| s.screen_at(Point::new(x, y)).scale);
            self.takeover_motion += dx.hypot(dy) / scale;
            if self.takeover_motion < TAKEOVER_MOTION {
                return actions;
            }
            // Someone moves this machine's mouse: it takes control back
            self.release_injected(&mut actions);
            actions.push(Action::Send {
                to: by,
                message: Message::TakeOver,
            });
            self.mode = Mode::Local;
        }
        match event {
            Captured::Motion { x, y, dx, dy } => self.motion(Point::new(x, y), dx, dy, &mut actions),
            Captured::Key { usage, down } => self.key(usage, down, &mut actions),
            Captured::Button { button, down } => self.button(button, down, &mut actions),
            Captured::Media { key, down } => {
                if let (true, Mode::Remote { target, .. }) = (down, &self.mode) {
                    actions.push(Action::Send {
                        to: target.clone(),
                        message: Message::Media { key },
                    });
                }
            }
            Captured::Wheel { dx, dy, precise } => {
                if let Mode::Remote { target, .. } = &self.mode {
                    let (dx, dy) = match self.layout.scroll.get(target) {
                        Some(scroll) => scroll.apply(dx, dy),
                        None => (dx, dy),
                    };
                    actions.push(Action::Send {
                        to: target.clone(),
                        message: Message::Wheel {
                            dx: dx as f32,
                            dy: dy as f32,
                            precise,
                        },
                    });
                }
            }
        }
        actions
    }

    /// A message from a peer (only input-related messages matter here).
    pub fn received(&mut self, from: &str, message: Message) -> Vec<Action> {
        let mut actions = Vec::new();
        let controlled_by_sender = matches!(&self.mode, Mode::Controlled { by, .. } if by == from);
        match message {
            Message::Enter { x, y } => {
                match &self.mode {
                    Mode::Remote { target, exit, .. } => {
                        // Someone else took the lead while we were leading: step back
                        let (target, exit) = (target.clone(), *exit);
                        self.leave_remote(&target, &mut actions);
                        actions.push(Action::Ungrab { at: exit });
                    }
                    // Another leader comes in: the previous one must stop, otherwise it keeps its
                    // pointer hidden and sends moves that are ignored here
                    Mode::Controlled { by, .. } if by != from => actions.push(Action::Send {
                        to: by.clone(),
                        message: Message::TakeOver,
                    }),
                    Mode::Controlled { .. } | Mode::Local => {}
                }
                self.release_injected(&mut actions);
                self.mode = Mode::Controlled {
                    by: from.to_string(),
                    last_seq: None,
                };
                self.takeover_motion = 0.0;
                actions.push(Action::Inject(Message::Enter { x, y }));
            }
            Message::MouseMove { seq, .. } if controlled_by_sender => {
                if let Mode::Controlled { last_seq, .. } = &mut self.mode
                    && last_seq.is_none_or(|last| newer(seq, last))
                {
                    *last_seq = Some(seq);
                    actions.push(Action::Inject(message));
                }
            }
            Message::Key { usage, down } if controlled_by_sender => {
                let forward = if down {
                    self.injected_keys.press(usage)
                } else {
                    self.injected_keys.release(usage)
                };
                if forward {
                    actions.push(Action::Inject(message));
                }
            }
            Message::MouseButton { button, down } if controlled_by_sender => {
                let forward = if down {
                    self.injected_buttons.insert(button)
                } else {
                    self.injected_buttons.remove(&button)
                };
                if forward {
                    actions.push(Action::Inject(message));
                }
            }
            Message::Wheel { .. } | Message::Media { .. } if controlled_by_sender => {
                actions.push(Action::Inject(message))
            }
            Message::ReleaseAll if controlled_by_sender => self.release_injected(&mut actions),
            Message::Leave if controlled_by_sender => {
                self.release_injected(&mut actions);
                self.mode = Mode::Local;
            }
            Message::TakeOver => {
                if let Mode::Remote { target, exit, .. } = &self.mode
                    && target == from
                {
                    // The target released everything itself
                    let exit = *exit;
                    let remote = Dest::Remote(from.to_string());
                    for dests in self.held_keys.values_mut().chain(self.held_buttons.values_mut()) {
                        dests.remove(&remote);
                    }
                    self.return_home(exit, &mut actions);
                }
            }
            _ => {}
        }
        actions
    }

    /// Call about every 250 ms: repeats the last position once after the pointer stopped, in case
    /// that datagram was lost, and forgets small local mouse movements.
    pub fn tick(&mut self) -> Vec<Action> {
        let mut actions = Vec::new();
        self.takeover_motion = 0.0;
        if self.resend
            && let Mode::Remote { target, pos, .. } = &self.mode
        {
            let (target, pos) = (target.clone(), *pos);
            self.send_position(&target, pos, 0.0, 0.0, &mut actions);
            self.resend = false;
        }
        actions
    }

    /// Moves control to the neighbor beside the machine the pointer is on now (keyboard shortcut).
    pub fn switch_side(&mut self, side: Side) -> Vec<Action> {
        let current = match &self.mode {
            Mode::Remote { target, .. } => target.clone(),
            _ => self.me.clone(),
        };
        match self.online_neighbor(&current, side) {
            Some(next) => self.switch_to(&next),
            None => Vec::new(),
        }
    }

    /// Puts the pointer in the middle of `machine` (this one or a connected peer).
    pub fn switch_to(&mut self, machine: &str) -> Vec<Action> {
        let mut actions = Vec::new();
        if let Mode::Controlled { by, .. } = &self.mode {
            // Asked for on this machine's own keyboard: take control first
            let by = by.clone();
            self.release_injected(&mut actions);
            actions.push(Action::Send {
                to: by,
                message: Message::TakeOver,
            });
            self.mode = Mode::Local;
        }
        let Some(center) = self.machines.get(machine).map(|s| {
            let r = s.rects()[0];
            Point::new(((r.left + r.right) / 2.0).floor(), ((r.top + r.bottom) / 2.0).floor())
        }) else {
            return actions;
        };
        match self.mode.clone() {
            Mode::Remote { target, .. } if target == machine => {}
            Mode::Remote {
                target, exit, scale, ..
            } => {
                self.leave_remote(&target, &mut actions);
                if machine == self.me {
                    self.mode = Mode::Local;
                    self.sync_modifiers_home(&mut actions);
                    actions.push(Action::Ungrab { at: center });
                } else {
                    self.enter_remote(machine, center, &mut actions);
                    self.mode = Mode::Remote {
                        target: machine.to_string(),
                        pos: center,
                        exit,
                        scale,
                    };
                }
            }
            _ if machine == self.me => {}
            _ => {
                let park = self.local_pointer;
                let scale = self.machines.get(&self.me).map_or(1.0, |s| s.screen_at(park).scale);
                for (button, dests) in &mut self.held_buttons {
                    if dests.remove(&Dest::Local) {
                        actions.push(Action::Inject(Message::MouseButton {
                            button: *button,
                            down: false,
                        }));
                    }
                }
                actions.push(Action::Grab { park });
                self.enter_remote(machine, center, &mut actions);
                self.mode = Mode::Remote {
                    target: machine.to_string(),
                    pos: center,
                    exit: park,
                    scale,
                };
            }
        }
        actions
    }

    /// Emergency: give everything back to the local system and release all keys everywhere.
    /// A drag of files went over the edge: the held `button` now belongs to the target, which
    /// continues the drag (it presses the button itself), so letting go drops the files there.
    pub fn hand_over_button(&mut self, button: MouseButton) {
        if let Mode::Remote { target, .. } = &self.mode {
            self.held_buttons
                .entry(button)
                .or_default()
                .insert(Dest::Remote(target.clone()));
        }
    }

    pub fn release_everything(&mut self) -> Vec<Action> {
        let mut actions = Vec::new();
        match &self.mode {
            Mode::Remote { target, exit, .. } => {
                let (target, exit) = (target.clone(), *exit);
                self.leave_remote(&target, &mut actions);
                actions.push(Action::Ungrab { at: exit });
            }
            Mode::Controlled { by, .. } => actions.push(Action::Send {
                to: by.clone(),
                message: Message::TakeOver,
            }),
            Mode::Local => {}
        }
        self.release_injected(&mut actions);
        self.mode = Mode::Local;
        actions
    }

    fn motion(&mut self, p: Point, dx: f64, dy: f64, actions: &mut Vec<Action>) {
        match self.mode.clone() {
            Mode::Local => {
                self.local_pointer = p;
                let Some(mine) = self.machines.get(&self.me) else {
                    return;
                };
                let Some(side) = mine.exit_side(p, Point::new(p.x + dx, p.y + dy)) else {
                    let me = self.me.clone();
                    self.end_push_if_left(&me, p);
                    return;
                };
                if self.pinned {
                    return;
                }
                if self.corner_blocks(side) && mine.corner_distance(side, p) < self.corner() {
                    if self.neighbor_entry(&self.me, side, p).is_some() {
                        let me = self.me.clone();
                        self.note_corner_block(&me, side, p);
                    }
                    return;
                }
                let scale = mine.screen_at(p).scale;
                let Some((next, entry)) = self.neighbor_entry(&self.me, side, p) else {
                    return;
                };
                let me = self.me.clone();
                if !self.pushed_long_enough(&me, side) {
                    return;
                }
                // A drag ends at the edge
                for (button, dests) in &mut self.held_buttons {
                    if dests.remove(&Dest::Local) {
                        actions.push(Action::Inject(Message::MouseButton {
                            button: *button,
                            down: false,
                        }));
                    }
                }
                actions.push(Action::Grab { park: p });
                self.enter_remote(&next, entry, actions);
                self.mode = Mode::Remote {
                    target: next,
                    pos: entry,
                    exit: p,
                    scale,
                };
            }
            Mode::Remote {
                target,
                pos,
                exit,
                scale,
            } => {
                let Some(screens) = self.machines.get(&target) else {
                    self.return_home(exit, actions);
                    return;
                };
                let (lx, ly) = (dx / scale, dy / scale);
                let here = screens.screen_at(pos);
                let to = Point::new(pos.x + lx * here.scale, pos.y + ly * here.scale);
                let Some(side) = screens.exit_side(pos, to) else {
                    self.end_push_if_left(&target, to);
                    self.set_position(&target, to, lx, ly, actions);
                    return;
                };
                let clamped = here.clamp(to);
                if self.pinned {
                    self.set_position(&target, clamped, lx, ly, actions);
                    return;
                }
                if self.corner_blocks(side) && screens.corner_distance(side, pos) < self.corner() {
                    if self.neighbor_entry(&target, side, pos).is_some() {
                        self.note_corner_block(&target, side, pos);
                    }
                    self.set_position(&target, clamped, lx, ly, actions);
                    return;
                }
                let neighbor = self.neighbor_entry(&target, side, pos);
                if neighbor.is_some() && !self.pushed_long_enough(&target, side) {
                    self.set_position(&target, clamped, lx, ly, actions);
                    return;
                }
                match neighbor {
                    Some((next, entry)) if next == self.me => {
                        self.leave_remote(&target, actions);
                        self.mode = Mode::Local;
                        self.sync_modifiers_home(actions);
                        actions.push(Action::Ungrab { at: entry });
                    }
                    Some((next, entry)) => {
                        self.leave_remote(&target, actions);
                        self.enter_remote(&next, entry, actions);
                        self.mode = Mode::Remote {
                            target: next,
                            pos: entry,
                            exit,
                            scale,
                        };
                    }
                    None => self.set_position(&target, clamped, lx, ly, actions),
                }
            }
            Mode::Controlled { .. } => {}
        }
    }

    /// Leaving the edge ends a push. Sideways motion does not (a mouse sends many events without a
    /// vertical part even when it is pushed up or down), and neither does the small motion back of a
    /// finger put down again on a trackpad.
    fn end_push_if_left(&mut self, machine: &str, p: Point) {
        if let Some(tap) = &mut self.edge_tap
            && tap.machine == machine
            && self
                .machines
                .get(machine)
                .is_none_or(|screens| screens.edge_distance(tap.side, p) > TAP_AWAY)
        {
            tap.away = true;
        }
        let Some(push) = &self.edge_push else {
            return;
        };
        if push.machine != machine {
            return;
        }
        let left = self
            .machines
            .get(machine)
            .is_none_or(|screens| screens.edge_distance(push.side, p) > EDGE_LEAVE);
        if left {
            self.edge_push = None;
        }
    }

    /// Corners keep left and right edges from switching, so the pointer can reach them. Top and
    /// bottom edges have no corner rule: it would trap a pointer that runs straight down along a
    /// side edge.
    fn corner_blocks(&self, side: Side) -> bool {
        !matches!(side, Side::Top | Side::Bottom)
    }

    fn note_corner_block(&mut self, machine: &str, side: Side, at: Point) {
        let now = (self.clock.0)();
        if let Some((m, s, last)) = &mut self.corner_reported
            && m == machine
            && *s == side
            && now - *last < CORNER_REPORT_PAUSE
        {
            *last = now;
            return;
        }
        self.corner_reported = Some((machine.to_string(), side, now));
        self.corner_block = Some(CornerBlock {
            machine: machine.to_string(),
            side,
            at,
        });
    }

    /// Top and bottom edges switch only after the pointer was pushed against them for a moment, in
    /// one go or in several strokes.
    fn pushed_long_enough(&mut self, machine: &str, side: Side) -> bool {
        if !matches!(side, Side::Top | Side::Bottom) {
            return true;
        }
        let now = (self.clock.0)();
        if self.double_tap {
            return self.tapped_twice(machine, side, now);
        }
        if self.edge_dwell.is_zero() {
            return true;
        }
        match &mut self.edge_push {
            Some(push) if push.machine == machine && push.side == side && now - push.last <= EDGE_PAUSE => {
                push.pushed += (now - push.last).min(EDGE_STEP);
                push.last = now;
                if push.pushed >= self.edge_dwell {
                    self.edge_push = None;
                    return true;
                }
                false
            }
            _ => {
                self.edge_push = Some(EdgePush {
                    machine: machine.to_string(),
                    side,
                    pushed: Duration::ZERO,
                    last: now,
                });
                false
            }
        }
    }

    /// Touching the edge, letting go and touching it again within `TAP_WINDOW`.
    fn tapped_twice(&mut self, machine: &str, side: Side, now: Instant) -> bool {
        match &mut self.edge_tap {
            Some(tap) if tap.machine == machine && tap.side == side && now - tap.at <= TAP_WINDOW => {
                if tap.away {
                    self.edge_tap = None;
                    return true;
                }
                tap.at = now;
                false
            }
            _ => {
                self.edge_tap = Some(EdgeTap {
                    machine: machine.to_string(),
                    side,
                    at: now,
                    away: false,
                });
                false
            }
        }
    }

    /// A machine beyond `side` of `machine` that is online: the one in the middle of the edge if
    /// there is one, else any.
    fn online_neighbor(&self, machine: &str, side: Side) -> Option<String> {
        let online = |m: &str| self.machines.contains_key(m);
        self.layout
            .across_online(machine, side, 0.5, self.skip_offline, online)
            .map(|(m, _)| m)
            .or_else(|| self.layout.neighbors(machine, side).into_iter().find(|m| online(m)))
    }

    /// The machine beyond `side` of `machine` where `p` crosses it, and where the pointer enters
    /// it. Of machines sharing a place, the first one online; past machines that are not, if allowed.
    fn neighbor_entry(&self, machine: &str, side: Side, p: Point) -> Option<(MachineId, Point)> {
        let from = self.machines.get(machine)?;
        let (next, along) = self
            .layout
            .across_online(machine, side, from.along(side, p), self.skip_offline, |m| {
                self.machines.contains_key(m)
            })?;
        let entry = self.machines.get(&next)?.entry(side.opposite(), along);
        Some((next, entry))
    }

    fn set_position(&mut self, target: &str, pos: Point, lx: f64, ly: f64, actions: &mut Vec<Action>) {
        if let Mode::Remote { pos: current, .. } = &mut self.mode {
            if *current == pos {
                return;
            }
            *current = pos;
        }
        self.send_position(target, pos, lx, ly, actions);
        self.resend = true;
    }

    fn send_position(&mut self, target: &str, pos: Point, lx: f64, ly: f64, actions: &mut Vec<Action>) {
        self.seq = self.seq.wrapping_add(1);
        actions.push(Action::Send {
            to: target.to_string(),
            message: Message::MouseMove {
                seq: self.seq,
                x: pos.x as f32,
                y: pos.y as f32,
                dx: lx as f32,
                dy: ly as f32,
            },
        });
    }

    fn map_key(&self, target: &str, usage: u16) -> u16 {
        if self.layout.swap_cmd_ctrl.contains(target) {
            swap_cmd_ctrl(usage)
        } else {
            usage
        }
    }

    fn key(&mut self, usage: u16, down: bool, actions: &mut Vec<Action>) {
        if !tc_protocol::is_key_usage(usage) {
            return;
        }
        if down {
            if self.held_keys.contains_key(&usage) {
                return; // Auto-repeat: the receiving system repeats on its own
            }
            let dest = match &self.mode {
                Mode::Remote { target, .. } => {
                    let target = target.clone();
                    actions.push(Action::Send {
                        to: target.clone(),
                        message: Message::Key {
                            usage: self.map_key(&target, usage),
                            down: true,
                        },
                    });
                    Dest::Remote(target)
                }
                _ => Dest::Local,
            };
            self.held_keys.insert(usage, BTreeSet::from([dest]));
        } else if let Some(dests) = self.held_keys.remove(&usage) {
            let grabbed = matches!(self.mode, Mode::Remote { .. });
            for dest in dests {
                match dest {
                    // Without grab the local system got the release itself
                    Dest::Local if grabbed => actions.push(Action::Inject(Message::Key { usage, down: false })),
                    Dest::Local => {}
                    Dest::Remote(m) => actions.push(Action::Send {
                        message: Message::Key {
                            usage: self.map_key(&m, usage),
                            down: false,
                        },
                        to: m,
                    }),
                }
            }
        }
    }

    fn button(&mut self, button: MouseButton, down: bool, actions: &mut Vec<Action>) {
        if down {
            // Pressed while it counts as held: its release was missed (it may have gone to another
            // computer, or the system kept it during a drag). The old press ends where it went,
            // and this one counts, so a drag starting now is let go at the edge as it should.
            if let Some(stale) = self.held_buttons.remove(&button) {
                for dest in stale {
                    if let Dest::Remote(to) = dest {
                        actions.push(Action::Send {
                            to,
                            message: Message::MouseButton { button, down: false },
                        });
                    }
                }
            }
            let dest = match &self.mode {
                Mode::Remote { target, .. } => {
                    actions.push(Action::Send {
                        to: target.clone(),
                        message: Message::MouseButton { button, down: true },
                    });
                    Dest::Remote(target.clone())
                }
                _ => Dest::Local,
            };
            self.held_buttons.insert(button, BTreeSet::from([dest]));
        } else if let Some(dests) = self.held_buttons.remove(&button) {
            let grabbed = matches!(self.mode, Mode::Remote { .. });
            for dest in dests {
                match dest {
                    Dest::Local if grabbed => {
                        actions.push(Action::Inject(Message::MouseButton { button, down: false }))
                    }
                    Dest::Local => {}
                    Dest::Remote(to) => actions.push(Action::Send {
                        to,
                        message: Message::MouseButton { button, down: false },
                    }),
                }
            }
        }
    }

    /// Tells `target` the pointer arrives and presses the held modifiers there as well.
    fn enter_remote(&mut self, target: &str, entry: Point, actions: &mut Vec<Action>) {
        actions.push(Action::Send {
            to: target.to_string(),
            message: Message::Enter {
                x: entry.x as f32,
                y: entry.y as f32,
            },
        });
        let modifiers: Vec<u16> = self.held_keys.keys().copied().filter(|u| is_modifier(*u)).collect();
        for usage in modifiers {
            actions.push(Action::Send {
                to: target.to_string(),
                message: Message::Key {
                    usage: self.map_key(target, usage),
                    down: true,
                },
            });
            if let Some(dests) = self.held_keys.get_mut(&usage) {
                dests.insert(Dest::Remote(target.to_string()));
            }
        }
        self.resend = false;
    }

    /// Releases on `target` whatever it got from here and says goodbye.
    fn leave_remote(&mut self, target: &str, actions: &mut Vec<Action>) {
        let remote = Dest::Remote(target.to_string());
        let swap = self.layout.swap_cmd_ctrl.contains(target);
        let mut releases = Vec::new();
        for (usage, dests) in &mut self.held_keys {
            if dests.remove(&remote) {
                releases.push(Message::Key {
                    usage: if swap { swap_cmd_ctrl(*usage) } else { *usage },
                    down: false,
                });
            }
        }
        for (button, dests) in &mut self.held_buttons {
            if dests.remove(&remote) {
                releases.push(Message::MouseButton {
                    button: *button,
                    down: false,
                });
            }
        }
        for message in releases {
            actions.push(Action::Send {
                to: target.to_string(),
                message,
            });
        }
        actions.push(Action::Send {
            to: target.to_string(),
            message: Message::Leave,
        });
        self.resend = false;
    }

    /// Back to local control after the target went away or took over.
    fn return_home(&mut self, at: Point, actions: &mut Vec<Action>) {
        self.mode = Mode::Local;
        self.sync_modifiers_home(actions);
        actions.push(Action::Ungrab { at });
        self.resend = false;
    }

    /// Modifiers still held but unknown to the local system (pressed while grabbed) are pressed
    /// locally, so a held Shift keeps working on this side.
    fn sync_modifiers_home(&mut self, actions: &mut Vec<Action>) {
        for (usage, dests) in &mut self.held_keys {
            if is_modifier(*usage) && !dests.contains(&Dest::Local) {
                dests.insert(Dest::Local);
                actions.push(Action::Inject(Message::Key {
                    usage: *usage,
                    down: true,
                }));
            }
        }
    }

    fn release_injected(&mut self, actions: &mut Vec<Action>) {
        for usage in self.injected_keys.release_all() {
            actions.push(Action::Inject(Message::Key { usage, down: false }));
        }
        for button in std::mem::take(&mut self.injected_buttons) {
            actions.push(Action::Inject(Message::MouseButton { button, down: false }));
        }
    }
}
