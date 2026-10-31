//! The running session: network node, engine and input backend, driven by one loop. The app and
//! the command line tool send [`Command`]s and read a [`Status`] snapshot plus [`Notice`]s.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Condvar;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use tc_core::{Action, Area, Captured, EdgeRule, Engine, Layout, LayoutFile, Role, Side};
use tc_input::{Backend, Capture, Inject, Problem};
use tc_net::{
    Disconnect, Discovered, Event, Fingerprint, Identity, LinkInfo, Node, NodeConfig, PairError, PairingCode,
    PeerStore, Refusal,
};
use tc_protocol::{EdgeMark, Message, MouseButton, Screen};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::clip::{self, ClipAction, ClipSync, Clipboard, Skip};
use crate::files::{self, Listing};
use crate::layout_file;
use crate::peer_logs;
use crate::screen_size;
use crate::transfer::{self, Cancels, Progress, TransferState, TransferView};

const ESCAPE: u16 = 0x29;
const KEY_L: u16 = 0x0F;
const KEY_F: u16 = 0x09;
/// Keys 1 to 9 of the main block.
const DIGITS: [u16; 9] = [0x1E, 0x1F, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26];
const ARROWS: [(u16, Side); 4] = [
    (0x4F, Side::Right),
    (0x50, Side::Left),
    (0x51, Side::Bottom),
    (0x52, Side::Top),
];
/// Ctrl, Shift and Alt (either side) form the prefix of the keyboard shortcuts.
const PREFIX: [(u16, u16); 3] = [(0xE0, 0xE4), (0xE1, 0xE5), (0xE2, 0xE6)];
const MAX_NOTICES: usize = 200;
const STATUS_EVERY: Duration = Duration::from_millis(500);
/// This computer's area on the desk is measured anew this long after its screens or their sizes
/// changed (the system rearranges the displays for a moment).
const MEASURE_AFTER: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Settings folder (identity, peers, layout).
    pub dir: PathBuf,
    pub name: String,
    pub port: u16,
    pub discovery: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PairTarget {
    /// A computer found on the network.
    Discovered(Fingerprint),
    Address(SocketAddr),
}

#[derive(Debug, Clone)]
pub enum Command {
    /// Show a pairing code (for `PAIRING_WINDOW`).
    StartPairing,
    StopPairing,
    /// Pair with the computer that shows `code`.
    Pair {
        code: String,
        target: PairTarget,
    },
    Unpair(Fingerprint),
    /// The whole desk as areas by machine id; computers whose areas touch become neighbors where
    /// the edges overlap. Replaces the arrangement.
    Arrange(Vec<Area>),
    /// How top and bottom edges switch: at once, after a moment of pushing or after a double tap
    /// (the Dock and the menu bar are there).
    EdgeRule(EdgeRule),
    /// No switching at left and right edges this close to a corner (logical points).
    Corner(f64),
    /// Let the pointer go past computers that are not connected to the next one that is.
    SkipOffline(bool),
    /// macOS: pause AWDL (AirDrop, Handoff, AirPlay) while the pointer is shared, through the
    /// system helper.
    QuietRadio(bool),
    /// While controlled, switch to the keyboard layout of the computer that controls this one.
    FollowLayout(bool),
    /// macOS: let this Mac be controlled on the login screen as well (through the system helper).
    LoginScreen(bool),
    /// Keep the pointer on the computer it is on (`None`: the other way round).
    Pin(Option<bool>),
    /// While the arrangement is being changed here: where the pointer would cross, per connected
    /// computer (empty: nowhere), shown on their screens.
    ShowEdges(Vec<(Fingerprint, Vec<EdgeMark>)>),
    /// Lock the screens of all connected computers and of this one.
    LockAll,
    /// Shut down all connected computers, and this one as well with `this_one`.
    PowerOff {
        this_one: bool,
    },
    /// Log out on all connected computers (those that know how), and on this one as well with
    /// `this_one`.
    LogOut {
        this_one: bool,
    },
    /// Put a peer next to this computer (`None`: nowhere).
    Place {
        peer: Fingerprint,
        side: Option<Side>,
    },
    SwapCmdCtrl {
        peer: Fingerprint,
        swap: bool,
    },
    /// How the wheel scrolls on that computer when controlled from here.
    Scroll {
        peer: Fingerprint,
        scroll: tc_core::Scroll,
    },
    AddAddress {
        peer: Fingerprint,
        address: SocketAddr,
    },
    /// Put the pointer on that computer (`None`: this one).
    SwitchTo(Option<Fingerprint>),
    /// Emergency: everything back to local control, all keys released.
    ReleaseAll,
    /// Try to start input capture again (after granting a permission).
    RetryInput,
    CancelTransfer(u64),
    /// Fetch the files copied on another computer into this folder.
    SaveFiles(PathBuf),
    /// Clear the file list: finished transfers and the note about copied files (running
    /// transfers stay, copied files can still be pasted or saved).
    ClearFiles,
    /// Collect the logs of the connected computers in `logs/peers` (troubleshooting), or stop.
    CollectLogs(bool),
    /// Diagonals typed in for screens here, by what they report (see `screen_size::Typed`).
    ScreenDiagonals(Vec<screen_size::Typed>),
    Shutdown,
}

/// Something that happened, for the list of recent messages.
#[derive(Debug, Clone, PartialEq)]
pub enum Notice {
    Connected(String),
    Disconnected(String, Disconnect),
    Refused(String, Refusal),
    Paired(String),
    /// A wrong code was typed for the code shown here.
    PairingFailed(u8),
    PairingClosed,
    PairError(PairError),
    Unpaired(String, bool),
    Woke,
    Controlling(String),
    Local,
    ControlledBy(String),
    EmergencyRelease,
    Problem(Problem),
    InputFailed(String),
    /// Clipboard content from this computer stayed here.
    ClipboardSkipped(Skip),
    /// Clipboard content from this peer was taken over.
    ClipboardReceived(String),
    /// Files from this peer arrived in the folder; `true`: they are on the clipboard as well
    /// (dragged over the edge).
    FilesReceived(String, u64, PathBuf, bool),
    TransferFailed(String, String),
    /// All screens were locked from here.
    LockedAll,
    /// These computers were told to shut down from here.
    PoweringOff(Vec<String>),
    /// These computers were told to log out from here; the second list is too old for that.
    LoggingOut(Vec<String>, Vec<String>),
    /// The computer controlling this one types with a keyboard layout not set up here.
    LayoutMissing(String, String),
}

