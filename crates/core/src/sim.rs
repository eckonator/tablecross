//! Several machines with simulated operating systems and network, to test whole sequences.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tc_protocol::{MediaKey, Message, MouseButton, Screen};

use crate::engine::{Action, Captured, EDGE_DWELL, EdgeRule, Engine, Role};
use crate::geometry::{Point, Screens, Side};
use crate::layout::Layout;

/// What a machine's operating system believes.
#[derive(Debug, Default)]
struct Os {
    keys: BTreeSet<u16>,
    buttons: BTreeSet<MouseButton>,
    pointer: Point,
    grabbed: bool,
    /// Keys and buttons as applications saw them, in order.
    seen: Vec<String>,
    wheel: Vec<(f32, f32, bool)>,
}

struct Machine {
    engine: Engine,
    os: Os,
    screens: Screens,
}

struct Sim {
    machines: BTreeMap<String, Machine>,
    offline: BTreeSet<(String, String)>,
    /// Time of all engines; every mouse event takes 10 ms.
    now: Arc<Mutex<Instant>>,
}

const A: u16 = 0x04;
const C: u16 = 0x06;
const LEFT_SHIFT: u16 = 0xE1;
const LEFT_CTRL: u16 = 0xE0;
const LEFT_GUI: u16 = 0xE3;

fn screen(x: i32, y: i32, w: u32, h: u32, scale: f32) -> Screen {
    Screen {
        x,
        y,
        width: w,
        height: h,
        scale,
    }
}

impl Sim {
    /// `machines`: name and screens. `layout` applies to all.
    fn new(machines: &[(&str, Vec<Screen>)], layout: Layout) -> Self {
        let mut sim = Sim {
            machines: BTreeMap::new(),
            offline: BTreeSet::new(),
            now: Arc::new(Mutex::new(Instant::now())),
        };
        for (name, screens) in machines {
            let mut engine = Engine::new(*name, screens, layout.clone());
            engine.set_edge_dwell(EDGE_DWELL);
            let now = sim.now.clone();
            engine.set_clock(move || *now.lock().unwrap());
            for (other, other_screens) in machines {
                if other != name {
                    engine.peer_online(other, other_screens);
                }
            }
            let screens = Screens::new(screens).unwrap();
            let center = screens.rects()[0];
            sim.machines.insert(
                name.to_string(),
                Machine {
                    engine,
                    os: Os {
                        pointer: Point::new((center.left + center.right) / 2.0, (center.top + center.bottom) / 2.0),
                        ..Os::default()
                    },
                    screens,
                },
            );
        }
        sim
    }

    /// Mac (1512×982 points) on the left, PC (3840×2160 pixels at 200 %) on the right.
    fn mac_pc() -> Self {
        Sim::new(
            &[
                ("mac", vec![screen(0, 0, 1512, 982, 1.0)]),
                ("pc", vec![screen(0, 0, 3840, 2160, 2.0)]),
            ],
            Layout::default().with("pc", Side::Right, "mac"),
        )
    }

    fn os(&self, m: &str) -> &Os {
        &self.machines[m].os
    }

    fn role(&self, m: &str) -> Role {
        self.machines[m].engine.role()
    }

    fn feed(&mut self, m: &str, event: Captured) {
        let actions = self.machines.get_mut(m).unwrap().engine.captured(event);
        self.apply(m, actions);
    }

    fn apply(&mut self, m: &str, actions: Vec<Action>) {
        let mut queue: VecDeque<(String, Action)> = actions.into_iter().map(|a| (m.to_string(), a)).collect();
        while let Some((m, action)) = queue.pop_front() {
            match action {
                Action::Grab { park } => {
                    let os = &mut self.machines.get_mut(&m).unwrap().os;
                    os.grabbed = true;
                    os.pointer = park;
                }
                Action::Ungrab { at } => {
                    let os = &mut self.machines.get_mut(&m).unwrap().os;
                    os.grabbed = false;
                    os.pointer = at;
                }
                Action::Inject(message) => inject(&mut self.machines.get_mut(&m).unwrap().os, message),
                Action::Send { to, message } => {
                    if self.offline.contains(&(m.clone(), to.clone())) {
                        continue;
                    }
                    let more = self.machines.get_mut(&to).unwrap().engine.received(&m, message);
                    queue.extend(more.into_iter().map(|a| (to.clone(), a)));
                }
            }
        }
    }

    /// The user moves the mouse at `m` by `dx`/`dy` native units.
    fn mouse(&mut self, m: &str, dx: f64, dy: f64) {
        *self.now.lock().unwrap() += Duration::from_millis(10);
        let machine = self.machines.get_mut(m).unwrap();
        let p = machine.os.pointer;
        if !machine.os.grabbed {
            let to = Point::new(p.x + dx, p.y + dy);
            machine.os.pointer = if machine.screens.contains(to) {
                to
            } else {
                machine.screens.screen_at(p).clamp(to)
            };
        }
        let at = machine.os.pointer;
        self.feed(
            m,
            Captured::Motion {
                x: at.x,
                y: at.y,
                dx,
                dy,
            },
        );
    }

    /// Moves in small steps, like a real mouse.
    fn glide(&mut self, m: &str, dx: f64, dy: f64) {
        let steps = (dx.abs().max(dy.abs()) / 10.0).ceil().max(1.0);
        for _ in 0..steps as usize {
            self.mouse(m, dx / steps, dy / steps);
        }
    }

    fn key(&mut self, m: &str, usage: u16, down: bool) {
        let os = &mut self.machines.get_mut(m).unwrap().os;
        if !os.grabbed {
            native_key(os, usage, down);
        }
        self.feed(m, Captured::Key { usage, down });
    }

