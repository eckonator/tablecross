//! The keyboard layout in use, and switching it. Keys travel as positions (HID usages), so the
//! characters they make depend on the layout of the computer that receives them; a controlled
//! computer can take over the layout of the one that controls it.
//!
//! - macOS: Text Input Sources (keyboard layouts and input methods that can be selected), on the
//!   main thread as the system demands. Without a main run loop (command line) nothing happens.
//! - Windows: the layout of the window in front, switched through `WM_INPUTLANGCHANGEREQUEST`.
//! - Linux: not yet (X11 at least types like the local keyboard, see `linux.rs`).

pub use tc_protocol::KeyboardLayout;

/// The one of `available` that types like `wanted`: the same layout on the same kind of system,
/// else one for the same language and region, else one for the same language.
pub fn best_match<'a>(wanted: &KeyboardLayout, available: &'a [KeyboardLayout]) -> Option<&'a KeyboardLayout> {
    let language = |l: &KeyboardLayout| l.language.to_lowercase().replace('_', "-");
    let primary = |l: &KeyboardLayout| language(l).split('-').next().unwrap_or_default().to_string();
    available
        .iter()
        .find(|l| l.id == wanted.id)
        .or_else(|| {
            available
                .iter()
                .find(|l| !language(l).is_empty() && language(l) == language(wanted))
        })
        .or_else(|| {
            available
                .iter()
                .find(|l| !primary(l).is_empty() && primary(l) == primary(wanted))
        })
}

/// Whether `a` and `b` type the same characters as far as can be told.
pub fn alike(a: &KeyboardLayout, b: &KeyboardLayout) -> bool {
    best_match(b, std::slice::from_ref(a)).is_some()
}

/// The layout in use right now.
pub fn current() -> Option<KeyboardLayout> {
    platform::current()
}

/// The layouts that can be switched to.
pub fn enabled() -> Vec<KeyboardLayout> {
    platform::enabled()
}

/// Switches to `layout` (one of [`enabled`]); whether that was possible.
pub fn select(layout: &KeyboardLayout) -> bool {
    platform::select(layout)
}

#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::c_void;
    use std::time::Duration;

    use core_foundation_sys::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
    use core_foundation_sys::base::{CFGetTypeID, CFRelease, CFTypeRef, kCFAllocatorDefault};
    use core_foundation_sys::dictionary::{
        CFDictionaryCreate, CFDictionaryRef, kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks,
    };
    use core_foundation_sys::number::kCFBooleanTrue;
    use core_foundation_sys::string::{
        CFStringGetCString, CFStringGetLength, CFStringGetTypeID, CFStringRef, kCFStringEncodingUTF8,
    };
    use dispatch2::DispatchQueue;

    use super::KeyboardLayout;

    type Source = *mut c_void;

    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn TISCopyCurrentKeyboardInputSource() -> Source;
        fn TISCreateInputSourceList(properties: CFDictionaryRef, include_all_installed: bool) -> CFArrayRef;
        fn TISGetInputSourceProperty(source: Source, key: CFStringRef) -> *const c_void;
        fn TISSelectInputSource(source: Source) -> i32;
        static kTISPropertyInputSourceID: CFStringRef;
        static kTISPropertyInputSourceLanguages: CFStringRef;
        static kTISPropertyLocalizedName: CFStringRef;
        static kTISPropertyInputSourceCategory: CFStringRef;
        static kTISPropertyInputSourceIsSelectCapable: CFStringRef;
        static kTISCategoryKeyboardInputSource: CFStringRef;
    }

    unsafe extern "C" {
        fn pthread_main_np() -> i32;
    }

    /// How long to wait for the main thread.
    const WAIT: Duration = Duration::from_millis(500);

    /// Runs `work` on the main thread and waits for it; `None` if the main thread does not get to it
    /// (no run loop there, or busy).
    fn on_main<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Option<T> {
        if unsafe { pthread_main_np() } != 0 {
            return Some(work());
        }
        let (tx, rx) = std::sync::mpsc::channel();
        DispatchQueue::main().exec_async(move || {
            let _ = tx.send(work());
        });
        rx.recv_timeout(WAIT).ok()
    }

    fn text(value: *const c_void) -> Option<String> {
        if value.is_null() || unsafe { CFGetTypeID(value as CFTypeRef) } != unsafe { CFStringGetTypeID() } {
            return None;
        }
        let string = value as CFStringRef;
        let capacity = unsafe { CFStringGetLength(string) } as usize * 4 + 1;
        let mut buffer = vec![0u8; capacity];
        let ok = unsafe {
            CFStringGetCString(
                string,
                buffer.as_mut_ptr().cast(),
                capacity as isize,
                kCFStringEncodingUTF8,
            )
        };
        if ok == 0 {
            return None;
        }
        let end = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
        String::from_utf8(buffer[..end].to_vec()).ok()
    }

    /// Reads a source; it stays owned by the caller.
    fn describe(source: Source) -> Option<KeyboardLayout> {
        unsafe {
            let id = text(TISGetInputSourceProperty(source, kTISPropertyInputSourceID))?;
            let name = text(TISGetInputSourceProperty(source, kTISPropertyLocalizedName)).unwrap_or_default();
            let languages = TISGetInputSourceProperty(source, kTISPropertyInputSourceLanguages) as CFArrayRef;
            let language = if languages.is_null() || CFArrayGetCount(languages) == 0 {
                String::new()
            } else {
                text(CFArrayGetValueAtIndex(languages, 0)).unwrap_or_default()
            };
            Some(KeyboardLayout { id, language, name })
        }
    }

    /// The keyboard sources that can be selected; released by the caller.
    fn sources() -> CFArrayRef {
        unsafe {
            let keys = [
                kTISPropertyInputSourceCategory.cast::<c_void>(),
                kTISPropertyInputSourceIsSelectCapable.cast::<c_void>(),
            ];
            let values = [
                kTISCategoryKeyboardInputSource.cast::<c_void>(),
                kCFBooleanTrue.cast::<c_void>(),
            ];
            let filter = CFDictionaryCreate(
                kCFAllocatorDefault,
                keys.as_ptr(),
                values.as_ptr(),
                2,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            );
            let list = TISCreateInputSourceList(filter, false);
            CFRelease(filter.cast());
            list
        }
    }

    pub fn current() -> Option<KeyboardLayout> {
        on_main(|| unsafe {
            let source = TISCopyCurrentKeyboardInputSource();
            if source.is_null() {
                return None;
            }
            let layout = describe(source);
            CFRelease(source.cast());
            layout
        })
        .flatten()
    }

    pub fn enabled() -> Vec<KeyboardLayout> {
        on_main(|| unsafe {
            let list = sources();
            if list.is_null() {
                return Vec::new();
            }
            let layouts = (0..CFArrayGetCount(list))
                .filter_map(|i| describe(CFArrayGetValueAtIndex(list, i) as Source))
                .collect();
            CFRelease(list.cast());
            layouts
        })
        .unwrap_or_default()
    }

    pub fn select(layout: &KeyboardLayout) -> bool {
        let id = layout.id.clone();
        on_main(move || unsafe {
            let list = sources();
            if list.is_null() {
                return false;
            }
            let mut done = false;
            for i in 0..CFArrayGetCount(list) {
                let source = CFArrayGetValueAtIndex(list, i) as Source;
                if describe(source).is_some_and(|l| l.id == id) {
                    done = TISSelectInputSource(source) == 0;
                    break;
                }
            }
            CFRelease(list.cast());
            done
        })
        .unwrap_or(false)
    }
}

