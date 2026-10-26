//! Installing a new version from inside the app, so no one needs a terminal for updates.
//!
//! macOS: the zip of the release is fetched with `curl` (no quarantine flag, so no Gatekeeper
//! prompt), checked to be signed by the same certificate as this app, and put in place of this app;
//! a small shell waits for this process to end and opens the new one. If the folder is not writable,
//! macOS asks for an administrator password. An installed system helper is replaced as well.
//! Windows: the installer of the release runs without questions and starts the app again.
//! Linux: if installed with the `install.sh` of the archive (programs in `~/.local/bin`), the new
//! archive is unpacked and its `install.sh` run; then the app starts again. No password needed.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver};

use eframe::egui;

/// Passed to the new version when it starts after an update.
pub const AFTER_UPDATE_FLAG: &str = "--after-update";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    Downloading,
    Installing,
    /// Done; the app quits now and the new one starts.
    Restart,
    Failed(String),
}

/// End of the release file for this system, if this build can update itself.
pub fn asset_suffix() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        bundle().map(|_| "-macOS.zip")
    } else if cfg!(windows) {
        installed_with_setup().then_some("-Windows-Setup.exe")
    } else if cfg!(target_os = "linux") {
        let arch = if cfg!(target_arch = "aarch64") {
            "-Linux-arm64.tar.gz"
        } else {
            "-Linux-x86_64.tar.gz"
        };
        installed_for_user().map(|_| arch)
    } else {
        None
    }
}

/// Downloads and installs the update at `url` in the background; when the new version is in
/// place, `quit` ends this one (from the worker thread: a hidden window is not drawn and would
/// never see the progress).
pub fn start(url: String, ctx: egui::Context, quit: impl FnOnce() + Send + 'static) -> Receiver<Progress> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("tablecross-self-update".into())
        .spawn(move || {
            let report = |p: Progress| {
                let _ = tx.send(p);
                ctx.request_repaint();
            };
            match run(&url, &report) {
                Ok(()) => {
                    tracing::info!("update installed, this version ends now");
                    report(Progress::Restart);
                    quit();
                }
                Err(e) => {
                    tracing::warn!("update failed: {e}");
                    report(Progress::Failed(e));
                }
            }
        })
        .ok();
    rx
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn command(program: &str) -> Command {
    Command::new(program)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn checked(cmd: &mut Command, what: &str) -> Result<String, String> {
    let out = cmd.output().map_err(|e| format!("{what}: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("{what}: {}", err.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn download(url: &str, to: &Path) -> Result<(), String> {
    match crate::update::fetch(url, None, 900, Some(to)) {
        Ok(_) => Ok(()),
        Err(crate::update::CheckError::NoTool) => Err("download: neither curl nor wget is installed".into()),
        Err(crate::update::CheckError::Unreachable) => Err("download failed".into()),
    }
}

fn work_dir() -> Result<PathBuf, String> {
    // Own folder per run, even within one process
    static RUN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let run = RUN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("tablecross-update-{}-{run}", std::process::id()));
    // The temporary folder may be shared (/tmp on Linux): the folder must be new and only ours,
    // else another user could have made it beforehand and swap what gets installed
    let _ = std::fs::remove_dir_all(&dir);
    let builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        let mut builder = builder;
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder
    };
    builder
        .create(&dir)
        .map_err(|e| format!("cannot make {}: {e}", dir.display()))?;
    Ok(dir)
}

/// `…/TableCross.app` this program runs from.
pub fn bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let bundle = exe.ancestors().nth(3)?;
    (bundle.extension()? == "app").then(|| bundle.to_path_buf())
}

/// Installed by the setup program (not the portable zip): the uninstaller lies next to it.
pub fn installed_with_setup() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("unins000.exe").exists()))
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn run(url: &str, report: &impl Fn(Progress)) -> Result<(), String> {
    let bundle = bundle().ok_or("not running from an app bundle")?;
    install_app(url, &bundle, report)?;
    // The system helper runs as root from its own place; it gets the new build as well (one more
    // password prompt). If that is cancelled, the status page offers it later.
    let helper = bundle.join("Contents/MacOS/tablecross-helper");
    if tc_session::helper_install::installed()
        && tc_session::helper_install::differs_from(&helper)
        && let Err(e) = tc_session::helper_install::update_from(&helper)
    {
        tracing::warn!("system helper not updated: {e}");
    }
    relaunch(&bundle)
}

