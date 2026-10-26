use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tc_input::win_send::{Injector, agent_pipe};
use tc_protocol::{MAX_MESSAGE_LEN, Message, decode};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    DuplicateTokenEx, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, SecurityImpersonation, SetTokenInformation,
    TOKEN_ALL_ACCESS, TokenPrimary, TokenSessionId,
};
use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_INBOUND;
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId, GetNamedPipeClientSessionId,
    PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSGetActiveConsoleSessionId};
use windows_sys::Win32::System::Services::{
    CloseServiceHandle, ControlService, CreateServiceW, DeleteService, OpenSCManagerW, OpenServiceW,
    QueryServiceStatus, RegisterServiceCtrlHandlerExW, SC_MANAGER_ALL_ACCESS, SERVICE_ACCEPT_SESSIONCHANGE,
    SERVICE_ACCEPT_STOP, SERVICE_ALL_ACCESS, SERVICE_AUTO_START, SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_STOP,
    SERVICE_ERROR_NORMAL, SERVICE_RUNNING, SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_STOPPED,
    SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS, SetServiceStatus, StartServiceCtrlDispatcherW, StartServiceW,
};
use windows_sys::Win32::System::StationsAndDesktops::{CloseDesktop, HDESK, OpenInputDesktop, SetThreadDesktop};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateProcessAsUserW, GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken,
    PROCESS_INFORMATION, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};
use windows_sys::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};

/// `CreateServiceW` while the old service is still being removed.
const ERROR_SERVICE_MARKED_FOR_DELETE: u32 = 1072;

const SERVICE_NAME: &str = "TableCross";
const DISPLAY_NAME: &str = "TableCross Service";
/// SYSTEM and administrators fully, interactive users may write. Pipe names are shared by all
/// sessions, so `client_allowed` also checks who connected.
const PIPE_SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)";
const GENERIC_ALL: u32 = 0x1000_0000;
/// The input desktop is looked up again at most this often (and after every refusal).
const DESKTOP_RECHECK: Duration = Duration::from_millis(100);

static STOP: AtomicBool = AtomicBool::new(false);
static mut STATUS_HANDLE: SERVICE_STATUS_HANDLE = std::ptr::null_mut();

fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(Some(0)).collect()
}

fn log(text: &str) {
    eprintln!("{text}");
}

pub fn main() {
    let args: Vec<String> = std::env::args().collect();
    let result = match args.get(1).map(String::as_str) {
        Some("install") => install(),
        Some("uninstall") => uninstall(),
        Some("service") => run_service(),
        Some("agent") => {
            agent();
            Ok(())
        }
        _ => {
            eprintln!("usage: tablecross-service install | uninstall");
            Ok(())
        }
    };
    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

fn last_error(what: &str) -> String {
    format!("{what} failed (error {})", unsafe { GetLastError() })
}

fn install() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let command = wide(format!("\"{}\" service", exe.display()));
    unsafe {
        let manager = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_ALL_ACCESS);
        if manager.is_null() {
            return Err(last_error("opening the service manager (run as administrator)"));
        }
        // A service just removed stays "marked for deletion" until it has stopped: wait for that
        let mut service = std::ptr::null_mut();
        for _ in 0..30 {
            service = CreateServiceW(
                manager,
                wide(SERVICE_NAME).as_ptr(),
                wide(DISPLAY_NAME).as_ptr(),
                SERVICE_ALL_ACCESS,
                SERVICE_WIN32_OWN_PROCESS,
                SERVICE_AUTO_START,
                SERVICE_ERROR_NORMAL,
                command.as_ptr(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
            );
            if !service.is_null() || GetLastError() != ERROR_SERVICE_MARKED_FOR_DELETE {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        if service.is_null() {
            CloseServiceHandle(manager);
            return Err(last_error("creating the service"));
        }
        let started = StartServiceW(service, 0, std::ptr::null()) != 0;
        CloseServiceHandle(service);
        CloseServiceHandle(manager);
        if !started {
            return Err(last_error("starting the service"));
        }
    }
    println!("Installed and started.");
    Ok(())
}

fn uninstall() -> Result<(), String> {
    unsafe {
        let manager = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_ALL_ACCESS);
        if manager.is_null() {
            return Err(last_error("opening the service manager (run as administrator)"));
        }
        let service = OpenServiceW(manager, wide(SERVICE_NAME).as_ptr(), SERVICE_ALL_ACCESS);
        if service.is_null() {
            CloseServiceHandle(manager);
            return Err(last_error("opening the service"));
        }
        let mut status: SERVICE_STATUS = std::mem::zeroed();
        ControlService(service, SERVICE_CONTROL_STOP, &mut status);
        // Wait until it has stopped (and with it the agent), so its files are free and a new
        // service can be created right after
        for _ in 0..30 {
            if QueryServiceStatus(service, &mut status) == 0 || status.dwCurrentState == SERVICE_STOPPED {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        let deleted = DeleteService(service) != 0;
        CloseServiceHandle(service);
        CloseServiceHandle(manager);
        if !deleted {
            return Err(last_error("removing the service"));
        }
    }
    println!("Removed.");
    Ok(())
}

fn run_service() -> Result<(), String> {
    let name = wide(SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: name.as_ptr() as *mut u16,
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: std::ptr::null_mut(),
            lpServiceProc: None,
        },
    ];
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
        return Err(last_error("connecting to the service manager (start it as a service)"));
    }
    Ok(())
}

fn set_status(state: u32) {
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SESSIONCHANGE
        } else {
            0
        },
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: 0,
    };
    unsafe { SetServiceStatus(STATUS_HANDLE, &status) };
}