    fn button(&mut self, m: &str, button: MouseButton, down: bool) {
        let os = &mut self.machines.get_mut(m).unwrap().os;
        if !os.grabbed {
            native_button(os, button, down);
        }
        self.feed(m, Captured::Button { button, down });
    }

    fn disconnect(&mut self, a: &str, b: &str) {
        self.offline.insert((a.to_string(), b.to_string()));
        self.offline.insert((b.to_string(), a.to_string()));
        let actions = self.machines.get_mut(a).unwrap().engine.peer_offline(b);
        self.apply(a, actions);
        let actions = self.machines.get_mut(b).unwrap().engine.peer_offline(a);
        self.apply(b, actions);
    }

    /// Nothing is held anywhere.
    fn assert_all_released(&self) {
        for (name, m) in &self.machines {
            assert!(m.os.keys.is_empty(), "{name} still holds keys {:x?}", m.os.keys);
            assert!(m.os.buttons.is_empty(), "{name} still holds buttons {:?}", m.os.buttons);
        }
    }
}

fn native_key(os: &mut Os, usage: u16, down: bool) {
    if down {
        os.keys.insert(usage);
    } else {
        os.keys.remove(&usage);
    }
    os.seen.push(format!("{}{usage:x}", if down { "+" } else { "-" }));
}

fn native_button(os: &mut Os, button: MouseButton, down: bool) {
    if down {
        os.buttons.insert(button);
    } else {
        os.buttons.remove(&button);
    }
    os.seen.push(format!("{}{button:?}", if down { "+" } else { "-" }));
}

fn inject(os: &mut Os, message: Message) {
    match message {
        Message::Enter { x, y } | Message::MouseMove { x, y, .. } => {
            os.pointer = Point::new(f64::from(x), f64::from(y));
        }
        Message::Key { usage, down } => {
            assert_ne!(
                os.keys.contains(&usage),
                down,
                "injected key state flips: {usage:x} {down}"
            );
            native_key(os, usage, down);
        }
        Message::MouseButton { button, down } => {
            assert_ne!(os.buttons.contains(&button), down, "injected button state flips");
            native_button(os, button, down);
        }
        Message::Wheel { dx, dy, precise } => os.wheel.push((dx, dy, precise)),
        Message::Media { key } => os.seen.push(format!("{key:?}")),
        other => panic!("unexpected injection {other:?}"),
    }
}

/// Moves the Mac pointer to the right edge at mid height and over it.
fn cross_to_pc(sim: &mut Sim) {
    sim.machines.get_mut("mac").unwrap().os.pointer = Point::new(1400.0, 491.0);
    for _ in 0..30 {
        sim.mouse("mac", 10.0, 0.0);
        if sim.role("mac") == Role::Controlling("pc".into()) {
            return;
        }
    }
    panic!("did not cross");
}

#[test]
fn crossing_and_typing_reach_only_the_target() {
    let mut sim = Sim::mac_pc();
    sim.key("mac", A, true);
    sim.key("mac", A, false);
    assert_eq!(sim.os("mac").seen, ["+4", "-4"]);

    cross_to_pc(&mut sim);
    assert!(sim.os("mac").grabbed);
    assert_eq!(sim.role("pc"), Role::ControlledBy("mac".into()));
    // Entered at the left edge, same relative height (mid)
    let p = sim.os("pc").pointer;
    assert_eq!(p.x, 0.0);
    assert!((p.y - 1080.0).abs() < 3.0, "{p:?}");

    sim.key("mac", A, true);
    sim.key("mac", A, true); // auto-repeat is not forwarded
    sim.key("mac", A, false);
    assert_eq!(sim.os("pc").seen, ["+4", "-4"]);
    assert_eq!(sim.os("mac").seen, ["+4", "-4"]);

    // 10 points at scale 1 on the Mac are 20 pixels at 200 % on the PC
    sim.mouse("mac", 10.0, 0.0);
    assert_eq!(sim.os("pc").pointer.x, 20.0);

    // Back: the PC pointer goes over its left edge, the Mac pointer appears at its right edge
    sim.glide("mac", -20.0, 0.0);
    assert_eq!(sim.role("mac"), Role::Idle);
    assert_eq!(sim.role("pc"), Role::Idle);
    assert!(!sim.os("mac").grabbed);
    assert_eq!(sim.os("mac").pointer.x, 1511.0);
    sim.assert_all_released();
}

#[test]
fn a_drag_handed_over_at_the_edge_is_let_go_on_the_target() {
    let mut sim = Sim::mac_pc();
    sim.button("mac", MouseButton::Left, true);
    cross_to_pc(&mut sim);
    // The drag ended on the Mac at the edge
    assert!(sim.os("mac").buttons.is_empty());
    sim.machines
        .get_mut("mac")
        .unwrap()
        .engine
        .hand_over_button(MouseButton::Left);
    // The PC continues the drag: it presses the button itself
    let down = Message::MouseButton {
        button: MouseButton::Left,
        down: true,
    };
    let actions = sim.machines.get_mut("pc").unwrap().engine.received("mac", down);
    sim.apply("pc", actions);
    assert!(sim.os("pc").buttons.contains(&MouseButton::Left));
    // Letting go on the Mac drops there
    sim.button("mac", MouseButton::Left, false);
    assert!(sim.os("pc").buttons.is_empty());
    sim.assert_all_released();
}