/// Puts the app from the zip at `url` in place of `bundle`.
#[cfg(target_os = "macos")]
fn install_app(url: &str, bundle: &Path, report: &impl Fn(Progress)) -> Result<(), String> {
    let dir = work_dir()?;
    report(Progress::Downloading);
    let zip = dir.join("update.zip");
    download(url, &zip)?;
    report(Progress::Installing);
    let unpacked = dir.join("unpacked");
    checked(command("ditto").args(["-x", "-k"]).arg(&zip).arg(&unpacked), "unpack")?;
    let new = unpacked.join("TableCross.app");
    if !new.is_dir() {
        return Err("the download contains no TableCross.app".into());
    }
    // Only accept what the same certificate signed (not checkable for an ad-hoc signed build)
    let mut verify = command("codesign");
    verify.args(["--verify", "--deep", "--strict"]);
    if let Some(requirement) = signer_requirement(bundle) {
        verify.arg(format!("-R={requirement}"));
    }
    checked(verify.arg(&new), "signature of the new version")?;
    replace(bundle, &new)?;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// The certificate part of this app's designated requirement, if it has one.
#[cfg(target_os = "macos")]
fn signer_requirement(bundle: &Path) -> Option<String> {
    let out = command("codesign").args(["-d", "-r-"]).arg(bundle).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    let line = text.lines().find_map(|l| l.strip_prefix("designated => "))?;
    line.contains("certificate root").then(|| line.trim().to_string())
}

#[cfg(target_os = "macos")]
fn replace(bundle: &Path, new: &Path) -> Result<(), String> {
    let parent = bundle.parent().ok_or("no folder")?;
    let old = parent.join(".TableCross-old.app");
    let _ = std::fs::remove_dir_all(&old);
    if std::fs::rename(bundle, &old).is_ok() {
        let copied = checked(command("ditto").arg(new).arg(bundle), "install");
        if let Err(e) = copied {
            let _ = std::fs::remove_dir_all(bundle);
            let _ = std::fs::rename(&old, bundle);
            return Err(e);
        }
        let _ = std::fs::remove_dir_all(&old);
        return Ok(());
    }
    // Not writable for this user: macOS asks for an administrator password
    let quote = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', r"'\''"));
    let script = format!(
        "rm -rf {old} && mv {bundle} {old} && ditto {new} {bundle} && rm -rf {old}",
        old = quote(&old),
        bundle = quote(bundle),
        new = quote(new),
    );
    let apple = format!(
        "do shell script \"{}\" with administrator privileges",
        script.replace('\\', "\\\\").replace('"', "\\\"")
    );
    checked(command("osascript").args(["-e", &apple]), "install")
        .map(|_| ())
        .map_err(|e| if e.contains("-128") { "cancelled".into() } else { e })
}

/// Opens the new app as soon as this process has ended.
#[cfg(target_os = "macos")]
fn relaunch(bundle: &Path) -> Result<(), String> {
    command("/bin/sh")
        .args([
            "-c",
            "while kill -0 \"$1\" 2>/dev/null; do sleep 0.2; done; open \"$0\" --args \"$2\"",
        ])
        .arg(bundle)
        .arg(std::process::id().to_string())
        .arg(AFTER_UPDATE_FLAG)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Opens this app again as soon as this process has ended (a permission granted meanwhile only
/// reaches a new process).
#[cfg(target_os = "macos")]
pub fn restart() -> Result<(), String> {
    let bundle = bundle().ok_or("not running from an app bundle")?;
    command("/bin/sh")
        .args([
            "-c",
            "while kill -0 \"$1\" 2>/dev/null; do sleep 0.2; done; open \"$0\"",
        ])
        .arg(bundle)
        .arg(std::process::id().to_string())
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(windows)]
fn run(url: &str, report: &impl Fn(Progress)) -> Result<(), String> {
    let dir = work_dir()?;
    report(Progress::Downloading);
    let setup = dir.join("TableCross-Setup.exe");
    download(url, &setup)?;
    report(Progress::Installing);
    // The installer asks for administrator rights itself, closes this app and starts the new one
    Command::new(&setup)
        .args(["/SILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/CLOSEAPPLICATIONS"])
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("installer: {e}"))
}

/// Where `install.sh` put the programs (`~/.local/bin` or `$XDG_BIN_HOME`), if this program runs
/// from there.
pub fn installed_for_user() -> Option<PathBuf> {
    let bin = std::env::var_os("XDG_BIN_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/bin")))?;
    let exe = std::env::current_exe().ok()?;
    (exe.parent()? == bin && exe.file_name()? == "tablecross").then_some(exe)
}

