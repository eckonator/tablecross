//! The app's side of the macOS system helper: connects (and reconnects) to its socket, asks it to
//! take the keyboards while the pointer is on another computer, and hands its keys to the capture
//! handler. Without the helper everything works as before, except while Secure Input is active.

use std::collections::BTreeSet;
use std::io::{BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tc_core::Captured;

use core_foundation_sys::base::{CFGetTypeID, CFRelease, CFTypeRef};
use core_foundation_sys::number::{
    CFBooleanGetTypeID, CFBooleanGetValue, CFBooleanRef, CFNumberGetTypeID, CFNumberGetValue, CFNumberRef,
    kCFNumberSInt32Type,
};
use core_foundation_sys::preferences::{
    CFPreferencesAppSynchronize, CFPreferencesCopyAppValue, kCFPreferencesAnyApplication,
};
use core_foundation_sys::string::{CFStringCreateWithCString, kCFStringEncodingUTF8};

use crate::helper_protocol::{
    ALIVE_INTERVAL, FromHelper, HELPER_VERSION, LOGIN_SCREEN_SINCE, LoginAgent, OLDEST_HELPER, QUIET_RADIO_SINCE,
    ToHelper, read_frame, socket_path, top_row, write_frame,
};

const RECONNECT: Duration = Duration::from_secs(2);
/// How often the helper is reminded of the login screen, so changed pairings reach it.
const LOGIN_AGAIN: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HelperStatus {
    pub connected: bool,
    /// The helper has the Input Monitoring permission.
    pub access: bool,
    pub seized: bool,
    pub devices: u16,
    pub failed: u16,
    /// The helper speaks another version (reinstall it).
    pub version_mismatch: Option<u16>,
    /// The connected helper's version.
    pub version: u16,
    /// The first attempt to reach the helper is over (until then it is not missing yet).
    pub tried: bool,
}

pub struct HelperClient {
    writer: Mutex<Option<BufWriter<UnixStream>>>,
    want_seize: AtomicBool,
    want_quiet: AtomicBool,
    /// Control on the login screen (`None`: not wanted, unknown at first).
    want_login: Mutex<Option<Option<LoginAgent>>>,
    status: Mutex<HelperStatus>,
}

impl HelperClient {
    /// Starts connecting in the background; `on_key` gets the helper's keys and media keys.
    pub fn start(on_key: impl Fn(Captured) + Send + Sync + 'static) -> Arc<Self> {
        let client = Arc::new(Self {
            writer: Mutex::new(None),
            want_seize: AtomicBool::new(false),
            want_quiet: AtomicBool::new(false),
            want_login: Mutex::new(None),
            status: Mutex::new(HelperStatus::default()),
        });
        let on_key = Arc::new(on_key);
        let weak = Arc::downgrade(&client);
        std::thread::Builder::new()
            .name("tablecross-helper-client".into())
            .spawn(move || {
                loop {
                    let Some(client) = weak.upgrade() else { return };
                    if let Ok(stream) = UnixStream::connect(socket_path()) {
                        client.serve(stream, on_key.clone());
                    }
                    client.status.lock().expect("helper status").tried = true;
                    drop(client);
                    std::thread::sleep(RECONNECT);
                }
            })
            .expect("thread");
        // Signs of life, so the helper keeps the keyboards only while we are running
        let weak = Arc::downgrade(&client);
        std::thread::Builder::new()
            .name("tablecross-helper-alive".into())
            .spawn(move || {
                let mut login_sent = std::time::Instant::now();
                loop {
                    std::thread::sleep(ALIVE_INTERVAL);
                    let Some(client) = weak.upgrade() else { return };
                    client.send(&ToHelper::Alive);
                    if login_sent.elapsed() >= LOGIN_AGAIN {
                        login_sent = std::time::Instant::now();
                        client.send_login();
                    }
                }
            })
            .expect("thread");
        client
    }

    fn serve(&self, stream: UnixStream, on_key: Arc<impl Fn(Captured)>) {
        let Ok(reader) = stream.try_clone() else { return };
        let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
        *self.writer.lock().expect("helper writer") = Some(BufWriter::new(stream));
        self.send(&ToHelper::Hello {
            version: HELPER_VERSION,
        });
        let mut reader = BufReader::new(reader);
        // Top-row keys that went down as media keys, so they also come up as such
        let mut media_down = BTreeSet::new();
        while let Ok(message) = read_frame::<FromHelper>(&mut reader) {
            match message {
                FromHelper::Hello { version, access } => {
                    let mut status = self.status.lock().expect("helper status");
                    if !(OLDEST_HELPER..=HELPER_VERSION).contains(&version) {
                        status.version_mismatch = Some(version);
                        drop(status);
                        break;
                    }
                    *status = HelperStatus {
                        connected: true,
                        access,
                        version,
                        tried: true,
                        ..HelperStatus::default()
                    };
                    drop(status);
                    tracing::info!("system helper {version} connected (input monitoring: {access})");
                    // Restore what we want after a reconnect
                    self.send(&ToHelper::Seize(self.want_seize.load(Ordering::SeqCst)));
                    if self.want_quiet.load(Ordering::SeqCst) {
                        self.send_quiet(true);
                    }
                    self.send_login();
                }
                FromHelper::Key { usage, down } => on_key(Captured::Key { usage, down }),
                FromHelper::Media { key, down } => on_key(Captured::Media { key, down }),
                FromHelper::FunctionKey { usage, down, fn_held } => {
                    let media = if down {
                        top_row(usage, fn_held, fn_keys_standard()).inspect(|_| {
                            media_down.insert(usage);
                        })
                    } else if media_down.remove(&usage) {
                        top_row(usage, false, false)
                    } else {
                        None
                    };
                    match media {
                        Some(key) => on_key(Captured::Media { key, down }),
                        None => on_key(Captured::Key { usage, down }),
                    }
                }
                FromHelper::Status {
                    seized,
                    devices,
                    failed,
                } => {
                    let mut status = self.status.lock().expect("helper status");
                    status.seized = seized;
                    status.devices = devices;
                    status.failed = failed;
                }
            }
        }
        *self.writer.lock().expect("helper writer") = None;
        let mut status = self.status.lock().expect("helper status");
        status.connected = false;
        status.seized = false;
    }

    fn send(&self, message: &ToHelper) {
        let mut writer = self.writer.lock().expect("helper writer");
        if let Some(stream) = writer.as_mut()
            && write_frame(stream, message).and_then(|()| stream.flush()).is_err()
        {
            *writer = None;
        }
    }

    pub fn set_seize(&self, seize: bool) {
        self.want_seize.store(seize, Ordering::SeqCst);
        self.send(&ToHelper::Seize(seize));
    }

    /// Pause AWDL while the pointer is shared (helpers from `QUIET_RADIO_SINCE` on; older ones
    /// would take the message for garbage and stop listening).
    pub fn set_quiet_radio(&self, quiet: bool) {
        if self.want_quiet.swap(quiet, Ordering::SeqCst) != quiet {
            self.send_quiet(quiet);
        }
    }

    fn send_quiet(&self, quiet: bool) {
        let version = self.status().version;
        if version >= QUIET_RADIO_SINCE {
            self.send(&ToHelper::QuietRadio(quiet));
        } else if quiet && version > 0 {
            tracing::info!("the system helper {version} cannot pause AWDL yet; update it for that");
        }
    }

    /// Control on the login screen as `agent` (helpers from `LOGIN_SCREEN_SINCE` on), or not.
    pub fn set_login_screen(&self, agent: Option<LoginAgent>) {
        *self.want_login.lock().expect("login lock") = Some(agent);
        self.send_login();
    }

    fn send_login(&self) {
        let Some(agent) = self.want_login.lock().expect("login lock").clone() else {
            return;
        };
        if self.status().version >= LOGIN_SCREEN_SINCE {
            self.send(&ToHelper::LoginScreen(agent));
        }
    }

    pub fn status(&self) -> HelperStatus {
        self.status.lock().expect("helper status").clone()
    }
}

/// The setting "Use F1, F2, etc. keys as standard function keys" (System Settings → Keyboard).
fn fn_keys_standard() -> bool {
    unsafe {
        CFPreferencesAppSynchronize(kCFPreferencesAnyApplication);
        let key = CFStringCreateWithCString(
            std::ptr::null(),
            c"com.apple.keyboard.fnState".as_ptr(),
            kCFStringEncodingUTF8,
        );
        let value: CFTypeRef = CFPreferencesCopyAppValue(key, kCFPreferencesAnyApplication);
        CFRelease(key.cast());
        if value.is_null() {
            return false;
        }
        let kind = CFGetTypeID(value);
        let on = if kind == CFBooleanGetTypeID() {
            CFBooleanGetValue(value as CFBooleanRef)
        } else if kind == CFNumberGetTypeID() {
            let mut n = 0i32;
            CFNumberGetValue(value as CFNumberRef, kCFNumberSInt32Type, (&mut n as *mut i32).cast());
            n != 0
        } else {
            false
        };
        CFRelease(value);
        on
    }
}