impl std::fmt::Display for Notice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Notice::Connected(n) => write!(f, "connected to {n}"),
            Notice::Disconnected(n, r) => write!(f, "disconnected from {n}: {r:?}"),
            Notice::Refused(n, r) => write!(f, "refused {n}: {r:?}"),
            Notice::Paired(n) => write!(f, "paired with {n}"),
            Notice::PairingFailed(left) => write!(f, "wrong pairing code entered ({left} attempts left)"),
            Notice::PairingClosed => write!(f, "pairing code void after too many attempts"),
            Notice::PairError(e) => write!(f, "pairing failed: {e}"),
            Notice::Unpaired(n, by_peer) => {
                write!(
                    f,
                    "pairing with {n} removed{}",
                    if *by_peer { " by the other computer" } else { "" }
                )
            }
            Notice::Woke => write!(f, "woke from sleep, reconnecting"),
            Notice::Controlling(n) => write!(f, "controlling {n}"),
            Notice::Local => write!(f, "input is local"),
            Notice::ControlledBy(n) => write!(f, "controlled by {n}"),
            Notice::EmergencyRelease => write!(f, "emergency release: everything released"),
            Notice::Problem(p) => write!(f, "warning: {p}"),
            Notice::InputFailed(e) => write!(f, "input capture not available: {e}"),
            Notice::ClipboardSkipped(Skip::Concealed) => write!(f, "clipboard not shared: marked as private"),
            Notice::ClipboardSkipped(Skip::TooLarge(n)) => write!(f, "clipboard not shared: too large ({n} bytes)"),
            Notice::ClipboardReceived(n) => write!(f, "clipboard taken over from {n}"),
            Notice::FilesReceived(n, count, dir, _) => {
                write!(f, "{count} file(s) from {n} in {}", dir.display())
            }
            Notice::TransferFailed(n, e) => write!(f, "file transfer with {n} failed: {e}"),
            Notice::LockedAll => write!(f, "all screens locked"),
            Notice::PoweringOff(names) => write!(f, "shutting down: {}", names.join(", ")),
            Notice::LoggingOut(names, too_old) => write!(
                f,
                "logging out: {}; too old for it: {}",
                names.join(", "),
                too_old.join(", ")
            ),
            Notice::LayoutMissing(name, layout) => {
                write!(
                    f,
                    "{name} types with the keyboard layout {layout}, which is not set up here"
                )
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct PeerView {
    pub fingerprint: Fingerprint,
    pub name: String,
    pub link: Option<LinkInfo>,
    pub reconnects: u32,
    pub refusal: Option<Refusal>,
    pub side: Option<Side>,
    pub swap_cmd_ctrl: bool,
    /// How the wheel scrolls there when controlled from here.
    pub scroll: tc_core::Scroll,
    /// Physical size of its screens in millimeters (see `Status::screen_sizes`).
    pub screen_sizes: Vec<(u32, u32)>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum InputState {
    #[default]
    Off,
    Running,
    Failed(String),
}

#[derive(Debug, Clone, Default)]
pub struct Status {
    pub name: String,
    pub fingerprint: Option<Fingerprint>,
    pub port: u16,
    pub controlling: Option<String>,
    pub controlled_by: Option<String>,
    pub peers: Vec<PeerView>,
    /// Computers on the network that are not paired.
    pub discovered: Vec<Discovered>,
    /// The code shown here and until when it is valid.
    pub pairing: Option<(PairingCode, Instant)>,
    pub pairing_busy: bool,
    pub input: InputState,
    pub problems: Vec<Problem>,
    pub screens: Vec<Screen>,
    /// Physical size of each screen in millimeters, in the order of `screens`; `(0, 0)` where
    /// unknown, empty where the system does not tell. Sizes that cannot be right are replaced
    /// as far as possible (see `screen_size::effective`).
    pub screen_sizes: Vec<(u32, u32)>,
    /// What the screens report about their size themselves, in the order of `screens`.
    pub reported_sizes: Vec<(u32, u32)>,
    /// One of the screens reports a size that cannot be right (its diagonal can be typed in).
    pub size_unsure: bool,
    pub notices: VecDeque<(SystemTime, Notice)>,
    pub layout: LayoutFile,
    /// File transfers, newest last.
    pub transfers: Vec<TransferView>,
    /// Files copied on another computer, to be saved here on request.
    pub files_waiting: Option<FilesWaiting>,
    /// Where the pointer crosses on this computer's screens, while another computer changes the
    /// arrangement, and when that came (shown for a moment).
    pub edge_marks: Option<(Vec<EdgeMark>, Instant)>,
    /// The pointer stays on the computer it is on (Ctrl+Shift+Alt+F).
    pub pinned: bool,
    /// macOS: Accessibility was granted, but this process does not get it before it starts anew.
    pub restart_needed: bool,
}

/// Talks to a running session; cheap to clone.
#[derive(Clone)]
pub struct SessionHandle {
    commands: mpsc::UnboundedSender<Command>,
    status: Arc<Mutex<Status>>,
    notices: broadcast::Sender<Notice>,
}

impl SessionHandle {
    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    pub fn status(&self) -> Status {
        self.status.lock().expect("status lock").clone()
    }

    pub fn notices(&self) -> broadcast::Receiver<Notice> {
        self.notices.subscribe()
    }
}

/// Files copied on another computer.
#[derive(Debug, Clone, PartialEq)]
pub struct FilesWaiting {
    pub peer: String,
    /// The names of the copied files and folders, shortened when there are many.
    pub label: String,
    pub count: u64,
    pub size: u64,
    /// On the clipboard here, fetched when pasted in the file manager (else saved on request).
    pub pasteable: bool,
}

/// Called whenever the status changed (the app repaints).
pub type OnChange = Arc<dyn Fn() + Send + Sync>;

enum Internal {
    Notice(Notice),
    /// The keyboard layout here, read for the computer controlled from here.
    OwnLayout(Fingerprint, tc_protocol::KeyboardLayout),
    /// What a fresh process says about the Accessibility permission (macOS).
    Accessibility(bool),
    PairDone(Result<String, PairError>),
    ClipRead(Option<tc_clipboard::Read>),
    ClipWritten(Fingerprint, Option<u64>),
    FilesListed(Result<Listing, String>),
    /// Files dragged over the edge, to be listed and sent.
    Dropped(Vec<PathBuf>),
    DropListed(Result<Listing, String>),
    LockAll,
    /// Promised files are being pasted: fetch them.
    Fetch(u64),
    /// Files dragged over the edge whose drag did not start here: into Downloads with them.
    DragGaveUp(u64),
}

/// How long pasting promised files waits for their transfer to start.
const PROMISE_START: Duration = Duration::from_secs(15);

/// Files copied on another computer, on this clipboard as a promise: fetched when pasted.
struct Promise {
    id: u64,
    dir: PathBuf,
    state: Mutex<Promised>,
    changed: Condvar,
}

enum Promised {
    Offered,
    Requested(Instant),
    Running,
    Done(Vec<PathBuf>),
    Failed,
}

impl Promise {
    fn set(&self, state: Promised) {
        *self.state.lock().expect("promise lock") = state;
        self.changed.notify_all();
    }

    /// Called by the file manager pasting (it waits): fetches the files once, then hands out where
    /// they are.
    fn fulfil(&self, internal: &mpsc::UnboundedSender<Internal>) -> Option<Vec<PathBuf>> {
        let mut state = self.state.lock().expect("promise lock");
        loop {
            match &*state {
                Promised::Done(paths) => return Some(paths.clone()),
                Promised::Failed => return None,
                Promised::Offered => {
                    *state = Promised::Requested(Instant::now());
                    let _ = internal.send(Internal::Fetch(self.id));
                }
                Promised::Requested(at) if at.elapsed() > PROMISE_START => {
                    // The other computer did not answer; pasting again tries again
                    *state = Promised::Offered;
                    return None;
                }
                Promised::Requested(_) | Promised::Running => {}
            }
            state = self
                .changed
                .wait_timeout(state, Duration::from_millis(250))
                .expect("promise lock")
                .0;
        }
    }
}

/// Copied files and running transfers.
#[derive(Default)]
struct FileState {
    /// Our copied files, if they are the newest content anywhere.
    local: Option<(u64, Arc<Listing>)>,
    /// Files waiting on a peer: peer, id, label, count, size, dragged over the edge.
    remote: Option<(Fingerprint, u64, String, u64, u64, bool)>,
    requested: Option<u64>,
    /// Where the requested files go (copied files, chosen by the user).
    save_to: Option<PathBuf>,
    /// The copied files on this clipboard as a promise, or files dragged over the edge being
    /// dragged on here.
    promise: Option<Arc<Promise>>,
    /// The controlling computer let go of the left button since the pointer came here (a drag
    /// that came over ended before its files were announced).
    left_up_seen: bool,
    /// State of the drag pasteboard when the controlling computer pressed the left button here.
    remote_press: Option<u64>,
    /// The pointer went back from this peer with the left button held: what it drags comes as
    /// dropped files, and the drag goes on here.
    drag_home: Option<(String, Instant)>,
    /// The pointer came from this computer with the left button held (the controlling computer
    /// said so): what it drags comes as dropped files, and the drag goes on here.
    drag_from: Option<(String, Instant)>,
    /// What we put on the clipboard after a transfer (not announced again).
    written: Vec<PathBuf>,
    cancels: Cancels,
    counter: u64,
}

struct Shared {
    node: Node,
    engine: Arc<Mutex<Engine>>,
    inject: Option<Arc<dyn Inject>>,
    status: Arc<Mutex<Status>>,
    notices: broadcast::Sender<Notice>,
    internal: mpsc::UnboundedSender<Internal>,
    on_change: Option<OnChange>,
    dir: PathBuf,
    clipboard: Option<Clipboard>,
    clip: Mutex<ClipSync>,
    files: Mutex<FileState>,
    logs: Mutex<LogState>,
    /// What the peers told about the physical size of their screens.
    peer_sizes: Mutex<HashMap<Fingerprint, Vec<(u32, u32)>>>,
    reported_sizes: Mutex<Reported>,
    /// When this computer's area is to be measured anew.
    measure_at: Mutex<Option<Instant>>,
    /// Pause AWDL while the pointer is shared (setting).
    quiet_radio: AtomicBool,
    /// Take over the keyboard layout of the computer that controls this one (setting).
    follow_layout: AtomicBool,
    keyboard: Arc<Mutex<KeyboardSync>>,
    /// Control on the login screen (setting; `None` until the app tells).
    login_screen: Mutex<Option<bool>>,
    /// Whether the program for the login screen has the Accessibility permission (`None`: unknown).
    login_access: Mutex<Option<bool>>,
}

/// Keyboard layouts between the computer that controls and the one controlled.
#[derive(Default)]
struct KeyboardSync {
    /// Controlling: what the computer controlled now was told.
    sent: Option<tc_protocol::KeyboardLayout>,
    /// When the layout here was read last for it.
    read: Option<Instant>,
    /// Controlled: the layout here before the other one's was taken over, to go back to.
    before: Option<tc_protocol::KeyboardLayout>,
    /// Controlled: a layout of the other computer missing here, told once.
    missing: Option<String>,
}

/// What this computer's screens report about their size in millimeters, and the diagonals typed in.
type Reported = (Vec<(u32, u32)>, Vec<screen_size::Typed>);

/// Log collection: whether this computer collects, and who collects ours.
#[derive(Default)]
struct LogState {
    collect: bool,
    /// Who collects our log, and up to where it got it.
    subscribers: Vec<(Fingerprint, u64)>,
}

impl Shared {
    fn notice(&self, notice: Notice) {
        tracing::info!("{notice}");
        let _ = self.notices.send(notice.clone());
        let mut status = self.status.lock().expect("status lock");
        status.notices.push_back((SystemTime::now(), notice));
        while status.notices.len() > MAX_NOTICES {
            status.notices.pop_front();
        }
        drop(status);
        self.changed();
    }

    fn changed(&self) {
        if let Some(f) = &self.on_change {
            f();
        }
    }

    fn name_of(&self, id: &str) -> String {
        id.parse::<Fingerprint>()
            .ok()
            .and_then(|fp| self.node.paired().into_iter().find(|p| p.fingerprint == fp))
            .map_or_else(|| id.chars().take(8).collect(), |p| p.name)
    }

    fn execute(&self, actions: Vec<Action>) {
        execute(actions, &self.node, &self.engine, self.inject.as_deref());
    }

    /// Turns a name or fingerprint prefix from `layout.json` into this machine or a paired one.
    fn resolver(&self) -> impl Fn(&str) -> Option<Fingerprint> + use<> {
        let machines: Vec<_> = std::iter::once((self.node.name().to_lowercase(), self.node.fingerprint()))
            .chain(
                self.node
                    .paired()
                    .into_iter()
                    .map(|p| (p.name.to_lowercase(), p.fingerprint)),
            )
            .collect();
        move |name: &str| {
            let name = name.to_lowercase();
            machines
                .iter()
                .find(|(n, fp)| *n == name || (name.len() >= 8 && fp.to_string().starts_with(&name)))
                .map(|(_, fp)| *fp)
        }
    }

    /// The computer with number `index + 1` on the desk, as machine id of the engine.
    fn numbered(&self, index: usize) -> Option<String> {
        let file = self.status.lock().expect("status lock").layout.clone();
        let resolve = self.resolver();
        let desk = layout_file::desk(&file, |entry| {
            resolve(entry).map_or_else(|| entry.to_string(), |fp| layout_file::machine_id(&fp))
        });
        let id = layout_file::reading_order(&desk).into_iter().nth(index)?;
        resolve(&id).map(|fp| fp.to_string())
    }

    fn layout(&self) -> Result<Layout> {
        let file = layout_file::load(&self.dir)?;
        let resolve = self.resolver();
        let layout = Layout::from_file(&file, |name: &str| resolve(name).map(|fp| fp.to_string()))?;
        self.status.lock().expect("status lock").layout = file;
        Ok(layout)
    }

    fn reload_layout(&self) {
        match self.layout() {
            Ok(layout) => self.engine.lock().expect("engine lock").set_layout(layout),
            Err(e) => tracing::warn!("layout: {e}"),
        }
    }

    /// Changes the arrangement here and passes it on to all connected computers.
    fn edit_layout(&self, edit: impl FnOnce(&mut LayoutFile)) {
        let me = self.node.fingerprint();
        let result = layout_file::load(&self.dir).and_then(|mut file| {
            edit(&mut file);
            layout_file::bump(&mut file, &me);
            layout_file::save(&self.dir, &file)
        });
        if let Err(e) = result {
            tracing::warn!("layout: {e}");
        }
        self.reload_layout();
        self.refresh();
        self.share_layout(None);
    }

    /// Changes what only holds here (nothing the other computers take over).
    fn edit_options(&self, edit: impl FnOnce(&mut LayoutFile)) {
        let result = layout_file::load(&self.dir).and_then(|mut file| {
            edit(&mut file);
            layout_file::save(&self.dir, &file)
        });
        if let Err(e) = result {
            tracing::warn!("layout: {e}");
        }
        self.reload_layout();
        self.refresh();
        self.changed();
    }

    /// Our arrangement for `to`: tiles if it runs a version that knows them, else neighbors.
    fn layout_message(&self, to: &Fingerprint) -> Option<Message> {
        let file = layout_file::load(&self.dir).ok()?;
        let desk = self
            .node
            .status()
            .into_iter()
            .find(|p| p.peer.fingerprint == *to)
            .and_then(|p| p.link)
            .is_some_and(|link| layout_file::knows_desk(&link.app_version));
        let me = (self.node.fingerprint(), self.node.name().to_string());
        let paired = self.node.paired();
        Some(layout_file::message(&file, desk, self.resolver(), |fp| {
            if *fp == me.0 {
                return Some(me.1.clone());
            }
            paired.iter().find(|p| p.fingerprint == *fp).map(|p| p.name.clone())
        }))
    }

    /// Sends the arrangement to `to`.
    fn share_layout_with(&self, to: &Fingerprint) {
        if let Some(message) = self.layout_message(to) {
            self.node.send(to, message);
        }
    }

    /// Sends the arrangement to every connected computer except `except`.
    fn share_layout(&self, except: Option<Fingerprint>) {
        for peer in self.node.status().into_iter().filter(|p| p.link.is_some()) {
            if Some(peer.peer.fingerprint) != except {
                self.share_layout_with(&peer.peer.fingerprint);
            }
        }
    }

    /// An arrangement from `from`: taken over if newer, then passed on.
    fn layout_received(&self, from: Fingerprint, message: Message) {
        let (revision, editor, arrangement, names) = match message {
            Message::Layout {
                revision,
                editor,
                placements,
                names,
            } => (revision, editor, layout_file::Arrangement::Neighbors(placements), names),
            Message::Tiles {
                revision,
                editor,
                tiles,
                names,
            } => (revision, editor, layout_file::Arrangement::Tiles(tiles), names),
            Message::Desk {
                revision,
                editor,
                areas,
                names,
            } => (revision, editor, layout_file::Arrangement::Areas(areas), names),
            _ => return,
        };
        let result = layout_file::load(&self.dir).and_then(|mut file| {
            let newer = layout_file::adopt(&mut file, revision, editor, arrangement, names);
            if newer {
                layout_file::save(&self.dir, &file)?;
            }
            Ok(newer)
        });
        match result {
            Ok(true) => {
                tracing::info!("arrangement taken over from {}", self.name_of(&from.to_string()));
                self.reload_layout();
                self.refresh();
                self.share_layout(Some(from));
            }
            Ok(false) => {}
            Err(e) => tracing::warn!("layout: {e}"),
        }
    }

    /// This computer's screens changed: new links get them, connected peers that understand it
    /// are told at once (older ones only after reconnecting).
    fn screens_changed(&self, screens: Vec<Screen>) {
        tracing::info!("screens of this computer changed: {screens:?}");
        self.node.set_screens(screens.clone());
        for peer in self.node.status() {
            if peer
                .link
                .as_ref()
                .is_some_and(|link| layout_file::knows_screens(&link.app_version))
            {
                self.node.send(
                    &peer.peer.fingerprint,
                    Message::Screens {
                        screens: screens.clone(),
                    },
                );
            }
        }
    }

    /// Whether the peer with this machine id runs a version for which `knows` holds.
    fn peer_knows(&self, id: &str, knows: fn(&str) -> bool) -> bool {
        self.node
            .status()
            .iter()
            .find(|p| p.peer.fingerprint.to_string() == id)
            .and_then(|p| p.link.as_ref())
            .is_some_and(|link| knows(&link.app_version))
    }

    /// Tells the physical size of this computer's screens to `to`, or to every connected peer,
    /// where the peer understands it.
    fn send_sizes(&self, to: Option<&Fingerprint>) {
        let mm = self.status.lock().expect("status lock").screen_sizes.clone();
        if mm.is_empty() {
            return;
        }
        for peer in self.node.status() {
            if to.is_some_and(|to| *to != peer.peer.fingerprint) {
                continue;
            }
            if peer
                .link
                .as_ref()
                .is_some_and(|link| layout_file::knows_sizes(&link.app_version))
            {
                self.node
                    .send(&peer.peer.fingerprint, Message::ScreenSizes { mm: mm.clone() });
            }
        }
    }

    /// Works out the sizes this computer tells about its screens (see `screen_size::effective`):
    /// when they change, the peers hear of it, and when they or the screens change, this
    /// computer's area on the desk is measured anew a moment later.
    fn update_sizes(&self, screens_changed: bool) {
        let others: Vec<(Screen, (u32, u32))> = {
            let sizes = self.peer_sizes.lock().expect("sizes lock");
            self.node
                .status()
                .into_iter()
                .filter_map(|p| Some((p.link?.screens, sizes.get(&p.peer.fingerprint)?.clone())))
                .flat_map(|(screens, mm)| screens.into_iter().zip(mm))
                .collect()
        };
        let (reported, typed) = self.reported_sizes.lock().expect("sizes lock").clone();
        let mut status = self.status.lock().expect("status lock");
        let sizes = screen_size::effective(&status.screens, &reported, &typed, &others);
        status.size_unsure = screen_size::any_implausible(&status.screens, &reported);
        status.reported_sizes = reported.clone();
        let changed = sizes != status.screen_sizes;
        status.screen_sizes = sizes.clone();
        drop(status);
        if changed {
            tracing::info!("screen sizes told to the others (mm): {sizes:?}, reported: {reported:?}");
            self.send_sizes(None);
        }
        if changed || screens_changed {
            *self.measure_at.lock().expect("measure lock") = Some(Instant::now() + MEASURE_AFTER);
        }
    }

    /// Gives this computer's area on the desk the size of its screens now, and computers used in
    /// turn at the same place along with it; the others move along so the desk keeps its shape.
    /// Only each computer measures itself, so two of them never take turns at changing a size.
    fn measure_me(&self) {
        let (screens, mm) = {
            let status = self.status.lock().expect("status lock");
            (status.screens.clone(), status.screen_sizes.clone())
        };
        let Some(size) = screen_size::desk_size(&screens, &mm) else {
            return;
        };
        let file = match layout_file::load(&self.dir) {
            Ok(file) => file,
            Err(e) => return tracing::warn!("layout: {e}"),
        };
        let me = layout_file::machine_id(&self.node.fingerprint());
        let resolve = self.resolver();
        let desk = layout_file::desk(&file, |entry| {
            resolve(entry).map_or_else(|| entry.to_string(), |fp| layout_file::machine_id(&fp))
        });
        let Some(mine) = desk.iter().find(|a| a.machine == me).cloned() else {
            return;
        };
        if (mine.w - size.0).abs() < 0.005 && (mine.h - size.1).abs() < 0.005 {
            return;
        }
        let here: Vec<String> = desk
            .iter()
            .filter(|a| (a.x - mine.x).abs() < 1e-3 && (a.y - mine.y).abs() < 1e-3)
            .map(|a| a.machine.clone())
            .collect();
        let areas: Vec<Area> = tc_core::resized(&desk, |a| here.contains(&a.machine).then_some(size), &me)
            .into_iter()
            .map(|a| Area {
                measured: a.measured || here.contains(&a.machine),
                ..a
            })
            .collect();
        tracing::info!(
            "arrangement: this computer measured anew, {:.2} × {:.2} instead of {:.2} × {:.2} (thousand points)",
            size.0,
            size.1,
            mine.w,
            mine.h
        );
        self.edit_layout(|file| file.set_areas(areas));
    }

    /// Keeps the pointer on the computer it is on, or lets it go again (`None`: the other way round).
    fn toggle_pin(&self, pin: Option<bool>) {
        let mut engine = self.engine.lock().expect("engine lock");
        let pin = pin.unwrap_or(!engine.pinned());
        if pin == engine.pinned() {
            return;
        }
        engine.set_pinned(pin);
        drop(engine);
        tracing::info!("pointer {}", if pin { "pinned to this computer" } else { "free again" });
        self.refresh();
        self.changed();
    }

    /// Rebuilds the status snapshot from node, engine and input.
    fn refresh(&self) {
        let file = self.status.lock().expect("status lock").layout.clone();
        let me = self.node.fingerprint();
        let sizes = self.peer_sizes.lock().expect("sizes lock").clone();
        let peers: Vec<PeerView> = self
            .node
            .status()
            .into_iter()
            .map(|s| PeerView {
                side: layout_file::side_of(&file, &me, self.node.name(), &s.peer.fingerprint, &s.peer.name),
                swap_cmd_ctrl: layout_file::swaps(&file, &s.peer.fingerprint, &s.peer.name),
                scroll: layout_file::scroll_of(&file, &s.peer.fingerprint, &s.peer.name),
                screen_sizes: sizes.get(&s.peer.fingerprint).cloned().unwrap_or_default(),
                fingerprint: s.peer.fingerprint,
                name: s.peer.name,
                link: s.link,
                reconnects: s.reconnects,
                refusal: s.refusal,
            })
            .collect();
        let paired: BTreeSet<Fingerprint> = peers.iter().map(|p| p.fingerprint).collect();
        let discovered = self
            .node
            .discovered()
            .into_iter()
            .filter(|d| !paired.contains(&d.fingerprint))
            .collect();
        let (role, pinned) = {
            let engine = self.engine.lock().expect("engine lock");
            (engine.role(), engine.pinned())
        };
        let (controlling, controlled_by) = match role {
            Role::Idle => (None, None),
            Role::Controlling(id) => (Some(self.name_of(&id)), None),
            Role::ControlledBy(id) => (None, Some(self.name_of(&id))),
        };
        let mut status = self.status.lock().expect("status lock");
        status.peers = peers;
        status.discovered = discovered;
        status.controlling = controlling;
        status.controlled_by = controlled_by;
        status.pinned = pinned;
        if status
            .pairing
            .as_ref()
            .is_some_and(|(_, until)| Instant::now() >= *until)
        {
            status.pairing = None;
        }
    }
}

/// Carries out the engine's actions. Grabs and ungrabs from different threads can arrive out of
/// order (a grab still under way while the target already took over): each one happens under the
/// engine lock and only if it still fits the role, otherwise the capture would keep pulling the
/// pointer back to its parking spot while nothing controls another computer.
fn execute(actions: Vec<Action>, node: &Node, engine: &Mutex<Engine>, inject: Option<&dyn Inject>) {
    let controlling = |engine: &Engine| matches!(engine.role(), Role::Controlling(_));
    for action in actions {
        let result = match (action, inject) {
            (Action::Send { to, message }, _) => {
                if let Ok(peer) = to.parse::<Fingerprint>() {
                    node.send(&peer, message);
                }
                Ok(())
            }
            (Action::Grab { park }, Some(inject)) => {
                let engine = engine.lock().expect("engine lock");
                if controlling(&engine) {
                    inject.grab(park)
                } else {
                    tracing::info!("a grab came too late: control is already back here");
                    Ok(())
                }
            }
            (Action::Ungrab { at }, Some(inject)) => {
                let engine = engine.lock().expect("engine lock");
                if controlling(&engine) {
                    tracing::info!("an ungrab came too late: another computer is controlled again");
                    Ok(())
                } else {
                    inject.ungrab(at)
                }
            }
            (Action::Inject(message), Some(inject)) => inject.inject(&message),
            (_, None) => Ok(()),
        };
        if let Err(e) = result {
            tracing::warn!("input: {e}");
        }
    }
}

/// The capture handler: keyboard shortcuts first, then the engine decides.
fn handler(shared: Arc<Shared>) -> tc_input::Handler {
    let mut held = BTreeSet::new();
    // State of the drag pasteboard when the left button went down
    let mut drag_before: Option<u64> = None;
    // The left button went over with dragged files
    let mut handed_over = false;
    // The last left button event seen here (for the log)
    let mut left_seen: Option<(Instant, bool)> = None;
    Box::new(move |event: Captured| {
        if let Captured::Button {
            button: MouseButton::Left,
            down,
        } = event
        {
            left_seen = Some((Instant::now(), down));
            if down {
                drag_before = tc_clipboard::drag::change_count();
                handed_over = false;
            } else {
                tc_clipboard::drag::released_here();
            }
            if !down && handed_over {
                // Cancelling the drag here makes macOS report a release of its own: only the real
                // one may drop the files over there
                if tc_input::system::left_button_held() == Some(true) {
                    tracing::info!("release while the button is still held ignored (the drag goes on)");
                    return false;
                }
                handed_over = false;
            }
        }
        if let Captured::Key { usage, down } = event {
            if down {
                held.insert(usage);
            } else {
                held.remove(&usage);
            }
            let prefix = PREFIX.iter().all(|(l, r)| held.contains(l) || held.contains(r));
            if down && prefix && usage == ESCAPE {
                shared.toggle_pin(Some(false));
                let actions = shared.engine.lock().expect("engine lock").release_everything();
                shared.execute(actions);
                if let Some(inject) = &shared.inject {
                    inject.release_all();
                }
                let _ = shared.internal.send(Internal::Notice(Notice::EmergencyRelease));
                return true;
            }
            if prefix && let Some(number) = DIGITS.iter().position(|u| *u == usage) {
                if down && let Some(target) = shared.numbered(number) {
                    let actions = shared.engine.lock().expect("engine lock").switch_to(&target);
                    shared.execute(actions);
                }
                return false;
            }
            if prefix && usage == KEY_F {
                if down {
                    shared.toggle_pin(None);
                }
                return false;
            }
            if prefix && usage == KEY_L {
                if down {
                    let _ = shared.internal.send(Internal::LockAll);
                }
                return false;
            }
            if prefix && let Some((_, side)) = ARROWS.iter().find(|(u, _)| *u == usage) {
                if down {
                    let actions = shared.engine.lock().expect("engine lock").switch_side(*side);
                    shared.execute(actions);
                }
                return false;
            }
        }
        let mut engine = shared.engine.lock().expect("engine lock");
        let controlled = matches!(engine.role(), Role::ControlledBy(_));
        let seen = controlled.then(|| event.clone());
        let mut actions = engine.captured(event);
        let grabbed = matches!(engine.role(), Role::Controlling(_));
        let corner = engine.take_corner_block();
        drop(engine);
        if let Some(block) = corner {
            tracing::info!(
                "no switch at the {:?} edge of {}: too close to a corner at ({:.0}, {:.0})",
                block.side,
                shared.name_of(&block.machine),
                block.at.x,
                block.at.y
            );
        }
        if let Some(seen) = seen
            && let Some(to) = actions.iter().find_map(|a| match a {
                Action::Send {
                    to,
                    message: Message::TakeOver,
                } => Some(to),
                _ => None,
            })
        {
            tracing::info!("local input takes control back from {}: {seen:?}", shared.name_of(to));
        }
        let mut dropped = Vec::new();
        let over_edge = dragged_over_edge(&actions);
        if !over_edge
            && actions.iter().any(|a| matches!(a, Action::Grab { .. }))
            && tc_input::system::left_button_held() == Some(true)
        {
            let last = match left_seen {
                Some((at, down)) => format!(
                    "the last one seen here was {} {} ms ago",
                    if down { "a press" } else { "a release" },
                    at.elapsed().as_millis()
                ),
                None => "none was seen here".into(),
            };
            tracing::info!("the pointer leaves with the left button held, but no press is held here ({last})");
        }
        if over_edge && drag_before.is_none() && tc_clipboard::drag::can_continue() {
            tracing::info!("the left button went over the edge; no drag was going on when it was pressed");
        }
        if let Some(before) = drag_before
            && over_edge
        {
            let files = tc_clipboard::drag::files_since(before);
            if files.is_empty() {
                tracing::info!(
                    "the left button went over the edge without dragged files (drag pasteboard {} since the press)",
                    if tc_clipboard::drag::change_count() == Some(before) {
                        "unchanged"
                    } else {
                        "changed, but no file URLs"
                    }
                );
            } else {
                // Esc cancels the drag here, so nothing lands at the screen edge; the files go over
                let esc = |down| Action::Inject(Message::Key { usage: ESCAPE, down });
                actions.splice(0..0, [esc(true), esc(false)]);
                dropped = files;
                // A computer that drags them on gets the button: letting go drops the files there
                let mut engine = shared.engine.lock().expect("engine lock");
                if let Role::Controlling(target) = engine.role() {
                    if shared.peer_knows(&target, layout_file::knows_drag) {
                        engine.hand_over_button(MouseButton::Left);
                        handed_over = true;
                        tracing::info!("files dragged over the edge, the button goes over with them");
                    } else {
                        tracing::info!("files dragged over the edge to an older version");
                    }
                }
            }
        }
        if actions.iter().any(|a| matches!(a, Action::Ungrab { .. })) {
            if tc_input::system::left_button_held() == Some(true) {
                drag_home(&shared, &mut actions);
            }
        } else if actions.iter().any(|a| {
            matches!(
                a,
                Action::Send {
                    message: Message::Enter { .. },
                    ..
                }
            )
        }) && tc_input::system::left_button_held() == Some(true)
        {
            drag_on(&shared, &mut actions);
        }
        execute(actions, &shared.node, &shared.engine, shared.inject.as_deref());
        // Announced only now: the pointer must arrive over there before the files do
        if !dropped.is_empty() {
            let _ = shared.internal.send(Internal::Dropped(dropped));
        }
        !grabbed
    })
}

/// The pointer comes back with the left button held: instead of letting go there, the computer
/// it comes from offers what it drags (if it does), and the drag goes on here.
fn drag_home(shared: &Shared, actions: &mut [Action]) {
    let release = Message::MouseButton {
        button: MouseButton::Left,
        down: false,
    };
    for action in actions {
        if let Action::Send { to, message } = action
            && *message == release
            && shared.peer_knows(to, layout_file::knows_drag_home)
        {
            tracing::info!(
                "back from {} with the left button held: files dragged there come along",
                shared.name_of(to)
            );
            *message = Message::DragHome;
            shared.files.lock().expect("files lock").drag_home = Some((to.clone(), Instant::now()));
        }
    }
}

/// The pointer goes from one controlled computer to the next with the left button held: the one
/// it leaves offers what it drags (if it does), the next one drags it on, with the button.
fn drag_on(shared: &Shared, actions: &mut Vec<Action>) {
    let release = Message::MouseButton {
        button: MouseButton::Left,
        down: false,
    };
    let left = actions.iter().find_map(|a| match a {
        Action::Send { to, message } if *message == release => Some(to.clone()),
        _ => None,
    });
    let next = actions.iter().position(|a| {
        matches!(
            a,
            Action::Send {
                message: Message::Enter { .. },
                ..
            }
        )
    });
    let (Some(left), Some(at)) = (left, next) else { return };
    let Action::Send { to: next, .. } = &actions[at] else {
        return;
    };
    let next = next.clone();
    if next == left
        || !shared.peer_knows(&left, layout_file::knows_drag_home)
        || !shared.peer_knows(&next, layout_file::knows_drag_from)
    {
        return;
    }
    tracing::info!(
        "from {} to {} with the left button held: files dragged there go on",
        shared.name_of(&left),
        shared.name_of(&next)
    );
    for action in actions.iter_mut() {
        if let Action::Send { to, message } = action
            && *to == left
            && *message == release
        {
            *message = Message::DragHome;
        }
    }
    actions.insert(
        at + 1,
        Action::Send {
            to: next,
            message: Message::DragFrom { from: left },
        },
    );
    // Letting go here drops the files over there
    shared
        .engine
        .lock()
        .expect("engine lock")
        .hand_over_button(MouseButton::Left);
}

/// The pointer left this computer with the left button held (the button is let go at the edge).
fn dragged_over_edge(actions: &[Action]) -> bool {
    actions.iter().any(|a| matches!(a, Action::Grab { .. }))
        && actions.iter().any(|a| {
            matches!(
                a,
                Action::Inject(Message::MouseButton {
                    button: MouseButton::Left,
                    down: false
                })
            )
        })
}

pub struct Session;

impl Session {
    /// Starts node, input (if a backend is given) and the loop on the current Tokio runtime.
    pub async fn start(
        config: SessionConfig,
        backend: Option<Backend>,
        clipboard: Option<Clipboard>,
        on_change: Option<OnChange>,
    ) -> Result<(SessionHandle, JoinHandle<()>)> {
        let identity = Identity::load_or_create(&config.dir)?;
        let store = PeerStore::load(&config.dir)?;
        let screens = backend.as_ref().map(|b| b.inject.screens()).unwrap_or_default();
        let mut node_config = NodeConfig::new(config.name.clone());
        node_config.bind.set_port(config.port);
        node_config.discovery = config.discovery;
        node_config.screens = screens.clone();
        let (node, events) = Node::start(node_config, identity, store).await?;
        // Files fetched for pasting last time are no longer on the clipboard
        let _ = std::fs::remove_dir_all(files::promise_dir());
        Self::with_node(node, events, config.dir, backend, clipboard, on_change).await
    }

    /// Like `start`, with a node that is already running (tests).
    pub async fn with_node(
        node: Node,
        events: mpsc::UnboundedReceiver<Event>,
        dir: PathBuf,
        backend: Option<Backend>,
        clipboard: Option<Clipboard>,
        on_change: Option<OnChange>,
    ) -> Result<(SessionHandle, JoinHandle<()>)> {
        let screens = backend.as_ref().map(|b| b.inject.screens()).unwrap_or_default();
        let screen_sizes = backend.as_ref().map(|b| b.inject.sizes()).unwrap_or_default();
        tracing::info!("screens of this computer: {screens:?}, physical sizes (mm): {screen_sizes:?}");
        let (inject, capture) = match backend {
            Some(b) => (Some(b.inject), Some(b.capture)),
            None => (None, None),
        };
        let status = Arc::new(Mutex::new(Status {
            name: node.name().to_string(),
            fingerprint: Some(node.fingerprint()),
            port: node.local_addr().port(),
            screens: screens.clone(),
            screen_sizes: screen_sizes.clone(),
            ..Status::default()
        }));
        let (commands_tx, commands) = mpsc::unbounded_channel();
        let (internal_tx, internal) = mpsc::unbounded_channel();
        let (notices, _) = broadcast::channel(256);
        let engine = Arc::new(Mutex::new(Engine::new(
            node.fingerprint().to_string(),
            &screens,
            Layout::default(),
        )));
        let shared = Arc::new(Shared {
            node,
            engine,
            inject,
            status: status.clone(),
            notices: notices.clone(),
            internal: internal_tx,
            on_change,
            dir,
            clipboard,
            clip: Mutex::new(ClipSync::default()),
            files: Mutex::new(FileState::default()),
            logs: Mutex::new(LogState::default()),
            peer_sizes: Mutex::new(HashMap::new()),
            reported_sizes: Mutex::new((screen_sizes, Vec::new())),
            measure_at: Mutex::new(None),
            quiet_radio: AtomicBool::new(false),
            follow_layout: AtomicBool::new(false),
            keyboard: Arc::new(Mutex::new(KeyboardSync::default())),
            login_screen: Mutex::new(None),
            login_access: Mutex::new(None),
        });
        shared.reload_layout();
        shared.update_sizes(true);
        shared.refresh();
        let handle = SessionHandle {
            commands: commands_tx,
            status,
            notices,
        };
        let task = tokio::spawn(run(shared, capture, events, commands, internal));
        Ok((handle, task))
    }
}

fn start_capture(shared: &Arc<Shared>, capture: &mut Option<Box<dyn Capture>>) {
    let Some(capture) = capture.as_mut() else {
        return;
    };
    if capture.single_push_at_edges() {
        shared.engine.lock().expect("engine lock").set_single_push();
    }
    let state = match capture.start(handler(shared.clone())) {
        Ok(()) => InputState::Running,
        Err(e) => {
            shared.notice(Notice::InputFailed(e.to_string()));
            InputState::Failed(e.to_string())
        }
    };
    shared.status.lock().expect("status lock").input = state;
    shared.changed();
}

/// Holds the keep-awake request on a thread of its own: asking the power management can take
/// seconds on a busy computer, and the session loop must not stand still that long (the links
/// would take it for sleep).
struct Awake(std::sync::mpsc::Sender<bool>);

impl Awake {
    fn start() -> Self {
        let (wanted, requests) = std::sync::mpsc::channel::<bool>();
        std::thread::spawn(move || {
            let mut held = None;
            // Ends with the session; the request goes with it
            while let Ok(on) = requests.recv() {
                if !on {
                    held = None;
                } else if held.is_none() {
                    held = tc_input::system::KeepAwake::new();
                }
            }
        });
        Self(wanted)
    }

    fn set(&self, on: bool) {
        let _ = self.0.send(on);
    }
}

async fn run(
    shared: Arc<Shared>,
    mut capture: Option<Box<dyn Capture>>,
    mut events: mpsc::UnboundedReceiver<Event>,
    mut commands: mpsc::UnboundedReceiver<Command>,
    mut internal: mpsc::UnboundedReceiver<Internal>,
) {
    start_capture(&shared, &mut capture);
    let mut role = Role::Idle;
    // Both computers stay awake while one controls the other
    let awake = Awake::start();
    let mut problems: Vec<Problem> = Vec::new();
    let mut radio_quiet = false;
    let mut login_applied: Option<bool> = None;
    let mut access_check: Option<Instant> = None;
    let mut login_access_check: Option<Instant> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut status_tick = tokio::time::interval(STATUS_EVERY);
    let mut screen_tick = tokio::time::interval(Duration::from_secs(2));
    let mut clip_tick = tokio::time::interval(Duration::from_millis(300));
    let mut log_tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { break };
                let mut pending = Some(event);
                while let Some(event) = pending.take() {
                    // Positions are absolute: when several wait, only the newest is injected, so
                    // a flood from the other computer cannot clog the local input queue
                    if is_mouse_move(&event)
                        && let Ok(next) = events.try_recv()
                    {
                        if !(is_mouse_move(&next) && same_peer(&event, &next)) {
                            handle_event(&shared, event);
                        }
                        pending = Some(next);
                        continue;
                    }
                    handle_event(&shared, event);
                }
            }
            command = commands.recv() => {
                match command {
                    None | Some(Command::Shutdown) => break,
                    Some(Command::CancelTransfer(id)) => {
                        shared.files.lock().expect("files lock").cancels.cancel(id);
                    }
                    Some(Command::RetryInput) => {
                        if !matches!(shared.status.lock().expect("status lock").input, InputState::Running) {
                            start_capture(&shared, &mut capture);
                        }
                    }
                    Some(command) => handle_command(&shared, command),
                }
            }
            Some(message) = internal.recv() => match message {
                Internal::Notice(notice) => shared.notice(notice),
                Internal::OwnLayout(target, layout) => {
                    let still = shared.engine.lock().expect("engine lock").role() == Role::Controlling(target.to_string());
                    let mut keyboard = shared.keyboard.lock().expect("keyboard lock");
                    if still && keyboard.sent.as_ref() != Some(&layout) {
                        tracing::info!("keyboard layout {} ({}) goes to {}", layout.name, layout.id, shared.name_of(&target.to_string()));
                        keyboard.sent = Some(layout.clone());
                        drop(keyboard);
                        shared.node.send(&target, Message::KeyboardLayout { layout });
                    }
                }
                Internal::Accessibility(granted) => {
                    access_check = Some(Instant::now());
                    let failed = matches!(shared.status.lock().expect("status lock").input, InputState::Failed(_));
                    if granted && failed {
                        tracing::info!("Accessibility is granted now, starting input capture again");
                        start_capture(&shared, &mut capture);
                        if matches!(shared.status.lock().expect("status lock").input, InputState::Failed(_)) {
                            tracing::info!("Accessibility only reaches a new process: TableCross has to start anew");
                            shared.status.lock().expect("status lock").restart_needed = true;
                            shared.changed();
                        }
                    }
                }
                Internal::ClipRead(read) => clip_read(&shared, read),
                Internal::FilesListed(listing) => files_listed(&shared, listing, false),
                Internal::Dropped(paths) => {
                    let internal = shared.internal.clone();
                    tokio::task::spawn_blocking(move || {
                        let listing = files::list(&paths).map_err(|e| e.to_string());
                        let _ = internal.send(Internal::DropListed(listing));
                    });
                }
                Internal::DropListed(listing) => files_listed(&shared, listing, true),
                Internal::ClipWritten(from, count) => {
                    shared.clip.lock().expect("clip lock").write_done(count);
                    if count.is_some() {
                        shared.notice(Notice::ClipboardReceived(shared.name_of(&from.to_string())));
                    }
                }
                Internal::LockAll => lock_all(&shared),
                Internal::Fetch(id) => fetch_promised(&shared, id),
                Internal::DragGaveUp(id) => {
                    let offered = shared.files.lock().expect("files lock").remote.as_ref().is_some_and(|r| r.1 == id);
                    if offered {
                        tracing::info!("files dragged over the edge go to Downloads: the drag did not start here");
                        request_files(&shared);
                    }
                }
                Internal::PairDone(result) => {
                    shared.status.lock().expect("status lock").pairing_busy = false;
                    match result {
                        Ok(name) => shared.notice(Notice::Paired(name)),
                        Err(e) => shared.notice(Notice::PairError(e)),
                    }
                    shared.reload_layout();
                }
            },
            _ = tick.tick() => {
                let actions = shared.engine.lock().expect("engine lock").tick();
                shared.execute(actions);
                let now = shared.engine.lock().expect("engine lock").role();
                if now != role {
                    let arrived = matches!(now, Role::ControlledBy(_))
                        || (now == Role::Idle && matches!(role, Role::Controlling(_)));
                    if arrived {
                        request_dropped(&shared);
                    }
                    if !matches!(now, Role::Controlling(_)) {
                        let request = shared.clip.lock().expect("clip lock").became_active();
                        if let Some((peer, message)) = request {
                            shared.node.send(&peer, message);
                        }
                    }
                    let notice = match &now {
                        Role::Idle => Notice::Local,
                        Role::Controlling(id) => Notice::Controlling(shared.name_of(id)),
                        Role::ControlledBy(id) => Notice::ControlledBy(shared.name_of(id)),
                    };
                    awake.set(now != Role::Idle);
                    shared.node.set_brisk(now != Role::Idle);
                    if matches!(role, Role::ControlledBy(_)) {
                        layout_back(&shared);
                    }
                    shared.keyboard.lock().expect("keyboard lock").sent = None;
                    role = now;
                    quiet_radio(&shared, &capture, &role, &mut radio_quiet);
                    read_own_layout(&shared, &role, true);
                    shared.refresh();
                    shared.notice(notice);
                }
            }
            _ = status_tick.tick() => {
                shared.refresh();
                quiet_radio(&shared, &capture, &role, &mut radio_quiet);
                check_access(&shared, &mut access_check);
                read_own_layout(&shared, &role, false);
                login_screen(&shared, &capture, &mut login_applied);
                check_login_access(&shared, &mut login_access_check);
                if let Some(capture) = &capture {
                    #[allow(unused_mut)]
                    let mut health = capture.health();
                    #[cfg(target_os = "macos")]
                    if crate::helper_install::outdated() && !health.problems.contains(&Problem::HelperOutdated) {
                        health.problems.retain(|p| !matches!(p, Problem::HelperMissing));
                        health.problems.push(Problem::HelperOutdated);
                    }
                    let login_on = *shared.login_screen.lock().expect("login lock") == Some(true);
                    if login_on && *shared.login_access.lock().expect("login access lock") == Some(false) {
                        health.problems.push(Problem::LoginAgentNoAccess);
                    }
                    for problem in health.problems.iter().filter(|p| !problems.contains(p)) {
                        shared.notice(Notice::Problem(problem.clone()));
                    }
                    problems = health.problems.clone();
                    shared.status.lock().expect("status lock").problems = health.problems;
                }
                shared.changed();
            }
            _ = clip_tick.tick() => clip_poll(&shared),
            _ = log_tick.tick() => send_log_lines(&shared),
            _ = screen_tick.tick() => {
                if let Some(inject) = &shared.inject {
                    let now = inject.screens();
                    let mut status = shared.status.lock().expect("status lock");
                    if !now.is_empty() && now != status.screens {
                        status.screens = now.clone();
                        drop(status);
                        let reported = inject.sizes();
                        tracing::info!("physical screen sizes (mm): {reported:?}");
                        shared.reported_sizes.lock().expect("sizes lock").0 = reported;
                        shared.engine.lock().expect("engine lock").set_local_screens(&now);
                        shared.screens_changed(now);
                        shared.update_sizes(true);
                    }
                }
                let due = shared.measure_at.lock().expect("measure lock").take_if(|at| *at <= Instant::now());
                if due.is_some() {
                    shared.measure_me();
                }
            }
        }
    }
    let actions = shared.engine.lock().expect("engine lock").release_everything();
    shared.execute(actions);
    if let Some(inject) = &shared.inject {
        inject.release_all();
    }
    if let Some(capture) = capture.as_mut() {
        if radio_quiet {
            capture.quiet_radio(false);
        }
        capture.stop();
    }
    shared.node.shutdown().await;
}