#[cfg(target_os = "linux")]
fn run(url: &str, report: &impl Fn(Progress)) -> Result<(), String> {
    let exe = installed_for_user().ok_or("not installed with install.sh")?;
    let dir = work_dir()?;
    report(Progress::Downloading);
    let archive = dir.join("update.tar.gz");
    download(url, &archive)?;
    report(Progress::Installing);
    checked(command("tar").arg("-xzf").arg(&archive).arg("-C").arg(&dir), "unpack")?;
    let unpacked = std::fs::read_dir(&dir)
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.join("install.sh").is_file())
        .ok_or("the download contains no install.sh")?;
    checked(command("bash").arg(unpacked.join("install.sh")), "install")?;
    let _ = std::fs::remove_dir_all(&dir);
    // Start the new program as soon as this one has ended
    command("/bin/sh")
        .args([
            "-c",
            "while kill -0 \"$1\" 2>/dev/null; do sleep 0.2; done; exec \"$0\" \"$2\" >/dev/null 2>&1",
        ])
        .arg(&exe)
        .arg(std::process::id().to_string())
        .arg(AFTER_UPDATE_FLAG)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
fn run(_url: &str, _report: &impl Fn(Progress)) -> Result<(), String> {
    Err("not supported on this system".into())
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    /// Replaces a signed app with another build; needs one in `TC_TEST_APP` (scripts/bundle-macos.sh).
    #[test]
    #[ignore = "needs a bundled app"]
    fn replaces_a_signed_app() {
        let source = PathBuf::from(std::env::var("TC_TEST_APP").expect("TC_TEST_APP"));
        let dir = std::env::temp_dir().join(format!("tablecross-update-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Applications")).unwrap();
        let installed = dir.join("Applications/TableCross.app");
        checked(command("ditto").arg(&source).arg(&installed), "copy").unwrap();
        std::fs::write(installed.join("Contents/old-marker"), "old").unwrap();
        let zip = dir.join("release.zip");
        checked(
            command("ditto")
                .args(["-c", "-k", "--keepParent"])
                .arg(&source)
                .arg(&zip),
            "zip",
        )
        .unwrap();
        let url = format!("file://{}", zip.display());
        install_app(&url, &installed, &|_| {}).unwrap();
        assert!(installed.join("Contents/MacOS/TableCross").exists());
        assert!(!installed.join("Contents/old-marker").exists());
        assert!(!dir.join("Applications/.TableCross-old.app").exists());

        // Signed by someone else (here: ad-hoc): refused, the installed app stays
        let foreign = dir.join("foreign/TableCross.app");
        checked(command("ditto").arg(&source).arg(&foreign), "copy").unwrap();
        checked(
            command("codesign")
                .args(["--force", "--deep", "--sign", "-"])
                .arg(&foreign),
            "sign",
        )
        .unwrap();
        let zip = dir.join("foreign.zip");
        checked(
            command("ditto")
                .args(["-c", "-k", "--keepParent"])
                .arg(&foreign)
                .arg(&zip),
            "zip",
        )
        .unwrap();
        let url = format!("file://{}", zip.display());
        let err = install_app(&url, &installed, &|_| {}).unwrap_err();
        assert!(err.contains("signature"), "{err}");
        assert!(installed.join("Contents/MacOS/TableCross").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
