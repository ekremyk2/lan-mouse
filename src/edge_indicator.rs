//! A glow at the screen edge while the mouse pushes against it, growing and
//! brightening with the push, and a flash when the mouse crosses over to the
//! other device. Drawn in a click-through overlay window, on Windows and macOS.
#![cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]

use input_capture::Position;
use tokio::sync::mpsc;

use crate::cursor::{self, Screen};

enum Command {
    /// the mouse pushes against `edge`; `progress` is 0..1 of the way to crossing
    Push { edge: Position, progress: f32 },
    /// the mouse crossed over at `edge`
    Crossed { edge: Position },
    /// the push ended without crossing
    Cancel,
}

#[derive(Clone)]
pub(crate) struct EdgeIndicator {
    tx: Option<mpsc::UnboundedSender<Command>>,
}

impl EdgeIndicator {
    pub(crate) fn new(enabled: bool) -> Self {
        if !enabled || !cfg!(any(windows, target_os = "macos")) {
            return Self { tx: None };
        }
        let (tx, rx) = mpsc::unbounded_channel();
        if let Err(e) = platform::start(rx) {
            log::warn!("no edge indicator: {e}");
            return Self { tx: None };
        }
        Self { tx: Some(tx) }
    }

    pub(crate) fn push(&self, edge: Position, progress: f64) {
        let progress = progress.clamp(0.0, 1.0) as f32;
        self.send(Command::Push { edge, progress });
    }

    pub(crate) fn crossed(&self, edge: Position) {
        self.send(Command::Crossed { edge });
    }

    pub(crate) fn cancel(&self) {
        self.send(Command::Cancel);
    }

    fn send(&self, command: Command) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(command);
        }
    }
}

/// what is shown right now
struct Glow {
    edge: Position,
    /// pointer position when the push began; the glow is centered on it
    anchor: (f64, f64),
    /// the screen the push happens on
    screen: Screen,
    progress: f32,
    flash: f32,
    phase: f32,
}

impl Glow {
    /// where the glow goes, at the edge of its screen: x, y, width, height
    /// in global pointer coordinates
    fn placement(&self) -> (i32, i32, i32, i32) {
        let (left, top) = (self.screen.left as i32, self.screen.top as i32);
        let (right, bottom) = (self.screen.right as i32, self.screen.bottom as i32);
        let (anchor_x, anchor_y) = (self.anchor.0 as i32, self.anchor.1 as i32);
        let (thickness, length) = glow_size(self.progress, self.flash);
        let span = match self.edge {
            Position::Left | Position::Right => bottom - top,
            Position::Top | Position::Bottom => right - left,
        };
        let length = length.min(span);
        let centered = |anchor: i32, start: i32, end: i32| {
            (anchor - length / 2).clamp(start, (end - length).max(start))
        };
        match self.edge {
            Position::Left => (left, centered(anchor_y, top, bottom), thickness, length),
            Position::Right => (
                right - thickness,
                centered(anchor_y, top, bottom),
                thickness,
                length,
            ),
            Position::Top => (centered(anchor_x, left, right), top, length, thickness),
            Position::Bottom => (
                centered(anchor_x, left, right),
                bottom - thickness,
                length,
                thickness,
            ),
        }
    }
}

/// change what is shown according to `command`
fn apply(glow: &mut Option<Glow>, command: Command) {
    match command {
        Command::Push { edge, progress } => {
            if let Some(glow) = start(glow, edge) {
                glow.progress = progress;
                glow.flash = 0.0;
            }
        }
        Command::Crossed { edge } => {
            if let Some(glow) = start(glow, edge) {
                glow.progress = 1.0;
                glow.flash = 1.0;
            }
        }
        // a running flash fades out by itself
        Command::Cancel => {
            if glow.as_ref().is_some_and(|glow| glow.flash == 0.0) {
                *glow = None;
            }
        }
    }
}

/// the glow at `edge`, started where the pointer is if none is shown yet
fn start(glow: &mut Option<Glow>, edge: Position) -> Option<&mut Glow> {
    if glow.is_none() {
        let (x, y, screen) = cursor::pointer_and_screen()?;
        *glow = Some(Glow {
            edge,
            anchor: (x, y),
            screen,
            progress: 0.0,
            flash: 0.0,
            phase: 0.0,
        });
    }
    let glow = glow.as_mut()?;
    glow.edge = edge;
    Some(glow)
}