#[test]
fn disconnect_in_the_middle_of_a_key_press_leaves_nothing_held() {
    let mut sim = Sim::mac_pc();
    cross_to_pc(&mut sim);
    sim.key("mac", LEFT_SHIFT, true);
    sim.key("mac", A, true);
    sim.button("mac", MouseButton::Left, true);
    assert_eq!(sim.os("pc").keys.len(), 2);
    sim.disconnect("mac", "pc");
    // The PC released everything; the Mac is back home
    assert!(sim.os("pc").keys.is_empty());
    assert!(sim.os("pc").buttons.is_empty());
    assert!(!sim.os("mac").grabbed);
    assert_eq!(sim.role("mac"), Role::Idle);
    // Shift is still held by the user, so the Mac knows it now
    assert_eq!(sim.os("mac").keys, BTreeSet::from([LEFT_SHIFT]));
    sim.key("mac", A, false);
    sim.key("mac", LEFT_SHIFT, false);
    sim.button("mac", MouseButton::Left, false);
    sim.assert_all_released();
}

#[test]
fn modifiers_follow_the_pointer_across_the_edge() {
    let mut sim = Sim::mac_pc();
    // Shift-click on the other side with Shift pressed before crossing
    sim.key("mac", LEFT_SHIFT, true);
    cross_to_pc(&mut sim);
    assert_eq!(sim.os("pc").keys, BTreeSet::from([LEFT_SHIFT]));
    sim.button("mac", MouseButton::Left, true);
    sim.button("mac", MouseButton::Left, false);
    assert_eq!(sim.os("pc").seen, ["+e1", "+Left", "-Left"]);
    // Releasing Shift reaches both systems (the Mac had it pressed natively)
    sim.key("mac", LEFT_SHIFT, false);
    sim.assert_all_released();

    // Ctrl held while coming back is pressed on the Mac and released on the PC
    sim.key("mac", LEFT_CTRL, true);
    sim.glide("mac", -50.0, 0.0);
    assert_eq!(sim.role("mac"), Role::Idle);
    assert!(sim.os("pc").keys.is_empty());
    assert_eq!(sim.os("mac").keys, BTreeSet::from([LEFT_CTRL]));
    sim.key("mac", C, true);
    sim.key("mac", C, false);
    sim.key("mac", LEFT_CTRL, false);
    sim.assert_all_released();
}

#[test]
fn a_press_after_a_missed_release_is_released_at_the_edge() {
    let mut sim = Sim::mac_pc();
    sim.machines.get_mut("mac").unwrap().os.pointer = Point::new(1400.0, 491.0);
    sim.button("mac", MouseButton::Left, true);
    sim.glide("mac", 200.0, 0.0);
    // The release never reaches the engine
    sim.machines.get_mut("mac").unwrap().os.buttons.clear();
    sim.glide("mac", -400.0, 0.0);
    assert_eq!(sim.role("mac"), Role::Idle);
    // The next drag still ends at the edge
    sim.button("mac", MouseButton::Left, true);
    cross_to_pc(&mut sim);
    assert!(sim.os("mac").buttons.is_empty(), "drag must end on the Mac");
    sim.button("mac", MouseButton::Left, false);
    sim.assert_all_released();
}

#[test]
fn a_drag_is_released_at_the_edge() {
    let mut sim = Sim::mac_pc();
    sim.machines.get_mut("mac").unwrap().os.pointer = Point::new(1400.0, 491.0);
    sim.button("mac", MouseButton::Left, true);
    sim.glide("mac", 200.0, 0.0);
    assert_eq!(sim.role("mac"), Role::Controlling("pc".into()));
    assert!(sim.os("mac").buttons.is_empty(), "drag must end on the Mac");
    assert!(sim.os("pc").buttons.is_empty(), "and not continue on the PC");
    sim.button("mac", MouseButton::Left, false);
    sim.assert_all_released();
    assert!(sim.os("pc").seen.is_empty());
}

#[test]
fn corners_do_not_switch() {
    let mut sim = Sim::mac_pc();
    sim.machines.get_mut("mac").unwrap().os.pointer = Point::new(1500.0, 5.0);
    sim.glide("mac", 100.0, 0.0);
    assert_eq!(sim.role("mac"), Role::Idle);
    sim.machines.get_mut("mac").unwrap().os.pointer = Point::new(1500.0, 975.0);
    sim.glide("mac", 100.0, 0.0);
    assert_eq!(sim.role("mac"), Role::Idle);
    // Left edge has no neighbor
    sim.machines.get_mut("mac").unwrap().os.pointer = Point::new(10.0, 491.0);
    sim.glide("mac", -100.0, 0.0);
    assert_eq!(sim.role("mac"), Role::Idle);

    // On the target the pointer slides along edges without neighbors and stops in corners
    cross_to_pc(&mut sim);
    sim.glide("mac", 0.0, -2000.0);
    assert_eq!(sim.os("pc").pointer.y, 0.0);
    sim.glide("mac", -100.0, 0.0);
    assert_eq!(
        sim.role("mac"),
        Role::Controlling("pc".into()),
        "top-left corner must not switch back"
    );
    sim.glide("mac", 4000.0, 0.0);
    assert_eq!(sim.os("pc").pointer.x, 3839.0);
}

#[test]
fn using_the_target_takes_control_back() {
    let mut sim = Sim::mac_pc();
    cross_to_pc(&mut sim);
    sim.key("mac", A, true);
    // A tiny wobble of the PC mouse does not count, not even many of them over time
    for _ in 0..40 {
        sim.mouse("pc", 3.0, 0.0);
        let actions = sim.machines.get_mut("pc").unwrap().engine.tick();
        sim.apply("pc", actions);
    }
    assert_eq!(sim.role("pc"), Role::ControlledBy("mac".into()));
    // Real movement does
    sim.mouse("pc", 30.0, 0.0);
    assert_eq!(sim.role("pc"), Role::Idle);
    assert_eq!(sim.role("mac"), Role::Idle);
    assert!(!sim.os("mac").grabbed);
    // The PC released the key the Mac had pressed
    assert!(sim.os("pc").keys.is_empty());
    sim.key("mac", A, false);
    // Typing on the Mac stays on the Mac now (the release of A reaches it natively, which is harmless)
    sim.key("mac", C, true);
    sim.key("mac", C, false);
    assert_eq!(sim.os("mac").seen, ["-4", "+6", "-6"]);
    sim.assert_all_released();

    // A key on the target types there and keeps the pointer where it is (mouse of one
    // computer, keyboard of the other)
    cross_to_pc(&mut sim);
    sim.key("pc", C, true);
    sim.key("pc", C, false);
    assert_eq!(sim.role("mac"), Role::Controlling("pc".into()));
    assert_eq!(sim.os("pc").seen.last().map(String::as_str), Some("-6"));
    sim.assert_all_released();
}