/// Controlling another computer that understands it: read the layout here when the pointer
/// arrives there (`now`) and every few seconds after, so the other computer can follow it.
fn read_own_layout(shared: &Arc<Shared>, role: &Role, now: bool) {
    const EVERY: Duration = Duration::from_secs(2);
    let Role::Controlling(target) = role else { return };
    let Ok(target) = target.parse::<Fingerprint>() else {
        return;
    };
    let knows = shared
        .node
        .status()
        .into_iter()
        .find(|p| p.peer.fingerprint == target)
        .and_then(|p| p.link)
        .is_some_and(|link| layout_file::knows_keyboard(&link.app_version));
    let mut keyboard = shared.keyboard.lock().expect("keyboard lock");
    if !knows || (!now && keyboard.read.is_some_and(|t| t.elapsed() < EVERY)) {
        return;
    }
    keyboard.read = Some(Instant::now());
    drop(keyboard);
    let internal = shared.internal.clone();
    tokio::task::spawn_blocking(move || {
        if let Some(layout) = tc_input::keyboard_layout::current() {
            let _ = internal.send(Internal::OwnLayout(target, layout));
        }
    });
}

/// The computer that controls this one types with `layout`: switch to the one here that types
/// alike, remembering the one in use to go back to.
fn follow_layout(shared: &Arc<Shared>, from: Fingerprint, layout: tc_protocol::KeyboardLayout) {
    if !shared.follow_layout.load(Ordering::SeqCst)
        || shared.engine.lock().expect("engine lock").role() != Role::ControlledBy(from.to_string())
    {
        return;
    }
    let keyboard = shared.keyboard.clone();
    let internal = shared.internal.clone();
    let name = shared.name_of(&from.to_string());
    tokio::task::spawn_blocking(move || {
        use tc_input::keyboard_layout::{alike, best_match, current, enabled, select};
        let Some(here) = current() else { return };
        if alike(&here, &layout) {
            return;
        }
        let available = enabled();
        let Some(fitting) = best_match(&layout, &available) else {
            let mut keyboard = keyboard.lock().expect("keyboard lock");
            if keyboard.missing.as_deref() != Some(layout.id.as_str()) {
                keyboard.missing = Some(layout.id.clone());
                let shown = if layout.name.is_empty() {
                    layout.language.clone()
                } else {
                    layout.name.clone()
                };
                let _ = internal.send(Internal::Notice(Notice::LayoutMissing(name, shown)));
            }
            return;
        };
        if select(fitting) {
            tracing::info!("keyboard layout {} like on {name} (was {})", fitting.name, here.name);
            keyboard.lock().expect("keyboard lock").before.get_or_insert(here);
        } else {
            tracing::warn!("cannot switch to the keyboard layout {}", fitting.id);
        }
    });
}

