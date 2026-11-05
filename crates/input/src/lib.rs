//! Capturing the local mouse and keyboard and injecting input from other machines, per operating
//! system, behind two small traits. The logic of *where* input goes lives in `tc-core`; a backend
//! only reports events and carries out what it is told.

pub mod keymap {
    //! Native key codes ↔ USB HID usages. Plain tables, compiled and tested on every system.
    pub mod linux;
    pub mod mac;
    pub mod windows;
}

#[cfg(target_os = "macos")]
pub mod handoff;
pub mod helper_protocol;
pub mod keyboard_layout;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
mod mac_helper;
#[cfg(target_os = "macos")]
mod mac_prefs;
#[cfg(any(target_os = "macos", windows, test))]
mod repeat;
pub mod system;
#[cfg(target_os = "macos")]
pub mod universal_control;
mod virtual_input;
#[cfg(target_os = "linux")]
mod wayland;
#[cfg(windows)]
pub mod win_send;
#[cfg(windows)]
mod windows;

use std::sync::Arc;

use tc_core::{Captured, Point, Side};
use tc_protocol::{Message, Screen};

pub use virtual_input::{VirtualCapture, VirtualDevice, VirtualInject, virtual_backend};

/// Called for every event of the local devices. Returns whether the local system gets the event
/// (`true`) or it is suppressed (`false`). Runs on the capture thread and must be quick.
pub type Handler = Box<dyn FnMut(Captured) -> bool + Send>;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Health {
    /// Events of the local devices arrive.
    pub capturing: bool,
    pub problems: Vec<Problem>,
}

/// Something the user should know about; the app shows each one in their language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// macOS: Accessibility permission missing (capturing and injecting impossible).
    AccessibilityMissing,
    /// macOS: a program holds Secure Input and no working helper covers it: letters stay on this Mac.
    SecureInput,
    /// The event tap or the hooks do not run.
    CaptureStopped,
    /// macOS: the system helper is not installed or not running.
    HelperMissing,
    /// macOS: the system helper lacks the Input Monitoring permission.
    HelperNoAccess,
    /// macOS: the system helper is from another version.
    HelperOutdated,
    /// macOS: control on the login screen is on, but its program lacks the Accessibility permission
    /// (macOS then drops what it injects there without a word).
    LoginAgentNoAccess,
    /// macOS: keyboards the helper could not take away from the system.
    HelperKeyboardsBusy(u16),
    /// Windows removed the hooks this many times (they were installed again).
    HooksRemoved(u32),
    /// macOS: Universal Control is switched on and takes the pointer at screen edges as well.
    UniversalControl,
    /// Linux with Wayland: the remote control portal is not available or was not allowed, so this
    /// computer cannot be controlled from others.
    ControlUnavailable,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Problem::AccessibilityMissing => write!(f, "Accessibility permission is missing"),
            Problem::SecureInput => write!(
                f,
                "a program holds Secure Input (password field): keys do not reach the other computer"
            ),
            Problem::CaptureStopped => write!(f, "input capture is not running"),
            Problem::HelperMissing => write!(f, "the system helper is not running"),
            Problem::HelperNoAccess => write!(f, "the system helper lacks the Input Monitoring permission"),
            Problem::HelperOutdated => write!(f, "the system helper is outdated; install it again"),
            Problem::LoginAgentNoAccess => write!(
                f,
                "tablecross-loginwindow lacks the Accessibility permission: no control on the login screen"
            ),
            Problem::HelperKeyboardsBusy(n) => {
                write!(
                    f,
                    "the system helper could not take {n} keyboard(s); their keys also reach this Mac"
                )
            }
            Problem::HooksRemoved(n) => write!(f, "Windows removed the input hooks {n} times (installed again)"),
            Problem::UniversalControl => write!(f, "Universal Control of macOS is switched on"),
            Problem::ControlUnavailable => write!(f, "remote control of this computer is not available"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("permission missing: {0}")]
    Permission(String),
    #[error("{0}")]
    System(String),
    #[error("not supported on this system yet")]
    Unsupported,
}

/// The local devices.
pub trait Capture: Send {
    /// Starts capturing on a thread of its own.
    fn start(&mut self, handler: Handler) -> Result<(), InputError>;
    fn stop(&mut self);
    fn health(&self) -> Health;
    /// The system reports a single push at an edge instead of continuous motion (Wayland
    /// barriers): edges must switch at once there.
    fn single_push_at_edges(&self) -> bool {
        false
    }
    /// The sides of this computer where a computer that is online lies beyond. Wayland puts
    /// barriers only there: a barrier holds the pointer until it is given back.
    fn open_sides(&self, _sides: &[Side]) {}
    /// macOS: pause AWDL (the radio of AirDrop, Handoff and AirPlay) through the system helper
    /// while the pointer is shared; it makes Wi-Fi packets wait every few seconds.
    fn quiet_radio(&self, _quiet: bool) {}
    /// macOS: let the system helper set up control on the login screen as this computer (`None`:
    /// no longer).
    fn login_screen(&self, _agent: Option<helper_protocol::LoginAgent>) {}
}

/// No capture at all: a computer that is only controlled (the login screen of a Mac).
pub struct NoCapture;

impl Capture for NoCapture {
    fn start(&mut self, _handler: Handler) -> Result<(), InputError> {
        Ok(())
    }

    fn stop(&mut self) {}

    fn health(&self) -> Health {
        Health::default()
    }
}

/// The local system's pointer and keyboard.
pub trait Inject: Send + Sync {
    /// Hide the pointer and keep it at `park`; the capture reports motion as deltas from now on.
    fn grab(&self, park: Point) -> Result<(), InputError>;
    /// Show the pointer again at `at`.
    fn ungrab(&self, at: Point) -> Result<(), InputError>;
    /// `Enter`, `MouseMove`, `Key`, `MouseButton`, `Wheel`.
    fn inject(&self, message: &Message) -> Result<(), InputError>;
    /// Releases everything injected so far (safety net when shutting down).
    fn release_all(&self);
    fn screens(&self) -> Vec<Screen>;
    /// Physical size of each screen in millimeters, in the order of `screens`; `(0, 0)` where the
    /// display does not tell. Empty: not known on this system.
    fn sizes(&self) -> Vec<(u32, u32)> {
        Vec::new()
    }
    /// Delay and interval for repeating a held injected key; `None`: the system repeats injected
    /// keys itself (or nothing is known).
    fn key_repeat(&self) -> Option<(std::time::Duration, std::time::Duration)> {
        None
    }
}

/// Capture and injection of one system.
pub struct Backend {
    pub capture: Box<dyn Capture>,
    pub inject: Arc<dyn Inject>,
}

/// Injected keys repeat while held (macOS and Windows repeat only real keyboards).
#[cfg(any(target_os = "macos", windows))]
fn repeating(backend: Backend) -> Backend {
    Backend {
        inject: repeat::Repeating::wrap(backend.inject),
        ..backend
    }
}

/// The backend of this system.
pub fn platform() -> Result<Backend, InputError> {
    #[cfg(target_os = "macos")]
    {
        mac::backend().map(repeating)
    }
    #[cfg(windows)]
    {
        windows::backend().map(repeating)
    }
    #[cfg(target_os = "linux")]
    {
        // Wayland sessions only through the portals; X11 (and XWayland-less X sessions) directly
        if std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t.eq_ignore_ascii_case("wayland"))
            || std::env::var_os("WAYLAND_DISPLAY").is_some() && std::env::var_os("DISPLAY").is_none()
        {
            wayland::backend()
        } else {
            linux::backend()
        }
    }
    #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
    {
        Err(InputError::Unsupported)
    }
}