#[test]
fn two_laptops_used_in_turn_at_the_same_place() {
    let mut sim = Sim::new(
        &[
            ("imac", vec![screen(0, 0, 2560, 1440, 1.0)]),
            ("book1", vec![screen(0, 0, 1512, 982, 1.0)]),
            ("book2", vec![screen(0, 0, 1470, 956, 1.0)]),
        ],
        Layout::default()
            .with("book1", Side::Left, "imac")
            .with("book2", Side::Left, "imac"),
    );
    // Only the second laptop is there today
    sim.disconnect("imac", "book1");
    sim.disconnect("book1", "book2");
    sim.machines.get_mut("imac").unwrap().os.pointer = Point::new(10.0, 700.0);
    sim.glide("imac", -40.0, 0.0);
    assert_eq!(sim.role("imac"), Role::Controlling("book2".into()));
    sim.glide("imac", 3000.0, 0.0);
    assert_eq!(sim.role("imac"), Role::Idle);
}

#[test]
fn three_machines_in_a_row() {
    let mut sim = Sim::new(
        &[
            ("mac", vec![screen(0, 0, 1000, 800, 1.0)]),
            (
                "pc",
                vec![screen(0, 0, 1920, 1080, 1.0), screen(1920, 0, 1920, 1080, 1.0)],
            ),
            ("laptop", vec![screen(0, 0, 1280, 800, 1.25)]),
        ],
        Layout::default()
            .with("pc", Side::Right, "mac")
            .with("laptop", Side::Right, "pc"),
    );
    sim.machines.get_mut("mac").unwrap().os.pointer = Point::new(990.0, 400.0);
    sim.key("mac", LEFT_GUI, true);
    sim.glide("mac", 20.0, 0.0);
    assert_eq!(sim.role("mac"), Role::Controlling("pc".into()));
    // Across both PC screens
    sim.glide("mac", 3900.0, 0.0);
    assert_eq!(sim.role("mac"), Role::Controlling("laptop".into()));
    assert_eq!(sim.role("pc"), Role::Idle);
    assert_eq!(sim.role("laptop"), Role::ControlledBy("mac".into()));
    // The held ⌘ moved along: released on the PC, pressed on the laptop
    assert!(sim.os("pc").keys.is_empty());
    assert_eq!(sim.os("laptop").keys, BTreeSet::from([LEFT_GUI]));
    sim.key("mac", LEFT_GUI, false);
    // All the way back
    sim.glide("mac", -6000.0, 0.0);
    assert_eq!(sim.role("mac"), Role::Idle);
    sim.assert_all_released();
}

#[test]
fn a_computer_that_is_away_is_skipped() {
    let three = || {
        Sim::new(
            &[
                ("book", vec![screen(0, 0, 1000, 800, 1.0)]),
                ("middle", vec![screen(0, 0, 1000, 800, 1.0)]),
                ("imac", vec![screen(0, 0, 2000, 1000, 1.0)]),
            ],
            Layout::default()
                .with("middle", Side::Right, "book")
                .with("imac", Side::Right, "middle"),
        )
    };
    let mut sim = three();
    sim.disconnect("book", "middle");
    sim.disconnect("imac", "middle");
    sim.machines.get_mut("book").unwrap().os.pointer = Point::new(990.0, 200.0);
    sim.glide("book", 20.0, 0.0);
    assert_eq!(sim.role("book"), Role::Controlling("imac".into()));
    assert_eq!(sim.role("imac"), Role::ControlledBy("book".into()));
    // At the same height: a quarter of the way down
    assert_eq!(sim.os("imac").pointer.y, 250.0);
    sim.glide("book", -100.0, 0.0);
    assert_eq!(sim.role("book"), Role::Idle);
    // Switched off, the edge leads nowhere
    let mut sim = three();
    sim.machines.get_mut("book").unwrap().engine.set_skip_offline(false);
    sim.disconnect("book", "middle");
    sim.machines.get_mut("book").unwrap().os.pointer = Point::new(990.0, 200.0);
    sim.glide("book", 20.0, 0.0);
    assert_eq!(sim.role("book"), Role::Idle);
    sim.assert_all_released();
}

#[test]
fn only_edges_with_a_computer_online_behind_them_are_open() {
    let mut sim = Sim::new(
        &[
            ("book", vec![screen(0, 0, 1000, 800, 1.0)]),
            ("middle", vec![screen(0, 0, 1000, 800, 1.0)]),
            ("imac", vec![screen(0, 0, 2000, 1000, 1.0)]),
        ],
        Layout::default()
            .with("middle", Side::Right, "book")
            .with("imac", Side::Right, "middle"),
    );
    let open = |sim: &Sim, m: &str| sim.machines[m].engine.open_sides();
    assert_eq!(open(&sim, "book"), vec![Side::Right]);
    assert_eq!(open(&sim, "middle"), vec![Side::Left, Side::Right]);
    sim.disconnect("book", "middle");
    // Past the one that is away
    assert_eq!(open(&sim, "book"), vec![Side::Right]);
    sim.machines.get_mut("book").unwrap().engine.set_skip_offline(false);
    assert!(open(&sim, "book").is_empty());
    // Alone on the desk
    let alone = Engine::new("solo", &[screen(0, 0, 1440, 900, 1.0)], Layout::default());
    assert!(alone.open_sides().is_empty());
}