/// Control from the other computer ended: the layout in use before comes back.
fn layout_back(shared: &Arc<Shared>) {
    let Some(before) = shared.keyboard.lock().expect("keyboard lock").before.take() else {
        return;
    };
    tokio::task::spawn_blocking(move || {
        if tc_input::keyboard_layout::select(&before) {
            tracing::info!("keyboard layout {} again", before.name);
        }
    });
}

/// Tells the system helper whether this Mac is to be controlled on the login screen, once the app
/// said so and whenever it changes.
fn login_screen(shared: &Shared, capture: &Option<Box<dyn Capture>>, applied: &mut Option<bool>) {
    let wanted = *shared.login_screen.lock().expect("login lock");
    let (Some(want), Some(capture)) = (wanted, capture) else {
        return;
    };
    if *applied == Some(want) {
        return;
    }
    let agent = want.then(|| tc_input::helper_protocol::LoginAgent {
        name: shared.node.name().to_string(),
        port: shared.node.local_addr().port(),
    });
    capture.login_screen(agent);
    *applied = Some(want);
}

/// AWDL pauses while the pointer is shared and the setting asks for it.
fn quiet_radio(shared: &Shared, capture: &Option<Box<dyn Capture>>, role: &Role, quiet: &mut bool) {
    let want = shared.quiet_radio.load(Ordering::SeqCst) && *role != Role::Idle;
    if want == *quiet {
        return;
    }
    if let Some(capture) = capture {
        tracing::info!(
            "{}",
            if want {
                "pausing AWDL while the pointer is shared"
            } else {
                "AWDL may run again"
            }
        );
        capture.quiet_radio(want);
    }
    *quiet = want;
}

