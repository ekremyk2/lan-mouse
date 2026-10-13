//! Hides the mouse pointer of the device the user is not working on, so only
//! one pointer is visible: while this device controls another one, and after
//! another device stopped controlling this one. The pointer shows again as
//! soon as it moves, or when control comes back.

use lan_mouse_proto::Position;
use tokio::sync::mpsc;

pub(crate) use platform::{pointer_and_screen, screens};

/// where along `edge` of its screen the pointer is, from 0 (top / left)
/// to 1 (bottom / right)
pub(crate) fn position_along_edge(edge: Position) -> Option<f64> {
    let (x, y, screen) = pointer_and_screen()?;
    Some(along_edge(edge, screen, x, y))
}

/// put the pointer at `along` (0..1) of `edge` of the outermost screen on
/// that side, so it enters where it left the other device
pub(crate) fn move_to_edge(edge: Position, along: f64) {
    let screens = screens();
    if let Some(screen) = outermost(edge, &screens) {
        let (x, y) = point_on_edge(edge, screen, along);
        platform::move_pointer(x, y);
    }
}

/// a screen in global pointer coordinates (y grows downwards); right and
/// bottom are exclusive
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Screen {
    pub(crate) left: f64,
    pub(crate) top: f64,
    pub(crate) right: f64,
    pub(crate) bottom: f64,
}

fn along_edge(edge: Position, screen: Screen, x: f64, y: f64) -> f64 {
    let along = match edge {
        Position::Left | Position::Right => (y - screen.top) / (screen.bottom - screen.top - 1.0),
        Position::Top | Position::Bottom => (x - screen.left) / (screen.right - screen.left - 1.0),
    };
    if along.is_finite() {
        along.clamp(0.0, 1.0)
    } else {
        0.5
    }
}

/// the screen whose `edge` is furthest out
fn outermost(edge: Position, screens: &[Screen]) -> Option<Screen> {
    let key = |screen: &Screen| match edge {
        Position::Left => -screen.left,
        Position::Right => screen.right,
        Position::Top => -screen.top,
        Position::Bottom => screen.bottom,
    };
    screens
        .iter()
        .copied()
        .max_by(|a, b| key(a).total_cmp(&key(b)))
}

/// the point `along` (0..1) of `edge`, just inside the screen
fn point_on_edge(edge: Position, screen: Screen, along: f64) -> (f64, f64) {
    let x_along = screen.left + along * (screen.right - screen.left - 1.0);
    let y_along = screen.top + along * (screen.bottom - screen.top - 1.0);
    match edge {
        Position::Left => (screen.left + 1.0, y_along),
        Position::Right => (screen.right - 2.0, y_along),
        Position::Top => (x_along, screen.top + 1.0),
        Position::Bottom => (x_along, screen.bottom - 2.0),
    }
}

enum Command {
    /// hide the pointer, if `until_moved` only until it moves
    Hide {
        #[cfg_attr(windows, allow(dead_code))]
        until_moved: bool,
    },
    Show,
}

#[derive(Clone)]
pub(crate) struct CursorHider {
    tx: Option<mpsc::UnboundedSender<Command>>,
}

impl CursorHider {
    pub(crate) fn new(enabled: bool) -> Self {
        if !enabled || !cfg!(any(windows, target_os = "macos")) {
            return Self { tx: None };
        }
        let (tx, rx) = mpsc::unbounded_channel();
        if let Err(e) = platform::start(rx) {
            log::warn!("cannot hide the mouse pointer: {e}");
            return Self { tx: None };
        }
        Self { tx: Some(tx) }
    }

    /// hide the pointer until [`CursorHider::show`] is called, while this
    /// device controls another one
    pub(crate) fn hide(&self) {
        self.send(Command::Hide { until_moved: false });
    }