#[test]
fn cmd_and_ctrl_can_be_swapped_per_target() {
    let mut layout = Layout::default().with("pc", Side::Right, "mac");
    layout.swap_cmd_ctrl.insert("pc".into());
    let mut sim = Sim::new(
        &[
            ("mac", vec![screen(0, 0, 1512, 982, 1.0)]),
            ("pc", vec![screen(0, 0, 1920, 1080, 1.0)]),
        ],
        layout,
    );
    cross_to_pc(&mut sim);
    // ⌘C on the Mac keyboard becomes Ctrl+C on the PC
    sim.key("mac", LEFT_GUI, true);
    sim.key("mac", C, true);
    sim.key("mac", C, false);
    sim.key("mac", LEFT_GUI, false);
    assert_eq!(sim.os("pc").seen, ["+e0", "+6", "-6", "-e0"]);
    sim.assert_all_released();
}

#[test]
fn the_wheel_follows_the_scroll_option_of_the_target() {
    let mut layout = Layout::default().with("pc", Side::Right, "mac");
    layout.scroll.insert(
        "pc".into(),
        crate::Scroll {
            invert: true,
            speed: 2.0,
        },
    );
    let mut sim = Sim::new(
        &[
            ("mac", vec![screen(0, 0, 1512, 982, 1.0)]),
            ("pc", vec![screen(0, 0, 1920, 1080, 1.0)]),
        ],
        layout,
    );
    cross_to_pc(&mut sim);
    let mac = &mut sim.machines.get_mut("mac").unwrap().engine;
    let sent = mac.captured(Captured::Wheel {
        dx: 1.0,
        dy: -3.0,
        precise: false,
    });
    let wheel: Vec<_> = sent
        .iter()
        .filter_map(|a| match a {
            Action::Send {
                message: Message::Wheel { dx, dy, .. },
                ..
            } => Some((*dx, *dy)),
            _ => None,
        })
        .collect();
    assert_eq!(wheel, [(-2.0, 6.0)]);
    sim.assert_all_released();
}

#[test]
fn scroll_speeds_from_the_file_stay_sensible() {
    let nonsense = crate::Scroll {
        invert: false,
        speed: f64::NAN,
    };
    assert!(nonsense.is_normal());
    let fast = crate::Scroll {
        invert: false,
        speed: 100.0,
    };
    assert_eq!(fast.apply(0.0, 1.0), (0.0, 4.0));
}

#[test]
fn old_positions_are_ignored_and_the_last_is_repeated() {
    let mut sim = Sim::mac_pc();
    cross_to_pc(&mut sim);
    let pc = &mut sim.machines.get_mut("pc").unwrap().engine;
    let newer = pc.received(
        "mac",
        Message::MouseMove {
            seq: 1000,
            x: 50.0,
            y: 50.0,
            dx: 0.0,
            dy: 0.0,
        },
    );
    assert_eq!(newer.len(), 1);
    let older = pc.received(
        "mac",
        Message::MouseMove {
            seq: 999,
            x: 10.0,
            y: 10.0,
            dx: 0.0,
            dy: 0.0,
        },
    );
    assert!(older.is_empty());
    // Messages from a machine that is not in control are ignored
    assert!(pc.received("laptop", Message::Key { usage: A, down: true }).is_empty());

    sim.mouse("mac", 5.0, 0.0);
    let mac = &mut sim.machines.get_mut("mac").unwrap().engine;
    let repeat = mac.tick();
    assert!(matches!(
        repeat.as_slice(),
        [Action::Send {
            message: Message::MouseMove { .. },
            ..
        }]
    ));
    assert!(mac.tick().is_empty(), "repeated only once");
}

#[test]
fn being_entered_by_another_leader_stops_leading() {
    let mut sim = Sim::new(
        &[
            ("mac", vec![screen(0, 0, 1000, 800, 1.0)]),
            ("pc", vec![screen(0, 0, 1000, 800, 1.0)]),
            ("laptop", vec![screen(0, 0, 1000, 800, 1.0)]),
        ],
        Layout::default()
            .with("pc", Side::Right, "mac")
            .with("laptop", Side::Left, "mac"),
    );
    sim.machines.get_mut("mac").unwrap().os.pointer = Point::new(990.0, 400.0);
    sim.glide("mac", 20.0, 0.0);
    sim.key("mac", A, true);
    assert_eq!(sim.role("mac"), Role::Controlling("pc".into()));
    // The laptop user moves into the Mac
    sim.machines.get_mut("laptop").unwrap().os.pointer = Point::new(990.0, 400.0);
    sim.glide("laptop", 20.0, 0.0);
    assert_eq!(sim.role("laptop"), Role::Controlling("mac".into()));
    assert_eq!(sim.role("mac"), Role::ControlledBy("laptop".into()));
    assert_eq!(sim.role("pc"), Role::Idle);
    assert!(sim.os("pc").keys.is_empty());
    sim.key("mac", A, false);
    sim.assert_all_released();
}