/// While capture failed: ask a fresh process every few seconds whether Accessibility was granted
/// meanwhile (this process may keep the old answer).
fn check_access(shared: &Arc<Shared>, last: &mut Option<Instant>) {
    const EVERY: Duration = Duration::from_secs(2);
    let status = shared.status.lock().expect("status lock");
    let waiting = matches!(status.input, InputState::Failed(_)) && !status.restart_needed;
    drop(status);
    if !waiting || last.is_some_and(|t| t.elapsed() < EVERY) {
        return;
    }
    // Not again until the answer is in
    *last = Some(Instant::now() + Duration::from_secs(3600));
    let internal = shared.internal.clone();
    tokio::task::spawn_blocking(move || {
        if let Some(granted) = tc_input::system::accessibility_fresh() {
            let _ = internal.send(Internal::Accessibility(granted));
        }
    });
}

/// macOS: while control on the login screen is on, asks now and then whether its program has the
/// Accessibility permission; without it macOS drops all its input there silently.
fn check_login_access(shared: &Arc<Shared>, last: &mut Option<Instant>) {
    const EVERY: Duration = Duration::from_secs(5);
    let on = *shared.login_screen.lock().expect("login lock") == Some(true);
    let program = std::path::Path::new(tc_input::helper_protocol::LOGIN_BINARY);
    if !cfg!(target_os = "macos") || !on || !program.exists() {
        *shared.login_access.lock().expect("login access lock") = None;
        return;
    }
    if last.is_some_and(|t| t.elapsed() < EVERY) {
        return;
    }
    *last = Some(Instant::now());
    let shared = shared.clone();
    tokio::task::spawn_blocking(move || {
        let granted = tc_input::system::accessibility_of(std::path::Path::new(tc_input::helper_protocol::LOGIN_BINARY));
        let mut known = shared.login_access.lock().expect("login access lock");
        if *known != granted {
            tracing::info!("Accessibility permission of the login screen program: {granted:?}");
        }
        *known = granted;
    });
}