    /// hide the pointer until it moves or [`CursorHider::show`] is called,
    /// after another device stopped controlling this one
    pub(crate) fn hide_until_moved(&self) {
        self.send(Command::Hide { until_moved: true });
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
    use std::{sync::mpsc, thread};

    use tokio::sync::mpsc::UnboundedReceiver;
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

    pub(super) fn start(rx: UnboundedReceiver<Command>) -> std::io::Result<()> {
        thread::Builder::new()
            .name("cursor-hider".into())
            .spawn(move || run(rx))?;
        Ok(())
    }

    fn run(mut rx: UnboundedReceiver<Command>) {
        let window = match create_window() {
            Ok(window) => window,
            Err(e) => return log::warn!("cannot hide the mouse pointer: {e}"),
        };
        // commands arrive through `rx`; wake the message loop for each of them
        let thread_id = unsafe { GetCurrentThreadId() };
        let (forward_tx, forward_rx) = mpsc::channel();
        thread::spawn(move || {
            while let Some(command) = rx.blocking_recv() {
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
                            // while controlling another device the pointer
                            // does not move here, so this is the same
                            Command::Hide { .. } => hidden_at = hide(window),
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

    fn to_screen(rect: windows::Win32::Foundation::RECT) -> super::Screen {
        super::Screen {
            left: rect.left as f64,
            top: rect.top as f64,
            right: rect.right as f64,
            bottom: rect.bottom as f64,
        }
    }

    /// the pointer position and the screen it is on
    pub(crate) fn pointer_and_screen() -> Option<(f64, f64, super::Screen)> {
        use windows::Win32::Graphics::Gdi::{
            GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
        };
        let pos = cursor_pos()?;
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let monitor = unsafe { MonitorFromPoint(pos, MONITOR_DEFAULTTONEAREST) };
        if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
            return None;
        }
        Some((pos.x as f64, pos.y as f64, to_screen(info.rcMonitor)))
    }

    pub(crate) fn screens() -> Vec<super::Screen> {
        use windows::{
            Win32::{
                Foundation::RECT,
                Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR},
            },
            core::BOOL,
        };
        unsafe extern "system" fn collect(
            _monitor: HMONITOR,
            _hdc: HDC,
            rect: *mut RECT,
            screens: LPARAM,
        ) -> BOOL {
            let screens = unsafe { &mut *(screens.0 as *mut Vec<super::Screen>) };
            screens.push(to_screen(unsafe { *rect }));
            BOOL(1)
        }
        let mut screens: Vec<super::Screen> = vec![];
        let _ = unsafe {
            EnumDisplayMonitors(
                None,
                None,
                Some(collect),
                LPARAM(&mut screens as *mut Vec<super::Screen> as isize),
            )
        };
        screens
    }

    pub(super) fn move_pointer(x: f64, y: f64) {
        let _ = unsafe { SetCursorPos(x.round() as i32, y.round() as i32) };
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
/// property (used by many utilities) lifts that restriction. This runs on the
/// main thread, as a task of the service's event loop: from other threads
/// macOS may ignore the call.
#[cfg(target_os = "macos")]
mod platform {
    use std::{
        ffi::{CString, c_char, c_void},
        time::Duration,
    };

    use tokio::sync::mpsc::UnboundedReceiver;

    use super::Command;

    /// how often to check whether the hidden pointer moved
    const MOVE_CHECK: Duration = Duration::from_millis(50);

    #[repr(C)]
    #[derive(Clone, Copy, PartialEq)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGSize {
        width: f64,
        height: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGRect {
        origin: CGPoint,
        size: CGSize,
    }

    /// the pointer position and the screen it is on
    pub(crate) fn pointer_and_screen() -> Option<(f64, f64, super::Screen)> {
        let pos = cursor_pos();
        let screen = screens().into_iter().find(|screen| {
            screen.left <= pos.x
                && pos.x < screen.right
                && screen.top <= pos.y
                && pos.y < screen.bottom
        })?;
        Some((pos.x, pos.y, screen))
    }

    pub(crate) fn screens() -> Vec<super::Screen> {
        let mut displays = [0u32; 16];
        let mut count = 0u32;
        if unsafe {
            CGGetActiveDisplayList(displays.len() as u32, displays.as_mut_ptr(), &mut count)
        } != 0
        {
            return vec![];
        }
        displays[..count as usize]
            .iter()
            .map(|&display| {
                let bounds = unsafe { CGDisplayBounds(display) };
                super::Screen {
                    left: bounds.origin.x,
                    top: bounds.origin.y,
                    right: bounds.origin.x + bounds.size.width,
                    bottom: bounds.origin.y + bounds.size.height,
                }
            })
            .collect()
    }

    pub(super) fn move_pointer(x: f64, y: f64) {
        unsafe { CGWarpMouseCursorPosition(CGPoint { x, y }) };
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGMainDisplayID() -> u32;
        fn CGDisplayHideCursor(display: u32) -> i32;
        fn CGDisplayShowCursor(display: u32) -> i32;
        fn CGEventCreate(source: *const c_void) -> *mut c_void;
        fn CGEventGetLocation(event: *mut c_void) -> CGPoint;
        fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
        fn CGDisplayBounds(display: u32) -> CGRect;
        fn CGWarpMouseCursorPosition(position: CGPoint) -> i32;
        fn CGAssociateMouseAndMouseCursorPosition(connected: i32) -> i32;
        fn CGCursorIsVisible() -> i32;
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

    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Shown,
        Hidden,
        /// hidden while the pointer stays at this point
        HiddenAt(CGPoint),
    }

    pub(super) fn start(rx: UnboundedReceiver<Command>) -> std::io::Result<()> {
        tokio::task::spawn_local(run(rx));
        Ok(())
    }

    async fn run(mut rx: UnboundedReceiver<Command>) {
        let mut state = State::Shown;
        let mut move_check = tokio::time::interval(MOVE_CHECK);
        loop {
            tokio::select! {
                command = rx.recv() => match command {
                    Some(Command::Hide { until_moved }) => {
                        if state == State::Shown {
                            hide();
                        }
                        state = if until_moved {
                            State::HiddenAt(cursor_pos())
                        } else {
                            State::Hidden
                        };
                    }
                    Some(Command::Show) => show(&mut state),
                    None => break,
                },
                _ = move_check.tick(), if matches!(state, State::HiddenAt(_)) => {
                    if state != State::HiddenAt(cursor_pos()) {
                        show(&mut state);
                    }
                }
            }
        }
        show(&mut state);
    }

    fn hide() {
        // set right before hiding: set once, early, it may not stick
        allow_hiding_in_background();
        unsafe {
            CGDisplayHideCursor(CGMainDisplayID());
            // other tools found this keeps the pointer from randomly staying visible
            CGAssociateMouseAndMouseCursorPosition(1);
        }
        if unsafe { CGCursorIsVisible() } != 0 {
            log::debug!("asked macOS to hide the mouse pointer, but it says it is still visible");
        }
    }

    fn show(state: &mut State) {
        if std::mem::replace(state, State::Shown) != State::Shown {
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
    pub(super) fn start(
        _rx: tokio::sync::mpsc::UnboundedReceiver<super::Command>,
    ) -> std::io::Result<()> {
        Ok(())
    }

    pub(crate) fn pointer_and_screen() -> Option<(f64, f64, super::Screen)> {
        None
    }

    pub(crate) fn screens() -> Vec<super::Screen> {
        vec![]
    }

    pub(super) fn move_pointer(_x: f64, _y: f64) {}
}

#[cfg(test)]
mod geometry_tests {
    use super::*;

    const LAPTOP: Screen = Screen {
        left: 0.0,
        top: 0.0,
        right: 1920.0,
        bottom: 1080.0,
    };
    /// a taller monitor to the left of the laptop
    const MONITOR: Screen = Screen {
        left: -1440.0,
        top: -300.0,
        right: 0.0,
        bottom: 2260.0,
    };

    #[test]
    fn keeps_the_relative_position() {
        // a quarter of the way down the laptop's 1080 pixel rows (0..=1079)
        let along = along_edge(Position::Left, LAPTOP, 0.0, 0.25 * 1079.0);
        assert!((along - 0.25).abs() < 1e-9);
        // entering a twice as tall screen a quarter of the way down
        let (x, y) = point_on_edge(Position::Right, MONITOR, along);
        assert_eq!(x, -2.0);
        assert!((y - (-300.0 + 0.25 * 2559.0)).abs() < 0.5);
        let (x, y) = point_on_edge(Position::Bottom, LAPTOP, 0.5);
        assert_eq!(y, 1078.0);
        assert!((x - 959.5).abs() < 0.5);
    }

    #[test]
    fn picks_the_outermost_screen() {
        let screens = [LAPTOP, MONITOR];
        assert_eq!(outermost(Position::Left, &screens), Some(MONITOR));
        assert_eq!(outermost(Position::Right, &screens), Some(LAPTOP));
        assert_eq!(outermost(Position::Top, &screens), Some(MONITOR));
        assert_eq!(outermost(Position::Left, &[]), None);
    }
}