#[test]
fn a_second_leader_sends_the_first_one_home() {
    let mut sim = Sim::new(
        &[
            ("book2", vec![screen(0, 0, 1000, 800, 1.0)]),
            ("imac", vec![screen(0, 0, 1000, 800, 1.0)]),
            ("book1", vec![screen(0, 0, 1000, 800, 1.0)]),
        ],
        Layout::default()
            .with("imac", Side::Right, "book2")
            .with("book1", Side::Bottom, "imac"),
    );
    sim.machines.get_mut("book2").unwrap().os.pointer = Point::new(990.0, 400.0);
    sim.glide("book2", 20.0, 0.0);
    sim.key("book2", A, true);
    assert_eq!(sim.role("imac"), Role::ControlledBy("book2".into()));
    // Someone uses the other laptop and moves up into the iMac
    sim.machines.get_mut("book1").unwrap().os.pointer = Point::new(500.0, 10.0);
    sim.glide("book1", 0.0, -400.0);
    assert_eq!(sim.role("imac"), Role::ControlledBy("book1".into()));
    assert_eq!(sim.role("book2"), Role::Idle);
    assert!(!sim.os("book2").grabbed, "pointer visible again at home");
    sim.key("book2", A, false);
    sim.glide("book1", 0.0, 2000.0);
    assert_eq!(sim.role("imac"), Role::Idle);
    sim.assert_all_released();
}

#[test]
fn top_and_bottom_edges_need_a_moment_of_pushing() {
    // The iMac with the Dock at the bottom, a laptop below it, another one to its left
    let mut sim = Sim::new(
        &[
            ("imac", vec![screen(0, 0, 1000, 800, 1.0)]),
            ("below", vec![screen(0, 0, 1000, 800, 1.0)]),
            ("left", vec![screen(0, 0, 1000, 800, 1.0)]),
        ],
        Layout::default()
            .with("below", Side::Bottom, "imac")
            .with("left", Side::Left, "imac"),
    );
    // Reaching for the Dock and overshooting a little: stays on the iMac
    sim.machines.get_mut("imac").unwrap().os.pointer = Point::new(500.0, 780.0);
    sim.glide("imac", 0.0, 60.0);
    assert_eq!(sim.role("imac"), Role::Idle);
    // A pause and another short push: still there
    *sim.now.lock().unwrap() += Duration::from_millis(500);
    sim.glide("imac", 0.0, 60.0);
    assert_eq!(sim.role("imac"), Role::Idle);
    // Pushing on: over to the laptop below
    sim.glide("imac", 0.0, 400.0);
    assert_eq!(sim.role("imac"), Role::Controlling("below".into()));
    // Back up from the laptop: the same moment of pushing at its top edge
    sim.glide("imac", 0.0, -200.0);
    assert_eq!(sim.role("imac"), Role::Controlling("below".into()), "only a short push");
    sim.glide("imac", 0.0, -400.0);
    assert_eq!(sim.role("imac"), Role::Idle);
    // Pushing with a real mouse: many events have no vertical part at all
    for _ in 0..80 {
        sim.mouse("imac", 0.0, 12.0);
        sim.mouse("imac", 3.0, 0.0);
    }
    assert_eq!(
        sim.role("imac"),
        Role::Controlling("below".into()),
        "sideways jitter must not stop the push"
    );
    for _ in 0..80 {
        sim.mouse("imac", 0.0, -12.0);
        sim.mouse("imac", -3.0, 0.0);
    }
    assert_eq!(sim.role("imac"), Role::Idle);
    // A trackpad: short strokes, the finger lifted in between
    let stroke = |sim: &mut Sim| {
        // Putting the finger down moves the pointer back a little
        sim.mouse("imac", 0.0, -3.0);
        for _ in 0..12 {
            sim.mouse("imac", 0.0, 5.0);
        }
        *sim.now.lock().unwrap() += Duration::from_millis(300);
    };
    sim.machines.get_mut("imac").unwrap().os.pointer = Point::new(500.0, 795.0);
    stroke(&mut sim);
    assert_eq!(sim.role("imac"), Role::Idle, "one short stroke is not enough");
    stroke(&mut sim);
    stroke(&mut sim);
    assert_eq!(sim.role("imac"), Role::Controlling("below".into()), "strokes add up");
    sim.glide("imac", 0.0, -400.0);
    assert_eq!(sim.role("imac"), Role::Idle);
    // Left edge: at once, as before
    sim.machines.get_mut("imac").unwrap().os.pointer = Point::new(10.0, 400.0);
    sim.glide("imac", -20.0, 0.0);
    assert_eq!(sim.role("imac"), Role::Controlling("left".into()));
}

#[test]
fn bottom_corners_switch_after_a_push() {
    // The laptop below the iMac leads; a second laptop sits left of the iMac
    let mut sim = Sim::new(
        &[
            ("laptop", vec![screen(0, 0, 1000, 800, 1.0)]),
            ("imac", vec![screen(0, 0, 1000, 800, 1.0)]),
            ("left", vec![screen(0, 0, 1000, 800, 1.0)]),
        ],
        Layout::default()
            .with("laptop", Side::Bottom, "imac")
            .with("left", Side::Left, "imac"),
    );
    sim.machines.get_mut("laptop").unwrap().os.pointer = Point::new(500.0, 20.0);
    sim.glide("laptop", 0.0, -400.0);
    assert_eq!(sim.role("laptop"), Role::Controlling("imac".into()));
    sim.glide("laptop", -1000.0, 0.0);
    assert_eq!(sim.role("laptop"), Role::Controlling("left".into()));
    // Back to the iMac, staying right at its left edge
    let over = 1000.0 - sim.os("left").pointer.x;
    sim.glide("laptop", over + 4.0, 0.0);
    assert_eq!(sim.role("laptop"), Role::Controlling("imac".into()));
    assert!(sim.os("imac").pointer.x < 16.0);
    // Straight down along the left edge of the iMac into its bottom-left corner: home after a push
    sim.glide("laptop", 2.0, 2000.0);
    assert_eq!(sim.role("laptop"), Role::Idle);
    assert_eq!(sim.machines.get_mut("laptop").unwrap().engine.take_corner_block(), None);
}