/// New lines of our log to the computers that collect it.
fn send_log_lines(shared: &Arc<Shared>) {
    let mut outgoing = Vec::new();
    for (peer, position) in &mut shared.logs.lock().expect("logs lock").subscribers {
        outgoing.push((*peer, peer_logs::since(position)));
    }
    for (peer, texts) in outgoing {
        for text in texts {
            shared.node.send(&peer, Message::LogLines { text });
        }
    }
}

fn files_offer(id: u64, listing: &Listing, dropped: bool) -> Message {
    Message::FilesOffer {
        id,
        names: listing.offered_names(),
        count: listing.count(),
        size: listing.size(),
        dropped,
    }
}

/// Files were copied here (or dragged over the edge): announce them to everyone connected.
fn files_listed(shared: &Arc<Shared>, listing: Result<Listing, String>, dropped: bool) {
    let listing = match listing {
        Ok(l) if !l.entries.is_empty() => l,
        Ok(_) => return,
        Err(e) => {
            tracing::warn!("cannot read the copied files: {e}");
            return;
        }
    };
    let offer = {
        let mut state = shared.files.lock().expect("files lock");
        state.counter += 1;
        let id = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64)
            ^ state.counter.rotate_left(40);
        let offer = files_offer(id, &listing, dropped);
        state.local = Some((id, Arc::new(listing)));
        state.remote = None;
        offer
    };
    shared.status.lock().expect("status lock").files_waiting = None;
    for peer in shared.node.status().into_iter().filter(|p| p.link.is_some()) {
        shared.node.send(&peer.peer.fingerprint, offer.clone());
    }
}

/// Puts files copied on a peer on this clipboard as a promise. Returns false where the clipboard
/// cannot hold a promise.
fn promise_files(shared: &Arc<Shared>, id: u64, items: usize) -> bool {
    let Some(clipboard) = &shared.clipboard else {
        return false;
    };
    if !clipboard.system.can_promise() {
        return false;
    }
    let fulfil = new_promise(shared, id);
    shared.clip.lock().expect("clip lock").writing();
    let system = clipboard.system.clone();
    let shared = shared.clone();
    tokio::task::spawn_blocking(move || {
        let count = match system.write_file_promise(items, fulfil) {
            Some(Ok(count)) => Some(count),
            Some(Err(e)) => {
                tracing::warn!("cannot put the copied files on the clipboard: {e}");
                None
            }
            None => None,
        };
        shared.clip.lock().expect("clip lock").write_done(count);
    });
    true
}

/// A promise of the files `id` (replacing the one before); the returned function fetches them.
fn new_promise(shared: &Arc<Shared>, id: u64) -> tc_clipboard::Fulfil {
    let promise = Arc::new(Promise {
        id,
        dir: files::promise_dir().join(format!("{id:x}")),
        state: Mutex::new(Promised::Offered),
        changed: Condvar::new(),
    });
    let old = shared
        .files
        .lock()
        .expect("files lock")
        .promise
        .replace(promise.clone());
    if let Some(old) = old {
        tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&old.dir));
    }
    let internal = shared.internal.clone();
    Arc::new(move || promise.fulfil(&internal))
}

/// Files dragged over the edge by the computer controlling this one: the drag goes on here, and
/// where they are dropped they are fetched. Returns false where it cannot (then they are fetched
/// into Downloads).
fn continue_drag(shared: &Arc<Shared>, controller: String, id: u64, names: &[String]) -> bool {
    let from = controller;
    let refused = if !tc_clipboard::drag::can_continue() {
        Some("not on this system")
    } else if names.is_empty() {
        Some("no names")
    } else if shared.engine.lock().expect("engine lock").role() != Role::ControlledBy(from.clone()) {
        Some("not controlled by the sender")
    } else if !shared.peer_knows(&from, layout_file::knows_drag) {
        Some("the sender's version does not hand the button over")
    } else if shared.files.lock().expect("files lock").left_up_seen {
        Some("the button was let go already")
    } else {
        None
    };
    if let Some(why) = refused {
        tracing::info!("files dragged over the edge go to Downloads: {why}");
        return false;
    }
    let fulfil = new_promise(shared, id);
    let shared_internal = shared.internal.clone();
    let shared = shared.clone();
    let press = Box::new(move || {
        // Let go meanwhile: nothing to drag
        if shared.files.lock().expect("files lock").left_up_seen {
            tracing::info!("the button was let go before the drag could start here");
            return;
        }
        tracing::info!("pressing the button here to go on with the drag");
        let down = Message::MouseButton {
            button: MouseButton::Left,
            down: true,
        };
        let actions = shared.engine.lock().expect("engine lock").received(&from, down);
        shared.execute(actions);
    });
    let internal = shared_internal;
    let gave_up = Box::new(move || {
        let _ = internal.send(Internal::DragGaveUp(id));
    });
    tracing::info!("files dragged over the edge, the drag goes on here");
    tc_clipboard::drag::continue_drag(names.to_vec(), fulfil, press, gave_up);
    true
}

/// The controlling computer took the pointer back with the left button held: files dragged here
/// since it pressed the button go over (the drag here is cancelled, the button let go after this).
fn drag_leaves(shared: &Arc<Shared>, peer: Fingerprint) {
    let from = peer.to_string();
    if shared.engine.lock().expect("engine lock").role() != Role::ControlledBy(from) {
        return;
    }
    let Some(before) = shared.files.lock().expect("files lock").remote_press.take() else {
        return;
    };
    let files = tc_clipboard::drag::files_since(before);
    if files.is_empty() {
        tracing::info!("the pointer went back with the left button held, no files dragged here");
        return;
    }
    tracing::info!(
        "{} file(s) dragged back to {}",
        files.len(),
        shared.name_of(&peer.to_string())
    );
    // Esc cancels the drag here, so nothing lands at the screen edge
    let esc = |down| Action::Inject(Message::Key { usage: ESCAPE, down });
    shared.execute(vec![esc(true), esc(false)]);
    let _ = shared.internal.send(Internal::Dropped(files));
}

/// Files dragged back from `peer` are offered: the drag goes on here while the button is still
/// held. Returns false where it cannot (then they are fetched into Downloads).
fn continue_drag_home(shared: &Arc<Shared>, id: u64, names: &[String]) -> bool {
    let refused = if !tc_clipboard::drag::can_continue() {
        Some("not on this system")
    } else if names.is_empty() {
        Some("no names")
    } else if tc_input::system::left_button_held() != Some(true) {
        Some("the button was let go already")
    } else {
        None
    };
    if let Some(why) = refused {
        tracing::info!("files dragged back go to Downloads: {why}");
        return false;
    }
    let fulfil = new_promise(shared, id);
    let internal = shared.internal.clone();
    let gave_up = Box::new(move || {
        let _ = internal.send(Internal::DragGaveUp(id));
    });
    tracing::info!("files dragged back, the drag goes on here");
    // The button is held here for real: nothing to press
    tc_clipboard::drag::continue_drag(names.to_vec(), fulfil, Box::new(|| {}), gave_up);
    true
}

/// Promised files are being pasted here: ask the peer for them.
fn fetch_promised(shared: &Arc<Shared>, id: u64) {
    let mut state = shared.files.lock().expect("files lock");
    let Some(promise) = state.promise.clone().filter(|p| p.id == id) else {
        return;
    };
    if !state.remote.as_ref().is_some_and(|r| r.1 == id) {
        drop(state);
        tracing::warn!("promised files are no longer offered");
        promise.set(Promised::Failed);
        return;
    }
    state.save_to = Some(promise.dir.clone());
    drop(state);
    tracing::info!("files pasted here, fetching them");
    request_files(shared);
}

/// Asks for files dragged over the edge from a peer (the pointer just arrived here). Copied files
/// wait until they are saved.
fn request_dropped(shared: &Arc<Shared>) {
    let state = shared.files.lock().expect("files lock");
    let dropped = state.remote.as_ref().is_some_and(|r| {
        // Dragged on here: they come when dropped
        r.5 && !state.promise.as_ref().is_some_and(|p| p.id == r.1)
    });
    drop(state);
    if dropped {
        request_files(shared);
    }
}

/// Asks for the files waiting on a peer.
fn request_files(shared: &Arc<Shared>) {
    let mut state = shared.files.lock().expect("files lock");
    let Some((peer, id, ..)) = state.remote else { return };
    if state.requested == Some(id) {
        return;
    }
    state.requested = Some(id);
    drop(state);
    shared.node.send(&peer, Message::FilesRequest { id });
}

fn update_transfer(shared: &Shared, id: u64, update: impl FnOnce(&mut TransferView)) {
    let mut status = shared.status.lock().expect("status lock");
    if let Some(t) = status.transfers.iter_mut().find(|t| t.id == id) {
        update(t);
    }
    drop(status);
    shared.changed();
}

fn add_transfer(shared: &Shared, view: TransferView) {
    let mut status = shared.status.lock().expect("status lock");
    status.transfers.retain(|t| t.id != view.id);
    status.transfers.push(view);
    if status.transfers.len() > 10 {
        status.transfers.remove(0);
    }
}

fn send_files(shared: &Arc<Shared>, peer: Fingerprint, id: u64) {
    let (listing, cancel) = {
        let mut state = shared.files.lock().expect("files lock");
        match &state.local {
            Some((ours, listing)) if *ours == id => {
                let listing = listing.clone();
                (listing, state.cancels.add(id))
            }
            _ => return,
        }
    };
    add_transfer(
        shared,
        TransferView {
            id,
            peer: shared.name_of(&peer.to_string()),
            incoming: false,
            label: listing.shown_names().join(", "),
            count: listing.count(),
            done: 0,
            total: listing.size(),
            state: TransferState::Running,
        },
    );
    let shared = shared.clone();
    tokio::spawn(async move {
        let progress_shared = shared.clone();
        let mut progress = Progress::new(move |done| update_transfer(&progress_shared, id, |t| t.done = done));
        let links = shared.peer_knows(&peer.to_string(), layout_file::knows_links);
        let result = transfer::send(&shared.node, &peer, id, &listing, links, &cancel, &mut progress).await;
        let cancelled = cancel.load(std::sync::atomic::Ordering::SeqCst);
        shared.files.lock().expect("files lock").cancels.finished(id);
        update_transfer(&shared, id, |t| {
            t.state = match &result {
                Ok(()) => TransferState::Done(None),
                Err(_) if cancelled => TransferState::Cancelled,
                Err(e) => TransferState::Failed(e.clone()),
            }
        });
    });
}

