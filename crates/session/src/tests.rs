//! Two complete sessions in one process, controlled only through commands, with virtual devices.

use std::path::PathBuf;
use std::time::Duration;

use tc_core::{Captured, Point, Side};
use tc_input::{Backend, VirtualDevice, virtual_backend};
use tc_net::{Identity, Node, NodeConfig, PeerStore};
use tc_protocol::{Message, Screen};

use std::sync::Arc;

use tc_clipboard::{MemoryClipboard, Read};
use tc_protocol::ClipContent;

use crate::{Clipboard, Command, Notice, PairTarget, Session, SessionHandle, Skip};

fn screen() -> Screen {
    Screen {
        x: 0,
        y: 0,
        width: 1000,
        height: 800,
        scale: 1.0,
    }
}

fn dir_of(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("tablecross-session-{name}-{}", std::process::id()))
}

fn dir(name: &str) -> PathBuf {
    let dir = dir_of(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Machine {
    session: SessionHandle,
    device: VirtualDevice,
    node: Node,
    clipboard: Arc<MemoryClipboard>,
}

async fn machine(name: &str) -> Machine {
    let mut config = NodeConfig::new(name);
    config.bind = "[::]:0".parse().unwrap();
    config.discovery = false;
    config.screens = vec![screen()];
    let (node, events) = Node::start(config, Identity::generate().unwrap(), PeerStore::in_memory())
        .await
        .unwrap();
    let (capture, inject, device) = virtual_backend(vec![screen()]);
    let backend = Backend {
        capture: Box::new(capture),
        inject,
    };
    let clipboard = Arc::new(MemoryClipboard::new());
    let shared = Clipboard {
        system: clipboard.clone(),
        max_size: 4 * 1024 * 1024,
    };
    let (session, _task) = Session::with_node(node.clone(), events, dir(name), Some(backend), Some(shared), None)
        .await
        .unwrap();
    Machine {
        session,
        device,
        node,
        clipboard,
    }
}

/// Waits until `check` holds, up to 20 seconds (CI machines can be slow).
async fn until(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..2000 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("not reached: {what}");
}

#[tokio::test(flavor = "multi_thread")]
async fn pair_place_and_share_through_commands() {
    let a = machine("alpha").await;
    let b = machine("beta").await;

    // Pairing: Beta shows a code, Alpha types it
    b.session.send(Command::StartPairing);
    until("code shown", || b.session.status().pairing.is_some()).await;
    let code = b.session.status().pairing.unwrap().0;
    let b_addr = std::net::SocketAddr::from(([127, 0, 0, 1], b.node.local_addr().port()));
    let mut a_notices = a.session.notices();
    a.session.send(Command::Pair {
        code: code.digits().to_string(),
        target: PairTarget::Address(b_addr),
    });
    until("paired on both sides", || {
        a.session.status().peers.len() == 1 && b.session.status().peers.len() == 1
    })
    .await;
    until("connected", || a.session.status().peers[0].link.is_some()).await;
    assert!(b.session.status().pairing.is_none());
    let mut saw_paired = false;
    while let Ok(n) = a_notices.try_recv() {
        saw_paired |= matches!(n, Notice::Paired(_));
    }
    assert!(saw_paired);

    // Beta stands right of Alpha (both sides say so)
    let (a_fp, b_fp) = (a.node.fingerprint(), b.node.fingerprint());
    a.session.send(Command::Place {
        peer: b_fp,
        side: Some(Side::Right),
    });
    b.session.send(Command::Place {
        peer: a_fp,
        side: Some(Side::Left),
    });
    until("placed", || {
        a.session.status().peers[0].side == Some(Side::Right) && b.session.status().peers[0].side == Some(Side::Left)
    })
    .await;

    // Over the edge, typing, moving
    let crossing = Captured::Motion {
        x: 999.0,
        y: 400.0,
        dx: 5.0,
        dy: 0.0,
    };
    assert!(!a.device.send(crossing));
    until("Alpha controls Beta", || a.session.status().controlling.is_some()).await;
    until("Beta entered", || {
        b.device.injected().iter().any(|m| matches!(m, Message::Enter { .. }))
    })
    .await;
    assert!(!a.device.send(Captured::Key {
        usage: 0x04,
        down: true
    }));
    assert!(!a.device.send(Captured::Key {
        usage: 0x04,
        down: false
    }));
    until("keys arrive", || {
        b.device
            .injected()
            .iter()
            .filter(|m| matches!(m, Message::Key { usage: 0x04, .. }))
            .count()
            == 2
    })
    .await;
    until("Beta shows its controller", || {
        b.session.status().controlled_by.is_some()
    })
    .await;

    // Ctrl+Shift+Alt+Left brings the pointer home
    for usage in [0xE0, 0xE1, 0xE2] {
        a.device.send(Captured::Key { usage, down: true });
    }
    assert!(
        !a.device.send(Captured::Key {
            usage: 0x50,
            down: true
        }),
        "the shortcut is not forwarded"
    );
    until("back home", || !a.device.grabbed()).await;
    assert_eq!(a.device.pointer(), Point::new(500.0, 400.0));
    for usage in [0x50, 0xE0, 0xE1, 0xE2] {
        a.device.send(Captured::Key { usage, down: false });
    }

    until("Beta free again", || b.session.status().controlled_by.is_none()).await;

    // Switching by command, then Beta's own mouse takes over
    a.session.send(Command::SwitchTo(Some(b_fp)));
    until("grabbed again", || a.device.grabbed()).await;
    // Beta must know it is controlled before its own mouse can take over
    until("Beta controlled again", || b.session.status().controlled_by.is_some()).await;
    b.device.send(Captured::Motion {
        x: 500.0,
        y: 500.0,
        dx: 20.0,
        dy: 0.0,
    });
    until("Alpha let go", || !a.device.grabbed()).await;

    // Unpairing removes it on both sides and from the layout
    a.session.send(Command::Unpair(b_fp));
    until("unpaired", || {
        a.session.status().peers.is_empty() && b.session.status().peers.is_empty()
    })
    .await;
    until("layout cleaned", || a.session.status().layout.neighbors.is_empty()).await;

    a.session.send(Command::Shutdown);
    b.session.send(Command::Shutdown);
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_code_is_reported() {
    let a = machine("gamma").await;
    let b = machine("delta").await;
    b.session.send(Command::StartPairing);
    until("code shown", || b.session.status().pairing.is_some()).await;
    let b_addr = std::net::SocketAddr::from(([127, 0, 0, 1], b.node.local_addr().port()));
    let shown = b.session.status().pairing.unwrap().0;
    let wrong = if shown.digits() == "111111" { "222222" } else { "111111" };
    let mut notices = a.session.notices();
    a.session.send(Command::Pair {
        code: wrong.into(),
        target: PairTarget::Address(b_addr),
    });
    let notice = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(Notice::PairError(e)) = notices.recv().await {
                return e;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(notice, tc_net::PairError::WrongCode);
    until("Beta counts the attempt", || {
        b.session
            .status()
            .notices
            .iter()
            .any(|(_, n)| matches!(n, Notice::PairingFailed(_)))
    })
    .await;
    assert!(a.session.status().peers.is_empty());
}

/// Pairs `a` with `b` (`b` shows the code) and waits for the connection.
async fn pair(a: &Machine, b: &Machine) {
    b.session.send(Command::StartPairing);
    until("code shown", || b.session.status().pairing.is_some()).await;
    let code = b.session.status().pairing.unwrap().0;
    let b_addr = std::net::SocketAddr::from(([127, 0, 0, 1], b.node.local_addr().port()));
    a.session.send(Command::Pair {
        code: code.digits().to_string(),
        target: PairTarget::Address(b_addr),
    });
    let linked = |m: &Machine, other: &Machine| {
        m.session
            .status()
            .peers
            .iter()
            .any(|p| p.fingerprint == other.node.fingerprint() && p.link.is_some())
    };
    for _ in 0..2000 {
        if linked(a, b) && linked(b, a) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // What each side knows, for a failure that only happens now and then on CI machines
    let state = |m: &Machine| {
        let status = m.session.status();
        let peers: Vec<_> = status
            .peers
            .iter()
            .map(|p| {
                format!(
                    "{} link {:?} reconnects {} refusal {:?}",
                    p.name, p.link, p.reconnects, p.refusal
                )
            })
            .collect();
        format!(
            "pairing {:?}, busy {}, peers {peers:?}",
            status.pairing.is_some(),
            status.pairing_busy
        )
    };
    panic!("not reached: connected\nA: {}\nB: {}", state(a), state(b));
}

async fn paired_pair(a_name: &str, b_name: &str) -> (Machine, Machine) {
    let a = machine(a_name).await;
    let b = machine(b_name).await;
    pair(&a, &b).await;
    // Let both clipboard watchers take their first look
    tokio::time::sleep(Duration::from_millis(400)).await;
    (a, b)
}

#[tokio::test(flavor = "multi_thread")]
async fn arrangement_is_shared_and_reaches_three_computers() {
    let a = machine("row-a").await;
    let b = machine("row-b").await;
    let c = machine("row-c").await;
    pair(&a, &b).await;
    pair(&b, &c).await;
    pair(&a, &c).await;
    let (a_fp, b_fp, c_fp) = (a.node.fingerprint(), b.node.fingerprint(), c.node.fingerprint());

    // Each computer places only its own right neighbor; everyone ends up with the whole row
    a.session.send(Command::Place {
        peer: b_fp,
        side: Some(Side::Right),
    });
    until("Beta knows Alpha", || {
        b.session
            .status()
            .peers
            .iter()
            .any(|p| p.fingerprint == a_fp && p.side == Some(Side::Left))
    })
    .await;
    b.session.send(Command::Place {
        peer: c_fp,
        side: Some(Side::Right),
    });
    for m in [&a, &b, &c] {
        until("whole row everywhere", || {
            m.session.status().layout.neighbors.len() == 2
        })
        .await;
    }
    let row = a.session.status().layout;
    assert_eq!(b.session.status().layout.revision, row.revision);
    assert_eq!(c.session.status().layout.neighbors, row.neighbors);

    // From Alpha's keyboard through Beta to Gamma
    assert!(!a.device.send(Captured::Motion {
        x: 999.0,
        y: 400.0,
        dx: 5.0,
        dy: 0.0,
    }));
    until("Alpha controls Beta", || {
        a.session.status().controlling.as_deref() == Some("row-b")
    })
    .await;
    for _ in 0..250 {
        a.device.send(Captured::Motion {
            x: 999.0,
            y: 400.0,
            dx: 5.0,
            dy: 0.0,
        });
    }
    until("Alpha controls Gamma", || {
        a.session.status().controlling.as_deref() == Some("row-c")
    })
    .await;
    until("Gamma entered", || {
        c.device.injected().iter().any(|m| matches!(m, Message::Enter { .. }))
    })
    .await;
    for m in [a, b, c] {
        m.session.send(Command::Shutdown);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn clipboard_follows_in_both_directions() {
    let (a, b) = paired_pair("clip-a", "clip-b").await;
    let text = ClipContent {
        text: Some("Grüße aus Alpha".into()),
        html: Some("<b>Grüße</b>".into()),
        ..ClipContent::default()
    };
    a.clipboard.set(Read::Content(text.clone()));
    until("text arrives on Beta", || {
        b.clipboard.get() == Read::Content(text.clone())
    })
    .await;

    // And back, with an image larger than what is sent along at once
    let image = ClipContent {
        png: Some((0..2_000_000u32).map(|i| (i % 253) as u8).collect()),
        ..ClipContent::default()
    };
    b.clipboard.set(Read::Content(image.clone()));
    // Alpha is the computer in use (not controlling anyone), so it fetches it right away
    until("image arrives on Alpha", || {
        a.clipboard.get() == Read::Content(image.clone())
    })
    .await;

    // Nothing bounces back and forth
    let (count_a, count_b) = (
        tc_clipboard::SystemClipboard::change_count(a.clipboard.as_ref()),
        tc_clipboard::SystemClipboard::change_count(b.clipboard.as_ref()),
    );
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(
        tc_clipboard::SystemClipboard::change_count(a.clipboard.as_ref()),
        count_a
    );
    assert_eq!(
        tc_clipboard::SystemClipboard::change_count(b.clipboard.as_ref()),
        count_b
    );

    // Passwords stay where they are
    let before = b.clipboard.get();
    a.clipboard.set(Read::Concealed);
    until("noticed as private", || {
        a.session
            .status()
            .notices
            .iter()
            .any(|(_, n)| *n == Notice::ClipboardSkipped(Skip::Concealed))
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(b.clipboard.get(), before);
    a.session.send(Command::Shutdown);
    b.session.send(Command::Shutdown);
}

/// Both file tests use `TC_DOWNLOAD_DIR`; they must not run at the same time.
static DOWNLOAD_DIR: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test(flavor = "multi_thread")]
async fn copied_files_arrive_when_saved() {
    let _guard = DOWNLOAD_DIR.lock().await;
    let source = dir("files-source");
    std::fs::create_dir_all(source.join("Fotos/Urlaub")).unwrap();
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 249) as u8).collect();
    std::fs::write(source.join("Fotos/Urlaub/big.bin"), &big).unwrap();
    std::fs::write(source.join("Fotos/a.txt"), "Grüße").unwrap();
    std::fs::write(source.join("notes.txt"), "hello").unwrap();
    // A program and a link inside, as in an app
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(source.join("Fotos/show.sh"), "#!/bin/sh").unwrap();
        std::fs::set_permissions(source.join("Fotos/show.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("Urlaub/big.bin", source.join("Fotos/latest")).unwrap();
    }
    let target = dir("files-target");
    // Only this test uses the variable
    unsafe { std::env::set_var("TC_DOWNLOAD_DIR", &target) };

    let (a, b) = paired_pair("files-a", "files-b").await;
    a.clipboard
        .set(Read::Files(vec![source.join("Fotos"), source.join("notes.txt")]));
    until("Beta knows about the files", || {
        b.session.status().files_waiting.is_some()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Not even when the pointer arrives: only when saved
    a.session.send(Command::SwitchTo(Some(b.node.fingerprint())));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!target.join("Fotos").exists(), "nothing moves before saving");

    b.session.send(Command::SaveFiles(target.clone()));
    until("files arrived", || {
        b.session
            .status()
            .notices
            .iter()
            .any(|(_, n)| matches!(n, Notice::FilesReceived(..)))
    })
    .await;
    assert_eq!(std::fs::read(target.join("Fotos/Urlaub/big.bin")).unwrap(), big);
    assert_eq!(std::fs::read_to_string(target.join("Fotos/a.txt")).unwrap(), "Grüße");
    assert_eq!(std::fs::read_to_string(target.join("notes.txt")).unwrap(), "hello");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(target.join("Fotos/show.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "the program still runs");
        assert_eq!(
            std::fs::read_link(target.join("Fotos/latest")).unwrap(),
            PathBuf::from("Urlaub/big.bin")
        );
    }
    assert_ne!(
        b.clipboard.get(),
        Read::Files(vec![target.join("Fotos"), target.join("notes.txt")]),
        "saved files stay off the clipboard"
    );
    // No part folder left, and the files are not announced back
    let leftovers: Vec<_> = std::fs::read_dir(&target)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with(".tablecross-part"))
        .collect();
    assert!(leftovers.is_empty());
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(a.session.status().files_waiting.is_none());
    let transfers = b.session.status().transfers;
    assert_eq!(transfers.len(), 1);
    assert_eq!(transfers[0].done, transfers[0].total);

    // Copied again: this time pasted in the file manager (same pair, pairing takes long)
    std::fs::write(source.join("notes.txt"), "pasted").unwrap();
    a.clipboard.set(Read::Files(vec![source.join("notes.txt")]));
    until("Beta has them on its clipboard", || {
        b.session.status().files_waiting.is_some_and(|w| w.pasteable)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(b.session.status().transfers.len(), 1, "nothing moves before pasting");
    // The file manager pastes (and waits)
    let clipboard = b.clipboard.clone();
    let pasted = tokio::task::spawn_blocking(move || clipboard.paste_promise())
        .await
        .unwrap()
        .expect("the files came");
    assert_eq!(pasted.len(), 1);
    assert!(pasted[0].starts_with(crate::files::promise_dir()), "{pasted:?}");
    assert_eq!(std::fs::read_to_string(&pasted[0]).unwrap(), "pasted");
    // Pasting again hands out the same files without fetching them again
    let clipboard = b.clipboard.clone();
    let again = tokio::task::spawn_blocking(move || clipboard.paste_promise())
        .await
        .unwrap();
    assert_eq!(again, Some(pasted));
    assert_eq!(b.session.status().transfers.len(), 2);
    a.session.send(Command::Shutdown);
    b.session.send(Command::Shutdown);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_transfer_leaves_nothing_behind() {
    let _guard = DOWNLOAD_DIR.lock().await;
    let source = dir("cancel-source");
    std::fs::write(source.join("huge.bin"), vec![1u8; 300_000_000]).unwrap();
    let target = dir("cancel-target");
    unsafe { std::env::set_var("TC_DOWNLOAD_DIR", &target) };
    let (a, b) = paired_pair("cancel-a", "cancel-b").await;
    a.clipboard.set(Read::Files(vec![source.join("huge.bin")]));
    until("Beta knows about the file", || {
        b.session.status().files_waiting.is_some()
    })
    .await;
    b.session.send(Command::SaveFiles(target.clone()));
    until("transfer running", || {
        b.session.status().transfers.first().is_some_and(|t| t.done > 0)
    })
    .await;
    let id = b.session.status().transfers[0].id;
    b.session.send(Command::CancelTransfer(id));
    until("cancelled", || {
        b.session.status().transfers[0].state == crate::TransferState::Cancelled
    })
    .await;
    until("sender noticed", || {
        a.session
            .status()
            .transfers
            .first()
            .is_some_and(|t| t.state != crate::TransferState::Running)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let left: Vec<_> = std::fs::read_dir(&target).unwrap().filter_map(Result::ok).collect();
    assert!(left.is_empty(), "{left:?}");
    let _ = std::fs::remove_dir_all(&source);
    a.session.send(Command::Shutdown);
    b.session.send(Command::Shutdown);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_computer_collects_the_logs_of_its_peers() {
    let (a, b) = paired_pair("logs-a", "logs-b").await;
    // Both sessions share this process; what is recorded reaches A from B
    crate::peer_logs::record(b"before collecting\n");
    a.session.send(Command::CollectLogs(true));
    let folder = crate::peer_logs::folder(&dir_of("logs-a"));
    let file = folder.join("logs-b.log");
    until("backlog arrives", || {
        std::fs::read_to_string(&file).is_ok_and(|t| t.contains("before collecting"))
    })
    .await;
    crate::peer_logs::record(b"new line after subscribing\n");
    until("new lines arrive", || {
        std::fs::read_to_string(&file).is_ok_and(|t| t.contains("new line after subscribing"))
    })
    .await;
    // Stopped: nothing more is stored
    a.session.send(Command::CollectLogs(false));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let before = std::fs::read_to_string(&file).unwrap();
    crate::peer_logs::record(b"after stopping\n");
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(!std::fs::read_to_string(&file).unwrap()[before.len()..].contains("after stopping"));
    drop(b);
}