#[cfg(windows)]
mod platform {
    use windows_sys::Win32::Globalization::LCIDToLocaleName;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardLayout, GetKeyboardLayoutList, HKL};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId, PostMessageW, WM_INPUTLANGCHANGEREQUEST,
    };

    use super::KeyboardLayout;

    fn describe(hkl: HKL) -> KeyboardLayout {
        let language_id = (hkl as usize & 0xFFFF) as u32;
        let mut buffer = [0u16; 85];
        let len = unsafe { LCIDToLocaleName(language_id, buffer.as_mut_ptr(), buffer.len() as i32, 0) };
        let language = if len > 1 {
            String::from_utf16_lossy(&buffer[..len as usize - 1])
        } else {
            String::new()
        };
        KeyboardLayout {
            id: format!("{:X}", hkl as usize),
            name: language.clone(),
            language,
        }
    }

    pub fn current() -> Option<KeyboardLayout> {
        unsafe {
            let window = GetForegroundWindow();
            let thread = GetWindowThreadProcessId(window, std::ptr::null_mut());
            let hkl = GetKeyboardLayout(thread);
            (!hkl.is_null()).then(|| describe(hkl))
        }
    }

    pub fn enabled() -> Vec<KeyboardLayout> {
        unsafe {
            let count = GetKeyboardLayoutList(0, std::ptr::null_mut());
            if count <= 0 {
                return Vec::new();
            }
            let mut list: Vec<HKL> = vec![std::ptr::null_mut(); count as usize];
            let count = GetKeyboardLayoutList(count, list.as_mut_ptr());
            list.truncate(count.max(0) as usize);
            list.into_iter().map(describe).collect()
        }
    }

    pub fn select(layout: &KeyboardLayout) -> bool {
        let Ok(handle) = usize::from_str_radix(&layout.id, 16) else {
            return false;
        };
        unsafe {
            let window = GetForegroundWindow();
            !window.is_null() && PostMessageW(window, WM_INPUTLANGCHANGEREQUEST, 0, handle as isize) != 0
        }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use super::KeyboardLayout;

    pub fn current() -> Option<KeyboardLayout> {
        None
    }

    pub fn enabled() -> Vec<KeyboardLayout> {
        Vec::new()
    }

    pub fn select(_layout: &KeyboardLayout) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(id: &str, language: &str) -> KeyboardLayout {
        KeyboardLayout {
            id: id.into(),
            language: language.into(),
            name: String::new(),
        }
    }

    #[test]
    fn the_same_layout_first_then_language_and_region_then_language() {
        let mac = [
            layout("com.apple.keylayout.US", "en"),
            layout("com.apple.keylayout.German", "de"),
            layout("com.apple.keylayout.SwissGerman", "de-CH"),
        ];
        assert_eq!(
            best_match(&layout("com.apple.keylayout.SwissGerman", "de-CH"), &mac),
            Some(&mac[2])
        );
        // From Windows: Swiss German by language and region, German by language
        assert_eq!(best_match(&layout("8070807", "de-CH"), &mac), Some(&mac[2]));
        assert_eq!(best_match(&layout("4070407", "de-DE"), &mac), Some(&mac[1]));
        assert_eq!(best_match(&layout("40C040C", "fr-FR"), &mac), None);
        // From a Mac to Windows
        let windows = [layout("4090409", "en-US"), layout("4070407", "de-DE")];
        assert_eq!(
            best_match(&layout("com.apple.keylayout.German", "de"), &windows),
            Some(&windows[1])
        );
        assert!(alike(&windows[1], &layout("com.apple.keylayout.German", "de")));
        // Nothing known about the language: only the same id fits
        assert_eq!(best_match(&layout("x", ""), &[layout("y", "")]), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn without_a_run_loop_on_the_main_thread_nothing_is_read() {
        // As in a test or on the command line: no waiting forever
        assert_eq!(current(), None);
        assert!(enabled().is_empty());
    }
}