fn receive_files(shared: &Arc<Shared>, peer: Fingerprint, id: u64, stream: tc_net::InStream) {
    let (label, count, total, dropped, target, promise, cancel) = {
        let mut state = shared.files.lock().expect("files lock");
        match &state.remote {
            Some((from, newest, label, count, size, dropped)) if *from == peer && *newest == id => {
                let (label, count, size, dropped) = (label.clone(), *count, *size, *dropped);
                state.remote = None;
                let save_to = state.save_to.take();
                let promise = state
                    .promise
                    .clone()
                    .filter(|p| p.id == id && save_to.as_ref() == Some(&p.dir));
                let target = match (&promise, save_to) {
                    (Some(promise), _) => promise.dir.clone(),
                    (None, Some(dir)) if !dropped => dir,
                    _ => files::downloads(),
                };
                (label, count, size, dropped, target, promise, state.cancels.add(id))
            }
            _ => {
                drop(state);
                stream.cancel();
                return;
            }
        }
    };
    shared.status.lock().expect("status lock").files_waiting = None;
    if let Some(promise) = &promise {
        promise.set(Promised::Running);
    }
    let peer_name = shared.name_of(&peer.to_string());
    add_transfer(
        shared,
        TransferView {
            id,
            peer: peer_name.clone(),
            incoming: true,
            label,
            count,
            done: 0,
            total,
            state: TransferState::Running,
        },
    );
    let shared = shared.clone();
    tokio::spawn(async move {
        let progress_shared = shared.clone();
        let mut progress = Progress::new(move |done| update_transfer(&progress_shared, id, |t| t.done = done));
        let result = match tokio::fs::create_dir_all(&target).await {
            Ok(()) => transfer::receive(stream, id, &target, &cancel, &mut progress).await,
            Err(e) => Err(e.to_string()),
        };
        let cancelled = cancel.load(std::sync::atomic::Ordering::SeqCst);
        shared.files.lock().expect("files lock").cancels.finished(id);
        if let Some(promise) = &promise {
            match &result {
                Ok(placed) => promise.set(Promised::Done(placed.clone())),
                Err(_) => promise.set(Promised::Failed),
            }
        }
        match result {
            Ok(_) if promise.is_some() => {
                // Pasted: the file manager copies them from the hidden folder to where they belong
                update_transfer(&shared, id, |t| t.state = TransferState::Done(None));
                tracing::info!("{count} pasted file(s) from {peer_name} fetched");
            }
            Ok(placed) => {
                update_transfer(&shared, id, |t| t.state = TransferState::Done(Some(target.clone())));
                // Dragged files go on the clipboard as well; saved ones were only meant for the folder
                if dropped && let Some(clipboard) = &shared.clipboard {
                    shared.files.lock().expect("files lock").written = placed.clone();
                    let system = clipboard.system.clone();
                    let written = placed.clone();
                    let count = tokio::task::spawn_blocking(move || system.write_files(&written).ok())
                        .await
                        .ok()
                        .flatten();
                    shared.clip.lock().expect("clip lock").write_done(count);
                }
                if dropped {
                    tc_input::system::show_folder(&target);
                }
                shared.notice(Notice::FilesReceived(peer_name, count, target, dropped));
            }
            Err(_) if cancelled => update_transfer(&shared, id, |t| t.state = TransferState::Cancelled),
            Err(e) => {
                update_transfer(&shared, id, |t| t.state = TransferState::Failed(e.clone()));
                shared.notice(Notice::TransferFailed(peer_name, e));
            }
        }
    });
}

/// Writes content from a peer into the local clipboard (on a blocking thread).
fn clip_write(shared: &Arc<Shared>, peer: Fingerprint, action: Option<ClipAction>) {
    if let (Some(ClipAction::Write { content, .. }), Some(clipboard)) = (action, &shared.clipboard) {
        let text = content.text.as_ref().map_or(0, |t| t.chars().count());
        tracing::info!(
            "clipboard from {}: {} bytes as {:?}, text {text} characters",
            shared.name_of(&peer.to_string()),
            content.size(),
            content.formats()
        );
        let system = clipboard.system.clone();
        let internal = shared.internal.clone();
        tokio::task::spawn_blocking(move || {
            let count = system
                .write(&content)
                .map_err(|e| tracing::warn!("clipboard: {e}"))
                .ok();
            let _ = internal.send(Internal::ClipWritten(peer, count));
        });
    }
}

/// Looks whether the local clipboard changed and reads it if so.
fn clip_poll(shared: &Arc<Shared>) {
    let Some(clipboard) = &shared.clipboard else { return };
    let count = clipboard.system.change_count();
    if shared.clip.lock().expect("clip lock").poll(count) == Some(ClipAction::Read) {
        let system = clipboard.system.clone();
        let internal = shared.internal.clone();
        tokio::task::spawn_blocking(move || {
            let read = system.read().map_err(|e| tracing::debug!("clipboard: {e}")).ok();
            let _ = internal.send(Internal::ClipRead(read));
        });
    }
}

fn clip_read(shared: &Arc<Shared>, read: Option<tc_clipboard::Read>) {
    let Some(clipboard) = &shared.clipboard else { return };
    let mut sync = shared.clip.lock().expect("clip lock");
    let Some(read) = read else {
        sync.read_failed();
        return;
    };
    if let tc_clipboard::Read::Files(paths) = &read {
        sync.files_copied();
        drop(sync);
        if shared.files.lock().expect("files lock").written == *paths {
            return; // What a transfer just put there
        }
        let paths = paths.clone();
        let internal = shared.internal.clone();
        tokio::task::spawn_blocking(move || {
            let _ = internal.send(Internal::FilesListed(files::list(&paths).map_err(|e| e.to_string())));
        });
        return;
    }
    let (announcement, skip) = sync.read_done(read, clipboard.max_size);
    drop(sync);
    if let Some(skip) = skip {
        shared.notice(Notice::ClipboardSkipped(skip));
    }
    if let Some((offer, eager)) = announcement {
        if let Message::ClipboardOffer { formats, size, .. } = &offer {
            tracing::info!("clipboard changed here: {size} bytes as {formats:?}");
        }
        for peer in shared.node.status().into_iter().filter(|p| p.link.is_some()) {
            clip::announce(&shared.node, &peer.peer.fingerprint, &offer, eager.as_ref());
        }
    }
}

/// Locks this screen without blocking the loop.
fn lock_here() {
    std::thread::spawn(|| {
        if let Err(e) = tc_input::system::lock_screen() {
            tracing::warn!("cannot lock the screen: {e}");
        }
    });
}

/// Mouse and keyboard back home, then every connected computer and this one lock their screens.
fn lock_all(shared: &Arc<Shared>) {
    let actions = shared.engine.lock().expect("engine lock").release_everything();
    shared.execute(actions);
    for peer in shared.node.status().into_iter().filter(|p| p.link.is_some()) {
        shared.node.send(&peer.peer.fingerprint, Message::LockScreen);
    }
    shared.notice(Notice::LockedAll);
    lock_here();
}

/// Shuts this computer down without blocking the loop.
fn power_off_here() {
    std::thread::spawn(|| {
        if let Err(e) = tc_input::system::power_off() {
            tracing::warn!("cannot shut down: {e}");
        }
    });
}

/// Logs this computer's user out without blocking the loop.
fn log_out_here() {
    std::thread::spawn(|| {
        if let Err(e) = tc_input::system::log_out() {
            tracing::warn!("cannot log out: {e}");
        }
    });
}

/// Mouse and keyboard back home, then the user of every connected computer that knows how is
/// logged out; this one a moment later, so that the messages are out before TableCross is asked
/// to quit.
fn log_out_all(shared: &Arc<Shared>, this_one: bool) {
    let actions = shared.engine.lock().expect("engine lock").release_everything();
    shared.execute(actions);
    let (mut told, mut too_old) = (Vec::new(), Vec::new());
    for peer in shared.node.status() {
        let Some(link) = &peer.link else { continue };
        let name = shared.name_of(&peer.peer.fingerprint.to_string());
        // Older versions end the connection on a message they do not know
        if layout_file::knows_log_out(&link.app_version) {
            shared.node.send(&peer.peer.fingerprint, Message::LogOut);
            told.push(name);
        } else {
            too_old.push(name);
        }
    }
    tracing::info!(
        "logging out {} other computer(s), {} too old for it, this one: {this_one}",
        told.len(),
        too_old.len()
    );
    shared.notice(Notice::LoggingOut(told, too_old));
    if this_one {
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(1));
            log_out_here();
        });
    }
}

/// Mouse and keyboard back home, then every connected computer shuts down; this one a moment later,
/// so that the messages are out before TableCross is asked to quit.
fn power_off_all(shared: &Arc<Shared>, this_one: bool) {
    let actions = shared.engine.lock().expect("engine lock").release_everything();
    shared.execute(actions);
    let mut names = Vec::new();
    for peer in shared.node.status().into_iter().filter(|p| p.link.is_some()) {
        shared.node.send(&peer.peer.fingerprint, Message::PowerOff);
        names.push(shared.name_of(&peer.peer.fingerprint.to_string()));
    }
    tracing::info!("shutting down {} other computer(s), this one: {this_one}", names.len());
    shared.notice(Notice::PoweringOff(names));
    if this_one {
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(1));
            power_off_here();
        });
    }
}

fn is_mouse_move(event: &Event) -> bool {
    matches!(
        event,
        Event::Message {
            message: Message::MouseMove { .. },
            ..
        }
    )
}

fn same_peer(a: &Event, b: &Event) -> bool {
    matches!((a, b), (Event::Message { peer: p, .. }, Event::Message { peer: q, .. }) if p == q)
}