unsafe extern "system" fn control_handler(
    control: u32,
    _event: u32,
    _data: *mut core::ffi::c_void,
    _context: *mut core::ffi::c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_STOP => {
            STOP.store(true, Ordering::SeqCst);
            0
        }
        SERVICE_CONTROL_INTERROGATE => 0,
        // Session changes are picked up by the watch loop
        _ => 0,
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut *mut u16) {
    unsafe {
        STATUS_HANDLE =
            RegisterServiceCtrlHandlerExW(wide(SERVICE_NAME).as_ptr(), Some(control_handler), std::ptr::null_mut());
    }
    set_status(SERVICE_RUNNING);
    let mut agent: Option<(u32, HANDLE)> = None;
    while !STOP.load(Ordering::SeqCst) {
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        let alive = agent
            .is_some_and(|(s, process)| s == session && unsafe { WaitForSingleObject(process, 0) } == WAIT_TIMEOUT);
        if !alive && session != u32::MAX {
            if let Some((_, process)) = agent.take() {
                unsafe {
                    TerminateProcess(process, 0);
                    CloseHandle(process);
                }
            }
            match spawn_agent(session) {
                Ok(process) => agent = Some((session, process)),
                Err(e) => log(&e),
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    if let Some((_, process)) = agent {
        unsafe {
            TerminateProcess(process, 0);
            CloseHandle(process);
        }
    }
    set_status(SERVICE_STOPPED);
}

/// Starts the agent in `session` with the service's own (SYSTEM) rights.
fn spawn_agent(session: u32) -> Result<HANDLE, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut command = wide(format!("\"{}\" agent", exe.display()));
    let mut desktop = wide("winsta0\\default");
    unsafe {
        let mut own: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut own) == 0 {
            return Err(last_error("opening the service token"));
        }
        let mut token: HANDLE = std::ptr::null_mut();
        let duplicated = DuplicateTokenEx(
            own,
            TOKEN_ALL_ACCESS,
            std::ptr::null(),
            SecurityImpersonation,
            TokenPrimary,
            &mut token,
        ) != 0;
        CloseHandle(own);
        if !duplicated {
            return Err(last_error("copying the service token"));
        }
        let mut session_id = session;
        if SetTokenInformation(
            token,
            TokenSessionId,
            (&mut session_id as *mut u32).cast(),
            size_of::<u32>() as u32,
        ) == 0
        {
            CloseHandle(token);
            return Err(last_error("moving the token to the session"));
        }
        let mut startup: STARTUPINFOW = std::mem::zeroed();
        startup.cb = size_of::<STARTUPINFOW>() as u32;
        startup.lpDesktop = desktop.as_mut_ptr();
        let mut info: PROCESS_INFORMATION = std::mem::zeroed();
        let created = CreateProcessAsUserW(
            token,
            std::ptr::null(),
            command.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_NO_WINDOW,
            std::ptr::null(),
            std::ptr::null(),
            &startup,
            &mut info,
        ) != 0;
        CloseHandle(token);
        if !created {
            return Err(last_error("starting the agent"));
        }
        CloseHandle(info.hThread);
        Ok(info.hProcess)
    }
}

/// The desktop that currently has the input (normal desktop, sign-in screen, UAC prompt).
struct InputDesktop {
    current: HDESK,
    checked: Option<Instant>,
}

impl InputDesktop {
    fn follow(&mut self, force: bool) {
        if !force && self.checked.is_some_and(|t| t.elapsed() < DESKTOP_RECHECK) {
            return;
        }
        self.checked = Some(Instant::now());
        unsafe {
            let desktop = OpenInputDesktop(0, 0, GENERIC_ALL);
            if desktop.is_null() {
                return;
            }
            if SetThreadDesktop(desktop) != 0 {
                if !self.current.is_null() {
                    CloseDesktop(self.current);
                }
                self.current = desktop;
            } else {
                CloseDesktop(desktop);
            }
        }
    }
}

/// Serves the app of this session: reads messages from the pipe and injects them on the desktop that
/// has the input. When the app goes away, everything it pressed is released.
fn agent() {
    // The app sends pixels; without this, Windows scales the agent's view of the screen and the
    // pointer lands in the wrong place at display scaling other than 100 %
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let mut session = 0u32;
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) };
    let name = wide(agent_pipe(session));
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide(PIPE_SDDL).as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        );
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let mut desktop = InputDesktop {
        current: std::ptr::null_mut(),
        checked: None,
    };
    let mut injector = Injector::default();
    loop {
        let pipe = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_INBOUND,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                0,
                64 * 1024,
                0,
                &attributes,
            )
        };
        if pipe == INVALID_HANDLE_VALUE {
            log(&last_error("creating the pipe"));
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        // A client that connected before this call is fine too (ERROR_PIPE_CONNECTED)
        unsafe { ConnectNamedPipe(pipe, std::ptr::null_mut()) };
        if let Err(reason) = client_allowed(pipe, session) {
            log(&format!("refused a client of the pipe: {reason}"));
            unsafe {
                DisconnectNamedPipe(pipe);
                CloseHandle(pipe);
            }
            continue;
        }
        let mut file = unsafe { File::from_raw_handle(pipe) };
        while let Some(message) = read_message(&mut file) {
            desktop.follow(false);
            if !injector.apply(&message) {
                // Refused: the input desktop probably changed, follow it and try again
                desktop.follow(true);
                injector.apply(&message);
            }
        }
        injector.release_all();
    }
    #[allow(unreachable_code)]
    unsafe {
        LocalFree(descriptor);
    }
}

