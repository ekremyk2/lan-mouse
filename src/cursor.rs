//! Hides the mouse pointer of the device the user is not working on, so only
//! one pointer is visible: while this device controls another one, and after
//! another device stopped controlling this one. The pointer shows again as
//! soon as it moves, or when control comes back.

use std::{sync::mpsc, thread};

enum Command {
    /// hide the pointer until it moves
    Hide,
    Show,
}

#[derive(Clone)]
pub(crate) struct CursorHider {
    tx: Option<mpsc::Sender<Command>>,
}

impl CursorHider {
    pub(crate) fn new(enabled: bool) -> Self {
        if !enabled || !cfg!(any(windows, target_os = "macos")) {
            return Self { tx: None };
        }
        let (tx, rx) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("cursor-hider".into())
            .spawn(move || platform::run(rx));
        if let Err(e) = spawned {
            log::warn!("cannot hide the mouse pointer: {e}");
            return Self { tx: None };
        }
        Self { tx: Some(tx) }
    }

    /// hide the pointer until it moves or [`CursorHider::show`] is called
    pub(crate) fn hide(&self) {
        self.send(Command::Hide);
    }

    pub(crate) fn show(&self) {
        self.send(Command::Show);
    }

    fn send(&self, command: Command) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(command);
        }
    }
}

/// Windows has no call to hide the pointer for other programs. Instead, a
/// 1x1 pixel, almost fully transparent, topmost window is put right under the
/// pointer and gives it an empty shape. Once the pointer moves off that pixel
/// it takes the shape of whatever is below again, and if Lan Mouse exits the
/// window goes away with it, so the pointer can never stay hidden by accident.
#[cfg(windows)]
mod platform {
    use std::sync::mpsc;