/// advance the animation by one frame; the glow is gone once its flash faded
fn animate(glow: &mut Option<Glow>) {
    let Some(current) = glow else {
        return;
    };
    current.phase += 0.3;
    if current.flash > 0.0 {
        current.flash -= 0.07;
        if current.flash <= 0.0 {
            *glow = None;
        }
    }
}

/// accent color of the glow (RGB)
const GLOW: [f32; 3] = [124.0, 92.0, 255.0];

/// size of the glow (thickness, length) in pixels
fn glow_size(progress: f32, flash: f32) -> (i32, i32) {
    let thickness = 12.0 + 28.0 * progress + 30.0 * flash;
    let length = 220.0 + 300.0 * progress + 240.0 * flash;
    (thickness as i32, length as i32)
}

/// Draw the glow into a `width` x `height` image as premultiplied BGRA rows,
/// top to bottom, `edge` being the screen edge it sits on. `phase` drives the
/// pulsing, `flash` (0..1) blends in the white flash of crossing over.
fn render(
    edge: Position,
    width: i32,
    height: i32,
    progress: f32,
    flash: f32,
    phase: f32,
) -> Vec<u8> {
    let pulse = 0.85 + 0.15 * phase.sin();
    let intensity = ((0.55 + 0.45 * progress) * pulse).max(flash);
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    for y in 0..height {
        for x in 0..width {
            // 0 at the screen edge, 1 at the inner side
            let depth = match edge {
                Position::Left => x as f32 / width as f32,
                Position::Right => (width - 1 - x) as f32 / width as f32,
                Position::Top => y as f32 / height as f32,
                Position::Bottom => (height - 1 - y) as f32 / height as f32,
            };
            // -1 .. 1 along the edge
            let along = match edge {
                Position::Left | Position::Right => (y as f32 + 0.5) / height as f32 * 2.0 - 1.0,
                Position::Top | Position::Bottom => (x as f32 + 0.5) / width as f32 * 2.0 - 1.0,
            };
            let alpha = intensity * (1.0 - depth).powf(1.6) * (1.0 - along * along).max(0.0);
            // white-hot core at the very edge, and all white while flashing
            let white = ((1.0 - depth).powi(8) * 0.75).max(flash);
            let i = ((y * width + x) * 4) as usize;
            for (channel, color) in [2, 1, 0].into_iter().zip(GLOW) {
                let color = color + (255.0 - color) * white;
                pixels[i + channel] = (color * alpha).round() as u8;
            }
            pixels[i + 3] = (255.0 * alpha).round() as u8;
        }
    }
    pixels
}

#[cfg(windows)]
mod platform {
    use std::{sync::mpsc, thread};