/// Only programs from this program's folder (installed by the administrator) running in this
/// session may send input: a user of another session (switched away, remote desktop) or any other
/// program would otherwise type with SYSTEM rights, also into the sign-in screen and UAC prompts.
fn client_allowed(pipe: HANDLE, session: u32) -> Result<(), String> {
    let (mut client_session, mut pid) = (u32::MAX, 0u32);
    if unsafe { GetNamedPipeClientSessionId(pipe, &mut client_session) } == 0 {
        return Err(last_error("asking for the client's session"));
    }
    if client_session != session {
        return Err(format!("it runs in session {client_session}, not {session}"));
    }
    if unsafe { GetNamedPipeClientProcessId(pipe, &mut pid) } == 0 {
        return Err(last_error("asking for the client's process"));
    }
    let image = process_image(pid).ok_or("cannot ask for the client's program")?;
    let own = std::env::current_exe().map_err(|e| e.to_string())?;
    let folder = |p: &std::path::Path| p.parent().map(|d| d.to_string_lossy().to_lowercase());
    if folder(&image).is_none() || folder(&image) != folder(&own) {
        return Err(format!("{} is not from {}", image.display(), own.display()));
    }
    Ok(())
}

fn process_image(pid: u32) -> Option<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return None;
        }
        let mut buffer = vec![0u16; 32 * 1024];
        let mut len = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buffer.as_mut_ptr(), &mut len) != 0;
        CloseHandle(process);
        ok.then(|| std::ffi::OsString::from_wide(&buffer[..len as usize]).into())
    }
}

fn read_message(file: &mut File) -> Option<Message> {
    let mut len = [0u8; 4];
    file.read_exact(&mut len).ok()?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_MESSAGE_LEN {
        return None;
    }
    let mut bytes = vec![0; len];
    file.read_exact(&mut bytes).ok()?;
    decode(&bytes).ok()
}