    use windows::{
        Win32::{
            Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, WPARAM},
            System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
            UI::WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DispatchMessageW, GetCursorPos, GetMessageW,
                HWND_TOPMOST, KillTimer, LWA_ALPHA, MA_NOACTIVATE, MSG, PostThreadMessageW,
                RegisterClassW, SW_HIDE, SWP_NOACTIVATE, SWP_SHOWWINDOW, SetCursor, SetCursorPos,
                SetLayeredWindowAttributes, SetTimer, SetWindowPos, ShowWindow, WM_APP,
                WM_MOUSEACTIVATE, WM_SETCURSOR, WM_TIMER, WNDCLASSW, WS_EX_LAYERED,
                WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
            },
        },
        core::w,
    };

    use super::Command;

    const WM_COMMAND_PENDING: u32 = WM_APP + 1;
    const MOVE_TIMER: usize = 1;
    /// how often to check whether the hidden pointer moved (ms)
    const MOVE_CHECK_MS: u32 = 50;

    pub(super) fn run(rx: mpsc::Receiver<Command>) {
        let window = match create_window() {
            Ok(window) => window,
            Err(e) => return log::warn!("cannot hide the mouse pointer: {e}"),
        };
        // commands arrive through `rx`; wake the message loop for each of them
        let thread_id = unsafe { GetCurrentThreadId() };
        let (forward_tx, forward_rx) = mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(command) = rx.recv() {
                if forward_tx.send(command).is_err() {
                    break;
                }
                let _ = unsafe {
                    PostThreadMessageW(thread_id, WM_COMMAND_PENDING, WPARAM(0), LPARAM(0))
                };
            }
        });

        let mut hidden_at: Option<POINT> = None;
        let mut msg = MSG::default();
        while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
            match msg.message {
                WM_COMMAND_PENDING => {
                    while let Ok(command) = forward_rx.try_recv() {
                        match command {
                            Command::Hide => hidden_at = hide(window),
                            Command::Show => {
                                show(window);
                                hidden_at = None;
                            }
                        }
                    }
                }
                WM_TIMER if msg.wParam.0 == MOVE_TIMER => {
                    if let Some(at) = hidden_at {
                        if cursor_pos().is_none_or(|pos| pos.x != at.x || pos.y != at.y) {
                            show(window);
                            hidden_at = None;
                        }
                    }
                }
                _ => unsafe {
                    DispatchMessageW(&msg);
                },
            }
        }
    }

    fn create_window() -> windows::core::Result<HWND> {
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance.into(),
                lpszClassName: w!("lan-mouse-cursor-hider"),
                // no class cursor: the pointer is invisible over this window
                ..Default::default()
            };
            RegisterClassW(&class);
            let window = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED,
                w!("lan-mouse-cursor-hider"),
                w!(""),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(instance.into()),
                None,
            )?;
            // invisible, but still under the pointer as far as Windows is concerned
            SetLayeredWindowAttributes(window, COLORREF(0), 1, LWA_ALPHA)?;
            Ok(window)
        }
    }

    unsafe extern "system" fn window_proc(
        window: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_SETCURSOR => {
                unsafe { SetCursor(None) };
                LRESULT(1)
            }
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            _ => unsafe { DefWindowProcW(window, msg, wparam, lparam) },
        }
    }

    fn cursor_pos() -> Option<POINT> {
        let mut pos = POINT::default();
        unsafe { GetCursorPos(&mut pos) }.ok()?;
        Some(pos)
    }

    /// returns where the pointer was hidden
    fn hide(window: HWND) -> Option<POINT> {
        let pos = cursor_pos()?;
        unsafe {
            if let Err(e) = SetWindowPos(
                window,
                Some(HWND_TOPMOST),
                pos.x,
                pos.y,
                1,
                1,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            ) {
                log::warn!("cannot hide the mouse pointer: {e}");
                return None;
            }
            SetCursor(None);
            SetTimer(Some(window), MOVE_TIMER, MOVE_CHECK_MS, None);
        }
        Some(pos)
    }

    fn show(window: HWND) {
        unsafe {
            let _ = KillTimer(Some(window), MOVE_TIMER);
            let _ = ShowWindow(window, SW_HIDE);
            // let the window below set the pointer shape again right away
            if let Some(pos) = cursor_pos() {
                let _ = SetCursorPos(pos.x, pos.y);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use std::{thread::sleep, time::Duration};

        use windows::Win32::UI::WindowsAndMessaging::{CURSORINFO, GetCursorInfo};

        use crate::cursor::CursorHider;

        fn current_cursor() -> windows::Win32::UI::WindowsAndMessaging::HCURSOR {
            let mut info = CURSORINFO {
                cbSize: size_of::<CURSORINFO>() as u32,
                ..Default::default()
            };
            unsafe { GetCursorInfo(&mut info) }.expect("cursor info");
            info.hCursor
        }

        #[test]
        #[ignore = "briefly hides the real mouse pointer"]
        fn hides_and_shows_pointer() {
            let hider = CursorHider::new(true);
            hider.hide();
            sleep(Duration::from_millis(300));
            let hidden = current_cursor();
            hider.show();
            sleep(Duration::from_millis(300));
            let shown = current_cursor();
            assert!(hidden.is_invalid(), "pointer still visible");
            assert!(!shown.is_invalid(), "pointer not shown again");
        }
    }
}

/// macOS hides the pointer with `CGDisplayHideCursor`, which normally only
/// works for the frontmost app; the `SetsCursorInBackground` connection
/// property (used by many utilities) lifts that restriction.
#[cfg(target_os = "macos")]
mod platform {
    use std::{
        ffi::{CString, c_char, c_void},
        sync::mpsc,
        time::Duration,
    };

    use super::Command;

    /// how often to check whether the hidden pointer moved
    const MOVE_CHECK: Duration = Duration::from_millis(50);

    #[repr(C)]
    #[derive(Clone, Copy, PartialEq)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGMainDisplayID() -> u32;
        fn CGDisplayHideCursor(display: u32) -> i32;
        fn CGDisplayShowCursor(display: u32) -> i32;
        fn CGEventCreate(source: *const c_void) -> *mut c_void;
        fn CGEventGetLocation(event: *mut c_void) -> CGPoint;
        fn _CGSDefaultConnection() -> i32;
        fn CGSSetConnectionProperty(
            connection: i32,
            target: i32,
            key: *const c_void,
            value: *const c_void,
        ) -> i32;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFBooleanTrue: *const c_void;
        fn CFStringCreateWithCString(
            allocator: *const c_void,
            string: *const c_char,
            encoding: u32,
        ) -> *const c_void;
        fn CFRelease(object: *const c_void);
    }

    const UTF8: u32 = 0x0800_0100;

    pub(super) fn run(rx: mpsc::Receiver<Command>) {
        allow_hiding_in_background();
        let mut hidden_at: Option<CGPoint> = None;
        loop {
            let command = match hidden_at {
                Some(_) => match rx.recv_timeout(MOVE_CHECK) {
                    Ok(command) => Some(command),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                },
                None => match rx.recv() {
                    Ok(command) => Some(command),
                    Err(_) => break,
                },
            };
            match command {
                Some(Command::Hide) if hidden_at.is_none() => {
                    unsafe { CGDisplayHideCursor(CGMainDisplayID()) };
                    hidden_at = Some(cursor_pos());
                }
                Some(Command::Show) => show(&mut hidden_at),
                Some(Command::Hide) => {}
                None => {
                    if hidden_at.is_some_and(|at| at != cursor_pos()) {
                        show(&mut hidden_at);
                    }
                }
            }
        }
        show(&mut hidden_at);
    }

    fn show(hidden_at: &mut Option<CGPoint>) {
        if hidden_at.take().is_some() {
            unsafe { CGDisplayShowCursor(CGMainDisplayID()) };
        }
    }

    fn cursor_pos() -> CGPoint {
        unsafe {
            let event = CGEventCreate(std::ptr::null());
            if event.is_null() {
                return CGPoint { x: 0.0, y: 0.0 };
            }
            let pos = CGEventGetLocation(event);
            CFRelease(event);
            pos
        }
    }

    fn allow_hiding_in_background() {
        let key = CString::new("SetsCursorInBackground").expect("no nul byte");
        unsafe {
            let key = CFStringCreateWithCString(std::ptr::null(), key.as_ptr(), UTF8);
            if key.is_null() {
                return;
            }
            let connection = _CGSDefaultConnection();
            CGSSetConnectionProperty(connection, connection, key, kCFBooleanTrue);
            CFRelease(key);
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    pub(super) fn run(_rx: std::sync::mpsc::Receiver<super::Command>) {}
}