#[test]
fn a_corner_that_keeps_the_pointer_is_reported() {
    let mut sim = Sim::mac_pc();
    sim.machines.get_mut("mac").unwrap().os.pointer = Point::new(1500.0, 5.0);
    sim.glide("mac", 100.0, 0.0);
    sim.glide("mac", 100.0, 0.0);
    assert_eq!(sim.role("mac"), Role::Idle);
    let engine = &mut sim.machines.get_mut("mac").unwrap().engine;
    let block = engine.take_corner_block().expect("the corner block is reported");
    assert_eq!((block.machine.as_str(), block.side), ("mac", Side::Right));
    assert_eq!(engine.take_corner_block(), None, "once");
}

#[test]
fn a_laptop_half_below_the_imac() {
    // The laptop leads and stands below the left half of the iMac; the other laptop is to the left
    let mut sim = Sim::new(
        &[
            ("laptop", vec![screen(0, 0, 1000, 800, 1.0)]),
            ("imac", vec![screen(0, 0, 2000, 1000, 1.0)]),
            ("left", vec![screen(0, 0, 1000, 800, 1.0)]),
        ],
        Layout::default()
            .at("imac", 0.0, 0.0)
            .at("laptop", -0.5, 1.0)
            .at("left", -1.0, 0.0),
    );
    // The left half of the laptop's top edge lies below the other laptop
    sim.machines.get_mut("laptop").unwrap().os.pointer = Point::new(250.0, 20.0);
    sim.glide("laptop", 0.0, -600.0);
    assert_eq!(sim.role("laptop"), Role::Controlling("left".into()));
    sim.glide("laptop", 0.0, 2000.0);
    assert_eq!(sim.role("laptop"), Role::Idle);
    // The right half leads to the left half of the iMac, at the matching place
    sim.machines.get_mut("laptop").unwrap().os.pointer = Point::new(750.0, 20.0);
    sim.glide("laptop", 0.0, -600.0);
    assert_eq!(sim.role("laptop"), Role::Controlling("imac".into()));
    let x = sim.os("imac").pointer.x;
    assert!((450.0..=550.0).contains(&x), "entered at {x}");
    // Nothing below the right half of the iMac: the pointer stays there
    sim.glide("laptop", 1000.0, 0.0);
    sim.glide("laptop", 0.0, 2000.0);
    assert_eq!(sim.role("laptop"), Role::Controlling("imac".into()));
    // From the left half down again
    sim.glide("laptop", -1000.0, 0.0);
    sim.glide("laptop", 0.0, 2000.0);
    assert_eq!(sim.role("laptop"), Role::Idle);
}

#[test]
fn without_pushing_top_and_bottom_switch_like_the_sides() {
    let mut sim = Sim::new(
        &[
            ("imac", vec![screen(0, 0, 1000, 800, 1.0)]),
            ("below", vec![screen(0, 0, 1000, 800, 1.0)]),
        ],
        Layout::default().with("below", Side::Bottom, "imac"),
    );
    sim.machines
        .get_mut("imac")
        .unwrap()
        .engine
        .set_edge_dwell(Duration::ZERO);
    // One short trackpad stroke over the edge is enough
    sim.machines.get_mut("imac").unwrap().os.pointer = Point::new(500.0, 790.0);
    sim.mouse("imac", 0.0, 6.0);
    sim.mouse("imac", 0.0, 6.0);
    assert_eq!(sim.role("imac"), Role::Controlling("below".into()));
    sim.mouse("imac", 0.0, -900.0);
    assert_eq!(sim.role("imac"), Role::Idle);
}

fn imac_and_below() -> Sim {
    Sim::new(
        &[
            ("imac", vec![screen(0, 0, 1000, 800, 1.0)]),
            ("below", vec![screen(0, 0, 1000, 800, 1.0)]),
            ("right", vec![screen(0, 0, 1000, 800, 1.0)]),
        ],
        Layout::default()
            .with("below", Side::Bottom, "imac")
            .with("right", Side::Right, "imac"),
    )
}

#[test]
fn a_pinned_pointer_stays_on_its_computer() {
    let mut sim = imac_and_below();
    sim.machines
        .get_mut("imac")
        .unwrap()
        .engine
        .set_edge_rule(EdgeRule::AtOnce);
    sim.machines.get_mut("imac").unwrap().engine.set_pinned(true);
    sim.machines.get_mut("imac").unwrap().os.pointer = Point::new(500.0, 400.0);
    sim.glide("imac", 2000.0, 0.0);
    sim.glide("imac", 0.0, 2000.0);
    assert_eq!(sim.role("imac"), Role::Idle);
    // Over there, pinned as well: no way back over the edge
    sim.machines.get_mut("imac").unwrap().engine.set_pinned(false);
    sim.glide("imac", 0.0, 100.0);
    assert_eq!(sim.role("imac"), Role::Controlling("below".into()));
    sim.machines.get_mut("imac").unwrap().engine.set_pinned(true);
    sim.glide("imac", 0.0, -3000.0);
    assert_eq!(sim.role("imac"), Role::Controlling("below".into()));
    // The shortcut still goes there
    let actions = sim.machines.get_mut("imac").unwrap().engine.switch_to("right");
    sim.apply("imac", actions);
    assert_eq!(sim.role("imac"), Role::Controlling("right".into()));
}