    use tokio::sync::mpsc::UnboundedReceiver;
    use windows::{
        Win32::{
            Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM},
            Graphics::Gdi::{
                AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
                CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject,
                GetDC, ReleaseDC, SelectObject,
            },
            System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
            UI::WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, KillTimer, MSG,
                PostThreadMessageW, RegisterClassW, SW_HIDE, SW_SHOWNOACTIVATE, SetTimer,
                ShowWindow, ULW_ALPHA, UpdateLayeredWindow, WM_APP, WM_TIMER, WNDCLASSW,
                WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
                WS_EX_TRANSPARENT, WS_POPUP,
            },
        },
        core::w,
    };

    use super::{Command, Glow, animate, apply, render};

    const WM_COMMAND_PENDING: u32 = WM_APP + 1;
    const FRAME_TIMER: usize = 1;
    /// ~60 frames per second while visible
    const FRAME_MS: u32 = 16;

    pub(super) fn start(rx: UnboundedReceiver<Command>) -> std::io::Result<()> {
        thread::Builder::new()
            .name("edge-indicator".into())
            .spawn(move || run(rx))?;
        Ok(())
    }

    fn run(mut rx: UnboundedReceiver<Command>) {
        let window = match create_window() {
            Ok(window) => window,
            Err(e) => return log::warn!("no edge indicator: {e}"),
        };
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

        let mut glow: Option<Glow> = None;
        let mut msg = MSG::default();
        while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
            match msg.message {
                WM_COMMAND_PENDING => {
                    let shown = glow.is_some();
                    while let Ok(command) = forward_rx.try_recv() {
                        apply(&mut glow, command);
                    }
                    if !shown && glow.is_some() {
                        unsafe { SetTimer(Some(window), FRAME_TIMER, FRAME_MS, None) };
                    }
                    update(window, &glow);
                }
                WM_TIMER if msg.wParam.0 == FRAME_TIMER => {
                    animate(&mut glow);
                    update(window, &glow);
                }
                _ => unsafe {
                    DispatchMessageW(&msg);
                },
            }
        }
    }

    fn update(window: HWND, glow: &Option<Glow>) {
        match glow {
            Some(glow) => draw(window, glow),
            None => hide(window),
        }
    }

    fn hide(window: HWND) {
        unsafe {
            let _ = KillTimer(Some(window), FRAME_TIMER);
            let _ = ShowWindow(window, SW_HIDE);
        }
    }

    fn create_window() -> windows::core::Result<HWND> {
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance.into(),
                lpszClassName: w!("lan-mouse-edge-indicator"),
                ..Default::default()
            };
            RegisterClassW(&class);
            // click-through: never in the way of the mouse
            CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE,
                w!("lan-mouse-edge-indicator"),
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
            )
        }
    }

    unsafe extern "system" fn window_proc(
        window: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe { DefWindowProcW(window, msg, wparam, lparam) }
    }

    fn draw(window: HWND, glow: &Glow) {
        let (x, y, width, height) = glow.placement();
        let position = POINT { x, y };
        let size = SIZE {
            cx: width,
            cy: height,
        };
        let pixels = render(
            glow.edge,
            size.cx,
            size.cy,
            glow.progress,
            glow.flash,
            glow.phase,
        );
        unsafe {
            let screen = GetDC(None);
            let memory = CreateCompatibleDC(Some(screen));
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: size.cx,
                    // negative: rows go top to bottom
                    biHeight: -size.cy,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            if let Ok(bitmap) =
                CreateDIBSection(Some(memory), &info, DIB_RGB_COLORS, &mut bits, None, 0)
            {
                std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits.cast::<u8>(), pixels.len());
                let previous = SelectObject(memory, bitmap.into());
                let blend = BLENDFUNCTION {
                    BlendOp: AC_SRC_OVER as u8,
                    BlendFlags: 0,
                    SourceConstantAlpha: 255,
                    AlphaFormat: AC_SRC_ALPHA as u8,
                };
                let source = POINT::default();
                let _ = UpdateLayeredWindow(
                    window,
                    Some(screen),
                    Some(&position),
                    Some(&size),
                    Some(memory),
                    Some(&source),
                    COLORREF(0),
                    Some(&blend),
                    ULW_ALPHA,
                );
                SelectObject(memory, previous);
                let _ = DeleteObject(bitmap.into());
                let _ = ShowWindow(window, SW_SHOWNOACTIVATE);
            }
            let _ = DeleteDC(memory);
            ReleaseDC(None, screen);
        }
    }
}

/// A borderless, transparent, click-through window above everything else,
/// whose layer shows the glow image. AppKit wants the main thread, so this
/// runs there, as a task of the service's event loop, and lets AppKit handle
/// its events every frame.
#[cfg(target_os = "macos")]
mod platform {
    use std::{ffi::c_void, time::Duration};