fn handle_event(shared: &Arc<Shared>, event: Event) {
    match event {
        Event::Connected {
            peer,
            name,
            screens,
            address,
        } => {
            let offer = shared.clip.lock().expect("clip lock").for_new_peer();
            if let Some((offer, eager)) = offer {
                clip::announce(&shared.node, &peer, &offer, eager.as_ref());
            }
            let files_offer = shared
                .files
                .lock()
                .expect("files lock")
                .local
                .as_ref()
                .map(|(id, l)| files_offer(*id, l, false));
            if let Some(offer) = files_offer {
                shared.node.send(&peer, offer);
            }
            shared
                .engine
                .lock()
                .expect("engine lock")
                .peer_online(&peer.to_string(), &screens);
            shared.reload_layout();
            shared.share_layout_with(&peer);
            shared.send_sizes(Some(&peer));
            shared.update_sizes(false);
            if shared.logs.lock().expect("logs lock").collect {
                shared.node.send(&peer, Message::LogSubscribe { on: true });
            }
            tracing::info!("screens of {name}: {screens:?}, reached at {address}");
            shared.notice(Notice::Connected(name));
        }
        Event::Disconnected { peer, reason } => {
            let actions = shared
                .engine
                .lock()
                .expect("engine lock")
                .peer_offline(&peer.to_string());
            shared.execute(actions);
            shared
                .logs
                .lock()
                .expect("logs lock")
                .subscribers
                .retain(|(p, _)| *p != peer);
            if !matches!(reason, Disconnect::Replaced) {
                shared.notice(Notice::Disconnected(shared.name_of(&peer.to_string()), reason));
            }
        }
        Event::Message {
            peer,
            message: Message::ClipboardOffer { id, size, .. },
        } => {
            shared.files.lock().expect("files lock").remote = None;
            shared.status.lock().expect("status lock").files_waiting = None;
            let active = !matches!(shared.engine.lock().expect("engine lock").role(), Role::Controlling(_));
            let mut sync = shared.clip.lock().expect("clip lock");
            let request = sync.offered(peer, id, size, active);
            let early = sync
                .take_early(peer, id)
                .and_then(|data| sync.received(peer, id, &data));
            drop(sync);
            if let Some(request) = request {
                shared.node.send(&peer, request);
            }
            clip_write(shared, peer, early);
        }
        Event::Message {
            peer,
            message:
                Message::FilesOffer {
                    id,
                    names,
                    count,
                    size,
                    dropped,
                },
        } => {
            shared.clip.lock().expect("clip lock").clear_remote();
            let label = files::label(&names);
            {
                let mut state = shared.files.lock().expect("files lock");
                state.local = None;
                state.remote = Some((peer, id, label.clone(), count, size, dropped));
                state.requested = None;
                state.save_to = None;
            }
            let pasteable = !dropped && promise_files(shared, id, names.len());
            shared.status.lock().expect("status lock").files_waiting = (!dropped).then(|| FilesWaiting {
                peer: shared.name_of(&peer.to_string()),
                label,
                count,
                size,
                pasteable,
            });
            // Dragged back from the computer the pointer just left
            const HOME_WAIT: Duration = Duration::from_secs(10);
            let home = dropped
                && shared
                    .files
                    .lock()
                    .expect("files lock")
                    .drag_home
                    .take_if(|(from, at)| *from == peer.to_string() && at.elapsed() < HOME_WAIT)
                    .is_some();
            // Dragged on from another computer the controlling one also controls
            let passed_on = dropped
                && shared
                    .files
                    .lock()
                    .expect("files lock")
                    .drag_from
                    .take_if(|(from, at)| *from == peer.to_string() && at.elapsed() < HOME_WAIT)
                    .is_some();
            let role = shared.engine.lock().expect("engine lock").role();
            let controller = match &role {
                Role::ControlledBy(by) if passed_on => by.clone(),
                _ => peer.to_string(),
            };
            if home {
                if !continue_drag_home(shared, id, &names) {
                    request_files(shared);
                }
            } else if dropped && !continue_drag(shared, controller, id, &names) && matches!(role, Role::ControlledBy(_))
            {
                request_files(shared);
            }
        }
        Event::Message {
            peer,
            message: Message::FilesRequest { id },
        } => send_files(shared, peer, id),
        Event::Stream { peer, header, stream } => {
            let Some(stream) = stream.take() else { return };
            match files::parse_header(&header) {
                Some(id) => receive_files(shared, peer, id, stream),
                None => stream.cancel(),
            }
        }
        Event::Message {
            peer,
            message: Message::ClipboardRequest { id },
        } => {
            let content = shared.clip.lock().expect("clip lock").requested(id);
            if let Some((header, data)) = content {
                shared.node.send_blob(&peer, header, data.as_ref().clone());
            }
        }
        Event::Blob { peer, header, data } => {
            let action = clip::parse_header(&header)
                .and_then(|id| shared.clip.lock().expect("clip lock").received(peer, id, &data));
            clip_write(shared, peer, action);
        }
        Event::Message {
            peer,
            message: message @ (Message::Layout { .. } | Message::Tiles { .. } | Message::Desk { .. }),
        } => shared.layout_received(peer, message),
        Event::Message {
            peer,
            message: Message::ScreenSizes { mm },
        } => {
            tracing::info!("screen sizes of {} (mm): {mm:?}", shared.name_of(&peer.to_string()));
            shared.peer_sizes.lock().expect("sizes lock").insert(peer, mm);
            shared.update_sizes(false);
            shared.refresh();
            shared.changed();
        }
        Event::Message {
            peer,
            message: Message::Screens { screens },
        } => {
            tracing::info!("screens of {} changed: {screens:?}", shared.name_of(&peer.to_string()));
            shared
                .engine
                .lock()
                .expect("engine lock")
                .peer_online(&peer.to_string(), &screens);
            shared.reload_layout();
            shared.update_sizes(false);
            shared.refresh();
            shared.changed();
        }
        Event::Message {
            peer,
            message: Message::KeyboardLayout { layout },
        } => follow_layout(shared, peer, layout),
        Event::Message {
            message: Message::ShowEdges { marks },
            ..
        } => {
            shared.status.lock().expect("status lock").edge_marks =
                (!marks.is_empty()).then(|| (marks, Instant::now()));
            shared.changed();
        }
        Event::Message {
            peer,
            message: Message::LockScreen,
        } => {
            tracing::info!("{} locks all screens", shared.name_of(&peer.to_string()));
            lock_here();
        }
        Event::Message {
            peer,
            message: Message::LogSubscribe { on },
        } => {
            tracing::info!(
                "{} {} this computer's log",
                shared.name_of(&peer.to_string()),
                if on { "collects" } else { "no longer collects" }
            );
            let mut logs = shared.logs.lock().expect("logs lock");
            logs.subscribers.retain(|(p, _)| *p != peer);
            if on {
                let (backlog, position) = peer_logs::backlog();
                logs.subscribers.push((peer, position));
                drop(logs);
                for text in backlog {
                    shared.node.send(&peer, Message::LogLines { text });
                }
            }
        }
        Event::Message {
            peer,
            message: Message::LogLines { text },
        } => {
            if shared.logs.lock().expect("logs lock").collect {
                let folder = peer_logs::folder(&shared.dir);
                if let Err(e) = peer_logs::store(&folder, &shared.name_of(&peer.to_string()), &text) {
                    tracing::warn!("cannot store a peer's log: {e}");
                }
            }
        }
        Event::Message {
            peer,
            message: Message::PowerOff,
        } => {
            tracing::info!("{} shuts down all computers", shared.name_of(&peer.to_string()));
            power_off_here();
        }
        Event::Message {
            peer,
            message: Message::LogOut,
        } => {
            tracing::info!("{} logs out all computers", shared.name_of(&peer.to_string()));
            log_out_here();
        }
        Event::Message { peer, mut message } => {
            if message == Message::DragHome {
                drag_leaves(shared, peer);
                message = Message::MouseButton {
                    button: MouseButton::Left,
                    down: false,
                };
            }
            if let Message::DragFrom { from } = &message {
                let controlled =
                    shared.engine.lock().expect("engine lock").role() == Role::ControlledBy(peer.to_string());
                if controlled {
                    tracing::info!("files dragged on {} come here", shared.name_of(from));
                    shared.files.lock().expect("files lock").drag_from = Some((from.clone(), Instant::now()));
                }
                return;
            }
            match message {
                Message::Enter { .. } => shared.files.lock().expect("files lock").left_up_seen = false,
                Message::MouseButton {
                    button: MouseButton::Left,
                    down: true,
                } => shared.files.lock().expect("files lock").remote_press = tc_clipboard::drag::change_count(),
                Message::MouseButton {
                    button: MouseButton::Left,
                    down: false,
                } => {
                    let mut files = shared.files.lock().expect("files lock");
                    if !files.left_up_seen {
                        tracing::info!("left button let go on the controlling computer");
                        tc_clipboard::drag::let_go();
                    }
                    files.left_up_seen = true;
                }
                _ => {}
            }
            let actions = shared
                .engine
                .lock()
                .expect("engine lock")
                .received(&peer.to_string(), message);
            shared.execute(actions);
        }
        Event::Paired { peer } => {
            let mut status = shared.status.lock().expect("status lock");
            status.pairing = None;
            drop(status);
            shared.reload_layout();
            shared.notice(Notice::Paired(peer.name));
        }
        Event::PairingFailed { attempts_left } => shared.notice(Notice::PairingFailed(attempts_left)),
        Event::PairingClosed => {
            shared.status.lock().expect("status lock").pairing = None;
            shared.notice(Notice::PairingClosed);
        }
        Event::Refused { peer, address, reason } => {
            let who = peer.map_or_else(|| address.to_string(), |p| shared.name_of(&p.to_string()));
            shared.notice(Notice::Refused(who, reason));
        }
        Event::Unpaired { peer, by_peer } => {
            let me = shared.node.fingerprint();
            shared.edit_layout(|file| layout_file::place(file, &me, &peer, None));
            shared.notice(Notice::Unpaired(peer.short(), by_peer));
        }
        Event::Woke { .. } => shared.notice(Notice::Woke),
    }
    shared.refresh();
}

fn handle_command(shared: &Arc<Shared>, command: Command) {
    let me = shared.node.fingerprint();
    match command {
        Command::StartPairing => {
            let code = shared.node.start_pairing();
            shared.status.lock().expect("status lock").pairing = Some((code, Instant::now() + tc_net::PAIRING_WINDOW));
        }
        Command::SaveFiles(dir) => {
            shared.files.lock().expect("files lock").save_to = Some(dir);
            request_files(shared);
        }
        Command::ClearFiles => {
            let mut status = shared.status.lock().expect("status lock");
            status.files_waiting = None;
            status.transfers.retain(|t| t.state == TransferState::Running);
        }
        Command::StopPairing => {
            shared.node.stop_pairing();
            shared.status.lock().expect("status lock").pairing = None;
        }
        Command::Pair { code, target } => {
            let Some(code) = PairingCode::parse(&code) else {
                shared.notice(Notice::PairError(PairError::WrongCode));
                return;
            };
            let addresses = match target {
                PairTarget::Address(a) => vec![a],
                PairTarget::Discovered(fp) => shared
                    .node
                    .discovered()
                    .into_iter()
                    .find(|d| d.fingerprint == fp)
                    .map(|d| d.addresses)
                    .unwrap_or_default(),
            };
            shared.status.lock().expect("status lock").pairing_busy = true;
            let node = shared.node.clone();
            let internal = shared.internal.clone();
            tokio::spawn(async move {
                let result = node.pair(&addresses, &code).await.map(|p| p.name);
                let _ = internal.send(Internal::PairDone(result));
            });
        }
        Command::Unpair(peer) => {
            if let Err(e) = shared.node.unpair(&peer) {
                tracing::warn!("cannot remove the pairing: {e}");
            }
        }
        Command::Place { peer, side } => shared.edit_layout(|file| layout_file::place(file, &me, &peer, side)),
        Command::Arrange(areas) => shared.edit_layout(|file| file.set_areas(areas)),
        Command::EdgeRule(rule) => {
            shared.engine.lock().expect("engine lock").set_edge_rule(rule);
            tracing::info!("top and bottom edges: {rule:?}");
        }
        Command::ScreenDiagonals(typed) => {
            tracing::info!("screen diagonals typed in: {typed:?}");
            shared.reported_sizes.lock().expect("sizes lock").1 = typed;
            shared.update_sizes(false);
            shared.refresh();
            shared.changed();
        }
        Command::Corner(points) => shared.engine.lock().expect("engine lock").set_corner(points),
        Command::LoginScreen(on) => {
            tracing::info!("controlled on the login screen as well: {on}");
            *shared.login_screen.lock().expect("login lock") = Some(on);
        }
        Command::FollowLayout(follow) => {
            tracing::info!("the keyboard layout follows the controlling computer: {follow}");
            shared.follow_layout.store(follow, Ordering::SeqCst);
        }
        Command::QuietRadio(quiet) => {
            tracing::info!("AWDL pauses while the pointer is shared: {quiet}");
            shared.quiet_radio.store(quiet, Ordering::SeqCst);
        }
        Command::SkipOffline(skip) => {
            tracing::info!("computers that are offline are skipped: {skip}");
            shared.engine.lock().expect("engine lock").set_skip_offline(skip);
        }
        Command::Pin(pin) => shared.toggle_pin(pin),
        Command::ShowEdges(marks) => {
            let knows = |peer: &Fingerprint| {
                shared
                    .node
                    .status()
                    .into_iter()
                    .find(|p| p.peer.fingerprint == *peer)
                    .and_then(|p| p.link)
                    .is_some_and(|link| layout_file::knows_desk(&link.app_version))
            };
            for (peer, marks) in marks {
                if knows(&peer) {
                    shared.node.send(&peer, Message::ShowEdges { marks });
                }
            }
        }
        Command::SwapCmdCtrl { peer, swap } => shared.edit_layout(|file| layout_file::set_swap(file, &peer, swap)),
        Command::Scroll { peer, scroll } => shared.edit_options(|file| layout_file::set_scroll(file, &peer, scroll)),
        Command::AddAddress { peer, address } => {
            if let Err(e) = shared.node.add_address(&peer, address) {
                tracing::warn!("cannot save the address: {e}");
            }
        }
        Command::SwitchTo(target) => {
            let target = target.map_or_else(|| me.to_string(), |fp| fp.to_string());
            let actions = shared.engine.lock().expect("engine lock").switch_to(&target);
            shared.execute(actions);
        }
        Command::LockAll => lock_all(shared),
        Command::PowerOff { this_one } => power_off_all(shared, this_one),
        Command::LogOut { this_one } => log_out_all(shared, this_one),
        Command::CollectLogs(on) => {
            let mut logs = shared.logs.lock().expect("logs lock");
            if logs.collect == on {
                return;
            }
            logs.collect = on;
            drop(logs);
            tracing::info!("collecting the logs of the other computers: {on}");
            for peer in shared.node.status().into_iter().filter(|p| p.link.is_some()) {
                shared.node.send(&peer.peer.fingerprint, Message::LogSubscribe { on });
            }
        }
        Command::ReleaseAll => {
            shared.toggle_pin(Some(false));
            let actions = shared.engine.lock().expect("engine lock").release_everything();
            shared.execute(actions);
            if let Some(inject) = &shared.inject {
                inject.release_all();
            }
            shared.notice(Notice::EmergencyRelease);
        }
        Command::RetryInput | Command::Shutdown | Command::CancelTransfer(_) => unreachable!("handled in the loop"),
    }
    shared.refresh();
    shared.changed();
}