#[test]
fn a_double_tap_switches_at_top_and_bottom() {
    let mut sim = imac_and_below();
    sim.machines
        .get_mut("imac")
        .unwrap()
        .engine
        .set_edge_rule(EdgeRule::DoubleTap);
    sim.machines.get_mut("imac").unwrap().os.pointer = Point::new(500.0, 790.0);
    // Pushing on and on: no switch
    sim.glide("imac", 0.0, 600.0);
    assert_eq!(sim.role("imac"), Role::Idle);
    // Back a little and against the edge again: over
    sim.mouse("imac", 0.0, -10.0);
    sim.mouse("imac", 0.0, 20.0);
    assert_eq!(sim.role("imac"), Role::Controlling("below".into()));
    // Back up: a double tap at the top edge of the laptop
    sim.glide("imac", 0.0, -900.0);
    assert_eq!(sim.role("imac"), Role::Controlling("below".into()));
    sim.mouse("imac", 0.0, 10.0);
    sim.mouse("imac", 0.0, -20.0);
    assert_eq!(sim.role("imac"), Role::Idle);
    // Two touches too far apart in time: no switch
    *sim.now.lock().unwrap() += Duration::from_secs(5);
    sim.machines.get_mut("imac").unwrap().os.pointer = Point::new(500.0, 790.0);
    assert_eq!(sim.role("imac"), Role::Idle);
    sim.mouse("imac", 0.0, 20.0);
    sim.mouse("imac", 0.0, -10.0);
    *sim.now.lock().unwrap() += Duration::from_secs(2);
    sim.mouse("imac", 0.0, 20.0);
    assert_eq!(sim.role("imac"), Role::Idle);
    // Left and right still at once
    sim.machines.get_mut("imac").unwrap().os.pointer = Point::new(990.0, 400.0);
    sim.glide("imac", 30.0, 0.0);
    assert_eq!(sim.role("imac"), Role::Controlling("right".into()));
}

#[test]
fn the_corner_rule_can_be_widened_or_switched_off() {
    let mut sim = imac_and_below();
    sim.machines.get_mut("imac").unwrap().engine.set_corner(100.0);
    sim.machines.get_mut("imac").unwrap().os.pointer = Point::new(990.0, 60.0);
    sim.glide("imac", 30.0, 0.0);
    assert_eq!(sim.role("imac"), Role::Idle, "60 points from the corner, within 100");
    sim.machines.get_mut("imac").unwrap().engine.set_corner(0.0);
    sim.machines.get_mut("imac").unwrap().os.pointer = Point::new(990.0, 2.0);
    sim.glide("imac", 30.0, 0.0);
    assert_eq!(sim.role("imac"), Role::Controlling("right".into()));
}

#[test]
fn wheel_goes_to_the_target() {
    let mut sim = Sim::mac_pc();
    sim.feed(
        "mac",
        Captured::Wheel {
            dx: 0.0,
            dy: 3.0,
            precise: true,
        },
    );
    cross_to_pc(&mut sim);
    sim.feed(
        "mac",
        Captured::Wheel {
            dx: 0.0,
            dy: -1.0,
            precise: false,
        },
    );
    assert_eq!(sim.os("pc").wheel, [(0.0, -1.0, false)]);
}

#[test]
fn media_keys_go_to_the_target_once_per_press() {
    let mut sim = Sim::mac_pc();
    let press = |down| Captured::Media {
        key: MediaKey::VolumeUp,
        down,
    };
    // At home the system handles them itself
    sim.feed("mac", press(true));
    sim.feed("mac", press(false));
    assert!(sim.os("pc").seen.is_empty());
    cross_to_pc(&mut sim);
    sim.feed("mac", press(true));
    sim.feed("mac", press(true)); // auto-repeat
    sim.feed("mac", press(false));
    assert_eq!(sim.os("pc").seen, ["VolumeUp", "VolumeUp"]);
}

#[test]
fn emergency_release() {
    let mut sim = Sim::mac_pc();
    cross_to_pc(&mut sim);
    sim.key("mac", A, true);
    let actions = sim.machines.get_mut("mac").unwrap().engine.release_everything();
    sim.apply("mac", actions);
    assert_eq!(sim.role("mac"), Role::Idle);
    assert!(!sim.os("mac").grabbed);
    assert!(sim.os("pc").keys.is_empty());
}

#[test]
fn keyboard_shortcut_switches_machines() {
    let mut sim = Sim::mac_pc();
    sim.key("mac", LEFT_SHIFT, true);
    let actions = sim.machines.get_mut("mac").unwrap().engine.switch_side(Side::Right);
    sim.apply("mac", actions);
    assert_eq!(sim.role("mac"), Role::Controlling("pc".into()));
    assert_eq!(sim.os("pc").pointer, Point::new(1920.0, 1080.0));
    assert_eq!(sim.os("pc").keys, BTreeSet::from([LEFT_SHIFT]));
    // Nothing further right
    let actions = sim.machines.get_mut("mac").unwrap().engine.switch_side(Side::Right);
    assert!(actions.is_empty());
    // Back home, to the middle of the Mac
    let actions = sim.machines.get_mut("mac").unwrap().engine.switch_side(Side::Left);
    sim.apply("mac", actions);
    assert_eq!(sim.role("mac"), Role::Idle);
    assert_eq!(sim.os("mac").pointer, Point::new(756.0, 491.0));
    sim.key("mac", LEFT_SHIFT, false);
    sim.assert_all_released();

    // From the controlled side: the PC user takes over and leaves for the Mac
    cross_to_pc(&mut sim);
    let actions = sim.machines.get_mut("pc").unwrap().engine.switch_to("mac");
    sim.apply("pc", actions);
    assert_eq!(sim.role("pc"), Role::Controlling("mac".into()));
    assert_eq!(sim.role("mac"), Role::ControlledBy("pc".into()));
    sim.assert_all_released();
}