    use objc2::{
        MainThreadMarker, MainThreadOnly, msg_send,
        rc::Retained,
        runtime::{AnyClass, AnyObject},
    };
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSColor, NSEventMask,
        NSScreenSaverWindowLevel, NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask,
    };
    use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSSize};
    use tokio::{
        sync::mpsc::UnboundedReceiver,
        time::{MissedTickBehavior, interval},
    };

    use super::{Command, Glow, animate, apply, render};
    use crate::cursor;

    /// ~60 frames per second while visible
    const FRAME: Duration = Duration::from_millis(16);
    /// how often AppKit gets to handle its events while nothing is shown
    const IDLE: Duration = Duration::from_secs(1);

    pub(super) fn start(rx: UnboundedReceiver<Command>) -> std::io::Result<()> {
        tokio::task::spawn_local(run(rx));
        Ok(())
    }

    async fn run(mut rx: UnboundedReceiver<Command>) {
        let Some(mtm) = MainThreadMarker::new() else {
            return log::warn!("no edge indicator: not running on the main thread");
        };
        let mut overlay: Option<Overlay> = None;
        let mut glow: Option<Glow> = None;
        let mut frames = interval(FRAME);
        frames.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut idle = interval(IDLE);
        loop {
            let shown = glow.is_some();
            tokio::select! {
                command = rx.recv() => {
                    let Some(command) = command else {
                        break;
                    };
                    apply(&mut glow, command);
                    while let Ok(command) = rx.try_recv() {
                        apply(&mut glow, command);
                    }
                    // while shown, the next frame draws the change
                    if shown && glow.is_some() {
                        continue;
                    }
                }
                _ = frames.tick(), if shown => animate(&mut glow),
                _ = idle.tick() => {
                    if let Some(overlay) = &overlay {
                        overlay.handle_events();
                    }
                    continue;
                }
            }
            match &glow {
                Some(glow) => {
                    if overlay.is_none() {
                        overlay = Overlay::new(mtm);
                    }
                    match overlay.as_mut() {
                        Some(overlay) => overlay.show(glow),
                        None => return log::warn!("no edge indicator: cannot create its window"),
                    }
                }
                None => {
                    if let Some(overlay) = overlay.as_mut() {
                        overlay.hide();
                    }
                }
            }
        }
    }

    struct Overlay {
        app: Retained<NSApplication>,
        window: Retained<NSWindow>,
        layer: Retained<AnyObject>,
        transaction: &'static AnyClass,
        visible: bool,
    }

    impl Overlay {
        fn new(mtm: MainThreadMarker) -> Option<Self> {
            let layer_class = AnyClass::get(c"CALayer")?;
            let transaction = AnyClass::get(c"CATransaction")?;
            let app = NSApplication::sharedApplication(mtm);
            // no Dock icon or menu bar, never takes the focus
            let _ = app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
            let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0));
            let window = unsafe {
                NSWindow::initWithContentRect_styleMask_backing_defer(
                    NSWindow::alloc(mtm),
                    frame,
                    NSWindowStyleMask::Borderless,
                    NSBackingStoreType::Buffered,
                    false,
                )
            };
            // owned here, closing must not free it
            unsafe { window.setReleasedWhenClosed(false) };
            window.setOpaque(false);
            window.setBackgroundColor(Some(&NSColor::clearColor()));
            window.setHasShadow(false);
            // click-through: never in the way of the mouse
            window.setIgnoresMouseEvents(true);
            window.setLevel(NSScreenSaverWindowLevel);
            window.setCollectionBehavior(
                NSWindowCollectionBehavior::CanJoinAllSpaces
                    | NSWindowCollectionBehavior::Stationary
                    | NSWindowCollectionBehavior::IgnoresCycle
                    | NSWindowCollectionBehavior::FullScreenAuxiliary,
            );
            let view = window.contentView()?;
            // the view hosts a plain layer, which shows the glow image
            let layer: Retained<AnyObject> = unsafe { msg_send![layer_class, layer] };
            let _: () = unsafe { msg_send![&*view, setLayer: &*layer] };
            view.setWantsLayer(true);
            Some(Self {
                app,
                window,
                layer,
                transaction,
                visible: false,
            })
        }

        fn show(&mut self, glow: &Glow) {
            let (x, y, width, height) = glow.placement();
            let pixels = render(
                glow.edge,
                width,
                height,
                glow.progress,
                glow.flash,
                glow.phase,
            );
            let Some(image) = Image::new(&pixels, width, height) else {
                return;
            };
            // Cocoa counts y upwards from the bottom of the main screen
            let main_bottom = cursor::screens()
                .into_iter()
                .find(|screen| screen.left == 0.0 && screen.top == 0.0)
                .map_or(0.0, |screen| screen.bottom);
            let size = NSSize::new(width as f64, height as f64);
            let frame = NSRect::new(
                NSPoint::new(x as f64, main_bottom - (y + height) as f64),
                size,
            );
            let bounds = NSRect::new(NSPoint::new(0.0, 0.0), size);
            unsafe {
                let transaction = self.transaction;
                let _: () = msg_send![transaction, begin];
                // no fading from one frame to the next
                let _: () = msg_send![transaction, setDisableActions: true];
                self.window.setFrame_display(frame, true);
                let _: () = msg_send![&*self.layer, setFrame: bounds];
                let _: () = msg_send![&*self.layer, setContents: image.as_object()];
                let _: () = msg_send![transaction, commit];
            }
            if !self.visible {
                self.window.orderFrontRegardless();
                self.visible = true;
            }
            self.handle_events();
        }

        fn hide(&mut self) {
            if self.visible {
                self.window.orderOut(None);
                self.visible = false;
                self.handle_events();
            }
        }

        /// AppKit puts changes on the screen, and stays responsive, while it
        /// handles its events
        fn handle_events(&self) {
            let past = NSDate::distantPast();
            let mode = unsafe { NSDefaultRunLoopMode };
            while let Some(event) = self.app.nextEventMatchingMask_untilDate_inMode_dequeue(
                NSEventMask::Any,
                Some(&past),
                mode,
                true,
            ) {
                self.app.sendEvent(&event);
            }
        }
    }

    impl Drop for Overlay {
        fn drop(&mut self) {
            self.window.orderOut(None);
        }
    }

    /// a CGImage of premultiplied BGRA pixels
    struct Image(*mut c_void);

    impl Image {
        fn new(pixels: &[u8], width: i32, height: i32) -> Option<Self> {
            const PREMULTIPLIED_FIRST: u32 = 2;
            const BYTE_ORDER_32_LITTLE: u32 = 2 << 12;
            unsafe {
                // the image keeps its own copy of the pixels
                let data = CFDataCreate(std::ptr::null(), pixels.as_ptr(), pixels.len() as isize);
                if data.is_null() {
                    return None;
                }
                let provider = CGDataProviderCreateWithCFData(data);
                CFRelease(data);
                if provider.is_null() {
                    return None;
                }
                let space = CGColorSpaceCreateDeviceRGB();
                let image = CGImageCreate(
                    width as usize,
                    height as usize,
                    8,
                    32,
                    width as usize * 4,
                    space,
                    PREMULTIPLIED_FIRST | BYTE_ORDER_32_LITTLE,
                    provider,
                    std::ptr::null(),
                    false,
                    0,
                );
                CGColorSpaceRelease(space);
                CGDataProviderRelease(provider);
                (!image.is_null()).then_some(Self(image))
            }
        }

        /// CGImage is a Core Foundation type, which is also an Objective-C object
        fn as_object(&self) -> &AnyObject {
            unsafe { &*(self.0 as *const AnyObject) }
        }
    }

    impl Drop for Image {
        fn drop(&mut self) {
            unsafe { CGImageRelease(self.0) };
        }
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
        fn CGColorSpaceRelease(space: *mut c_void);
        fn CGDataProviderCreateWithCFData(data: *const c_void) -> *mut c_void;
        fn CGDataProviderRelease(provider: *mut c_void);
        fn CGImageCreate(
            width: usize,
            height: usize,
            bits_per_component: usize,
            bits_per_pixel: usize,
            bytes_per_row: usize,
            space: *mut c_void,
            bitmap_info: u32,
            provider: *mut c_void,
            decode: *const f64,
            should_interpolate: bool,
            intent: i32,
        ) -> *mut c_void;
        fn CGImageRelease(image: *mut c_void);
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFDataCreate(allocator: *const c_void, bytes: *const u8, length: isize)
        -> *const c_void;
        fn CFRelease(object: *const c_void);
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    pub(super) fn start(
        _rx: tokio::sync::mpsc::UnboundedReceiver<super::Command>,
    ) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glow_is_strongest_at_the_edge_and_grows_with_the_push() {
        let (width, height) = glow_size(0.5, 0.0);
        let pixels = render(Position::Left, width, height, 0.5, 0.0, 0.0);
        let alpha = |x: i32, y: i32| pixels[((y * width + x) * 4 + 3) as usize];
        let middle = height / 2;
        assert!(alpha(0, middle) > alpha(width - 1, middle));
        assert!(alpha(0, middle) > alpha(0, 0));
        assert!(glow_size(1.0, 0.0) > glow_size(0.0, 0.0));
        assert!(glow_size(1.0, 1.0) > glow_size(1.0, 0.0));
        // premultiplied: no channel exceeds alpha
        for pixel in pixels.chunks(4) {
            assert!(pixel[0] <= pixel[3] && pixel[1] <= pixel[3] && pixel[2] <= pixel[3]);
        }
    }

    #[test]
    fn glow_sits_on_the_edge_around_the_pointer() {
        let glow = |edge, anchor| Glow {
            edge,
            anchor,
            screen: Screen {
                left: -1440.0,
                top: 0.0,
                right: 0.0,
                bottom: 900.0,
            },
            progress: 0.0,
            flash: 0.0,
            phase: 0.0,
        };
        let (thickness, length) = glow_size(0.0, 0.0);
        assert_eq!(
            glow(Position::Right, (-1.0, 450.0)).placement(),
            (-thickness, 450 - length / 2, thickness, length)
        );
        assert_eq!(
            glow(Position::Left, (-1440.0, 450.0)).placement(),
            (-1440, 450 - length / 2, thickness, length)
        );
        // kept on the screen near its corners
        assert_eq!(glow(Position::Right, (-1.0, 5.0)).placement().1, 0);
        assert_eq!(
            glow(Position::Bottom, (-5.0, 899.0)).placement(),
            (-length, 900 - thickness, length, thickness)
        );
    }

    #[test]
    fn flash_fades_out_and_cancel_keeps_it() {
        let mut glow = Some(Glow {
            edge: Position::Right,
            anchor: (0.0, 0.0),
            screen: Screen {
                left: 0.0,
                top: 0.0,
                right: 100.0,
                bottom: 100.0,
            },
            progress: 1.0,
            flash: 1.0,
            phase: 0.0,
        });
        apply(&mut glow, Command::Cancel);
        assert!(glow.is_some(), "the flash plays to the end");
        for _ in 0..20 {
            animate(&mut glow);
        }
        assert!(glow.is_none(), "the flash faded out");
    }

    /// shows the real overlay at the left screen edge, then the flash
    #[cfg(windows)]
    #[test]
    #[ignore = "shows the glow on the screen for a second"]
    fn shows_and_hides_overlay() {
        use std::{thread::sleep, time::Duration};
        use windows::{
            Win32::{
                Foundation::RECT,
                UI::WindowsAndMessaging::{FindWindowW, GetWindowRect, IsWindowVisible},
            },
            core::w,
        };

        let indicator = EdgeIndicator::new(true);
        for step in 0..=10 {
            indicator.push(Position::Left, step as f64 / 10.0);
            sleep(Duration::from_millis(40));
        }
        let window =
            unsafe { FindWindowW(w!("lan-mouse-edge-indicator"), None) }.expect("overlay window");
        assert!(
            unsafe { IsWindowVisible(window) }.as_bool(),
            "glow not shown"
        );
        let mut rect = RECT::default();
        unsafe { GetWindowRect(window, &mut rect) }.unwrap();
        let (thickness, _) = glow_size(1.0, 0.0);
        assert_eq!(rect.right - rect.left, thickness, "full push, full size");

        indicator.crossed(Position::Left);
        sleep(Duration::from_millis(1200));
        assert!(
            !unsafe { IsWindowVisible(window) }.as_bool(),
            "flash did not fade out"
        );
    }

    /// renders a few states into a PNG next to the test binary, to look at
    #[test]
    #[ignore = "writes a preview image"]
    fn preview() {
        let states = [(0.0, 0.0), (0.5, 0.0), (1.0, 0.0), (1.0, 1.0), (1.0, 0.5)];
        let (max_width, max_height) = glow_size(1.0, 1.0);
        let gap = 40;
        let width = (max_width + gap) * states.len() as i32;
        let mut canvas = image::RgbaImage::from_pixel(
            width as u32,
            max_height as u32,
            image::Rgba([30, 30, 34, 255]),
        );
        for (n, (progress, flash)) in states.into_iter().enumerate() {
            let (w, h) = glow_size(progress, flash);
            let pixels = render(Position::Left, w, h, progress, flash, 1.5);
            let left = n as i32 * (max_width + gap);
            let top = (max_height - h) / 2;
            for y in 0..h {
                for x in 0..w {
                    let i = ((y * w + x) * 4) as usize;
                    let alpha = pixels[i + 3] as f32 / 255.0;
                    let background = canvas.get_pixel((left + x) as u32, (top + y) as u32).0;
                    // premultiplied "over"
                    let blend = |src: u8, dst: u8| (src as f32 + dst as f32 * (1.0 - alpha)) as u8;
                    canvas.put_pixel(
                        (left + x) as u32,
                        (top + y) as u32,
                        image::Rgba([
                            blend(pixels[i + 2], background[0]),
                            blend(pixels[i + 1], background[1]),
                            blend(pixels[i], background[2]),
                            255,
                        ]),
                    );
                }
            }
        }
        let path = std::env::temp_dir().join("lan-mouse-edge-indicator.png");
        canvas.save(&path).unwrap();
        println!("preview: {}", path.display());
    }
}
