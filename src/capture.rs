use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    rc::Rc,
    time::{Duration, Instant},
};

use futures::StreamExt;
use input_capture::{
    CaptureError, CaptureEvent, CaptureHandle, InputCapture, InputCaptureError, Position,
};
use input_event::{Event, KeyboardEvent, PointerEvent, scancode};
use lan_mouse_proto::ProtoEvent;
use local_channel::mpsc::{Receiver, Sender, channel};
use tokio::task::{JoinHandle, spawn_local};
use tokio_util::sync::CancellationToken;

use crate::{
    connect::{ConnectionEvent, LanMouseConnection},
    cursor::{self, CursorHider},
    edge_indicator::EdgeIndicator,
    game::GameState,
};
use lan_mouse_ipc::MacKeys;

/// how often to check whether clients became ready or unreachable
const READY_CHECK_INTERVAL: Duration = Duration::from_secs(1);
/// first and longest pause between attempts to reach an unreachable client
const RECONNECT_DELAY_MIN: Duration = Duration::from_secs(1);
const RECONNECT_DELAY_MAX: Duration = Duration::from_secs(10);

pub(crate) struct Capture {
    cancellation_token: CancellationToken,
    request_tx: Sender<CaptureRequest>,
    task: JoinHandle<()>,
    event_rx: Receiver<ICaptureEvent>,
}

pub(crate) enum ICaptureEvent {
    /// the mouse crossed the edge of the given capture, with where along
    /// the edge (0..1) it crossed
    CaptureBegin(CaptureHandle, Option<f64>),
    /// capture disabled
    CaptureDisabled,
    /// capture disabled
    CaptureEnabled,
    /// A (new) client was entered.
    /// In contrast to [`ICaptureEvent::CaptureBegin`] this
    /// event is only triggered when the capture was
    /// explicitly released in the meantime by
    /// either the remote client leaving its device region,
    /// a new device entering the screen or the release bind.
    ClientEntered(u64),
    /// The previously active client was left, i.e. capture
    /// was released for the given handle. Mirrors
    /// [`ICaptureEvent::ClientEntered`] for the leave side
    /// and fires on every release path (release-bind chord,
    /// remote `Leave`, explicit `Release` request, send
    /// failure, or destroy of the active capture).
    ClientLeft(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureType {
    /// a normal input capture
    Default,
    /// A capture only interested in [`CaptureEvent::Begin`] events.
    /// The capture is released immediately, if there is no
    /// Default capture at the same position.
    EnterOnly,
}

#[derive(Clone, Debug)]
enum CaptureRequest {
    /// capture must release the mouse
    Release,
    /// add a capture client
    Create(CaptureHandle, Position, CaptureType),
    /// destory a capture client
    Destroy(CaptureHandle),
    /// reenable input capture
    Reenable,
    /// set release bind
    SetReleaseBind(Vec<scancode::Linux>),
    /// change how keys are adapted for Macs
    SetMacKeys(MacKeys),
    /// scroll the other way on other devices, or not
    SetReverseScroll(bool),
}

/// how capturing behaves, from the config
pub(crate) struct CaptureOptions {
    pub(crate) release_bind: Vec<scancode::Linux>,
    pub(crate) mac_keys: MacKeys,
    /// how far the mouse has to push against an edge to cross over (px)
    pub(crate) edge_resistance: u32,
    /// scroll the other way on other devices
    pub(crate) reverse_scroll: bool,
}

impl Capture {
    pub(crate) fn new(
        backend: Option<input_capture::Backend>,
        conn: LanMouseConnection,
        options: CaptureOptions,
        game: GameState,
        cursor: CursorHider,
        indicator: EdgeIndicator,
    ) -> Self {
        let CaptureOptions {
            release_bind,
            mac_keys,
            edge_resistance,
            reverse_scroll,
        } = options;
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let cancellation_token = CancellationToken::new();
        let capture_task = CaptureTask {
            active_client: None,
            armed: Default::default(),
            reconnect: Default::default(),
            game,
            mac_keys: mac_keys.into(),
            edge_resistance: edge_resistance as f64,
            edge_push: None,
            enter_along: None,
            reverse_scroll,
            cursor,
            indicator,
            backend,
            cancellation_token: cancellation_token.clone(),
            captures: Default::default(),
            conn,
            event_tx,
            request_rx,
            release_bind: Rc::new(RefCell::new(release_bind)),
            state: Default::default(),
        };
        let task = spawn_local(capture_task.run());
        Self {
            cancellation_token,
            request_tx,
            task,
            event_rx,
        }
    }

    pub(crate) fn reenable(&self) {
        self.request_tx
            .send(CaptureRequest::Reenable)
            .expect("channel closed");
    }

    pub(crate) async fn terminate(&mut self) {
        self.cancellation_token.cancel();
        log::debug!("terminating capture");
        if let Err(e) = (&mut self.task).await {
            log::warn!("{e}");
        }
    }

    pub(crate) fn create(
        &self,
        handle: CaptureHandle,
        pos: lan_mouse_ipc::Position,
        capture_type: CaptureType,
    ) {
        let pos = to_capture_pos(pos);
        self.request_tx
            .send(CaptureRequest::Create(handle, pos, capture_type))
            .expect("channel closed");
    }

    pub(crate) fn destroy(&self, handle: CaptureHandle) {
        self.request_tx
            .send(CaptureRequest::Destroy(handle))
            .expect("channel closed");
    }

    pub(crate) fn release(&self) {
        self.request_tx
            .send(CaptureRequest::Release)
            .expect("channel closed");
    }

    pub(crate) async fn event(&mut self) -> ICaptureEvent {
        self.event_rx.recv().await.expect("channel closed")
    }

    pub(crate) fn set_release_bind(&mut self, bind: Vec<scancode::Linux>) {
        let _ = self.request_tx.send(CaptureRequest::SetReleaseBind(bind));
    }

    pub(crate) fn set_mac_keys(&self, mac_keys: MacKeys) {
        let _ = self.request_tx.send(CaptureRequest::SetMacKeys(mac_keys));
    }

    pub(crate) fn set_reverse_scroll(&self, reverse: bool) {
        let _ = self
            .request_tx
            .send(CaptureRequest::SetReverseScroll(reverse));
    }
}

/// debounce a statement `$st`, i.e. the statement is executed only if the
/// time since the previous execution is at least `$dur`.
/// `$prev` is used to keep track of this timestamp
macro_rules! debounce {
    ($prev:ident, $dur:expr, $st:stmt) => {
        let exec = match $prev.get() {
            None => true,
            Some(instant) if instant.elapsed() > $dur => true,
            _ => false,
        };
        if exec {
            $prev.replace(Some(Instant::now()));
            $st
        }
    };
}

struct CaptureTask {
    active_client: Option<CaptureHandle>,
    /// [`CaptureType::Default`] captures with a barrier in the backend.
    /// A client is only armed while it is connected and accepts input, so
    /// an unreachable client never grabs the mouse and a lost connection
    /// hands it back right away.
    armed: HashSet<CaptureHandle>,
    /// next attempt and current back-off for reaching unarmed clients
    reconnect: HashMap<CaptureHandle, (Instant, Duration)>,
    /// no client is armed while a game is in the foreground
    game: GameState,
    /// adapts keys for clients where exactly one side is a Mac
    mac_keys: KeyAdapter,
    /// how far the mouse has to push against an edge to cross over (px)
    edge_resistance: f64,
    edge_push: Option<EdgePush>,
    /// where along the edge the mouse crossed over (0..1)
    enter_along: Option<f64>,
    /// scroll the other way on other devices
    reverse_scroll: bool,
    /// hides the local pointer while the mouse is on another device
    cursor: CursorHider,
    /// glows at the edge the mouse pushes against
    indicator: EdgeIndicator,
    backend: Option<input_capture::Backend>,
    cancellation_token: CancellationToken,
    captures: Vec<(CaptureHandle, Position, CaptureType)>,
    conn: LanMouseConnection,
    event_tx: Sender<ICaptureEvent>,
    release_bind: Rc<RefCell<Vec<scancode::Linux>>>,
    request_rx: Receiver<CaptureRequest>,
    state: State,
}

impl CaptureTask {
    fn add_capture(&mut self, handle: CaptureHandle, pos: Position, capture_type: CaptureType) {
        self.captures.push((handle, pos, capture_type));
    }

    fn remove_capture(&mut self, handle: CaptureHandle) {
        self.captures.retain(|&(h, ..)| handle != h);
        self.reconnect.remove(&handle);
    }

    fn is_default_capture_at(&self, pos: Position) -> bool {
        self.captures
            .iter()
            .any(|&(h, p, t)| p == pos && t == CaptureType::Default && self.armed.contains(&h))
    }

    /// create the barrier for `handle`, now that its client is ready
    async fn arm(
        &mut self,
        capture: &mut InputCapture,
        handle: CaptureHandle,
    ) -> Result<(), CaptureError> {
        if self.armed.insert(handle) {
            log::info!("client {handle} is ready, capturing at its screen edge");
            self.reconnect.remove(&handle);
            capture.create(handle, self.get_pos(handle)).await?;
        }
        Ok(())
    }

    /// adapt an event to the conventions of the device it is sent to
    fn adapt_event(&self, handle: CaptureHandle, event: Event) -> Event {
        match event {
            Event::Keyboard(event) if self.adapts_mac_keys(handle) => {
                Event::Keyboard(self.mac_keys.event(event))
            }
            Event::Pointer(PointerEvent::Axis { time, axis, value }) if self.reverse_scroll => {
                Event::Pointer(PointerEvent::Axis {
                    time,
                    axis,
                    value: -value,
                })
            }
            Event::Pointer(PointerEvent::AxisDiscrete120 { axis, value })
                if self.reverse_scroll =>
            {
                Event::Pointer(PointerEvent::AxisDiscrete120 {
                    axis,
                    value: -value,
                })
            }
            event => event,
        }
    }

    /// keys are adapted when exactly one side is a Mac
    fn adapts_mac_keys(&self, handle: CaptureHandle) -> bool {
        self.mac_keys.enabled
            && self
                .conn
                .peer_os(handle)
                .is_some_and(|peer_os| (peer_os == "macos") != cfg!(target_os = "macos"))
    }

    /// remove the barrier for `handle`, giving the mouse back if it was there
    async fn disarm(
        &mut self,
        capture: &mut InputCapture,
        handle: CaptureHandle,
    ) -> Result<(), CaptureError> {
        if self.active_client == Some(handle) {
            self.release_capture(capture).await?;
        }
        if self.armed.remove(&handle) {
            capture.destroy(handle).await?;
        }
        Ok(())
    }

    /// arm clients that became ready, disarm unreachable ones and keep
    /// trying to reach those, with growing pauses in between.
    /// While a game is in the foreground, no client is armed.
    async fn update_armed(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        let now = Instant::now();
        let paused = self.game.active();
        let defaults = self
            .captures
            .iter()
            .filter(|&&(_, _, t)| t == CaptureType::Default)
            .map(|&(h, ..)| h)
            .collect::<Vec<_>>();
        for handle in defaults {
            let reachable = self.conn.is_ready(handle).await;
            let ready = reachable && !paused;
            let armed = self.armed.contains(&handle);
            if ready && !armed {
                self.arm(capture, handle).await?;
            } else if !ready && armed {
                if paused {
                    log::info!(
                        "game in the foreground, releasing the screen edge of client {handle}"
                    );
                } else {
                    log::info!(
                        "client {handle} stopped accepting input, releasing its screen edge"
                    );
                }
                self.disarm(capture, handle).await?;
            } else if !reachable {
                let (next, delay) = self
                    .reconnect
                    .entry(handle)
                    .or_insert((now, RECONNECT_DELAY_MIN));
                if now >= *next {
                    self.conn.connect(handle).await;
                    *next = now + *delay;
                    *delay = (*delay * 2).min(RECONNECT_DELAY_MAX);
                }
            }
        }
        Ok(())
    }

    fn get_pos(&self, handle: CaptureHandle) -> Position {
        self.captures
            .iter()
            .find(|(h, ..)| *h == handle)
            .expect("no such capture")
            .1
    }

    fn get_type(&self, handle: CaptureHandle) -> CaptureType {
        self.captures
            .iter()
            .find(|(h, ..)| *h == handle)
            .expect("no such capture")
            .2
    }

    async fn run(mut self) {
        loop {
            if let Err(e) = self.do_capture().await {
                log::warn!("input capture exited: {e}");
            }
            loop {
                tokio::select! {
                    r = self.request_rx.recv() => match r.expect("channel closed") {
                        CaptureRequest::Reenable => break,
                        CaptureRequest::Create(h, p, t) => self.add_capture(h, p, t),
                        CaptureRequest::Destroy(h) => self.remove_capture(h),
                        CaptureRequest::Release => { /* nothing to do */ }
                        CaptureRequest::SetReleaseBind(bind) => {
                            self.release_bind.borrow_mut().clone_from(&bind);
                        }
                        CaptureRequest::SetMacKeys(mac_keys) => self.mac_keys = mac_keys.into(),
                        CaptureRequest::SetReverseScroll(reverse) => self.reverse_scroll = reverse,
                    },
                    _ = self.cancellation_token.cancelled() => return,
                }
            }
        }
    }

    async fn do_capture(&mut self) -> Result<(), InputCaptureError> {
        /* allow cancelling capture request */
        let mut capture = tokio::select! {
            r = InputCapture::new(self.backend) => r?,
            _ = self.cancellation_token.cancelled() => return Ok(()),
        };

        let _capture_guard = DropGuard::new(
            self.event_tx.clone(),
            ICaptureEvent::CaptureEnabled,
            ICaptureEvent::CaptureDisabled,
        );

        /* create barriers for active clients */
        let r = self.create_captures(&mut capture).await;
        if let Err(e) = r {
            capture.terminate().await?;
            return Err(e.into());
        }

        let r = self.do_capture_session(&mut capture).await;

        // FIXME replace with async drop when stabilized
        capture.terminate().await?;

        r
    }

    async fn create_captures(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        // clients get armed by `update_armed` once they are ready
        self.armed.clear();
        let captures = self.captures.clone();
        for (handle, pos, capture_type) in captures {
            if capture_type == CaptureType::Default {
                continue;
            }
            tokio::select! {
                r = capture.create(handle, pos) => r?,
                _ = self.cancellation_token.cancelled() => return Ok(()),
            }
        }
        Ok(())
    }

    async fn do_capture_session(
        &mut self,
        capture: &mut InputCapture,
    ) -> Result<(), InputCaptureError> {
        // the first tick fires right away
        let mut ready_check = tokio::time::interval(READY_CHECK_INTERVAL);
        ready_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                event = capture.next() => match event {
                    Some(event) => self.handle_capture_event(capture, event?).await?,
                    None => return Ok(()),
                },
                _ = ready_check.tick() => self.update_armed(capture).await?,
                _ = self.game.changed() => self.update_armed(capture).await?,
                _ = edge_push_idle(&self.edge_push) => {
                    // stopped pushing against the edge before crossing over
                    self.edge_push = None;
                    self.release_capture(capture).await?;
                }
                event = self.conn.recv() => {
                    let (handle, event) = match event {
                        ConnectionEvent::Event(handle, event) => (handle, event),
                        ConnectionEvent::Disconnected(handle) => {
                            if self.armed.contains(&handle) {
                                log::info!("lost connection to client {handle}, releasing its screen edge");
                                self.disarm(capture, handle).await?;
                            }
                            continue;
                        }
                    };
                    if let Some(active) = self.active_client {
                        if handle != active {
                            // we only care about events coming from the client we are currently connected to
                            // only `Ack` and `Leave` are relevant
                            continue
                        }
                    }

                    match event {
                        // connection acknowlegded => set state to Sending
                        ProtoEvent::Ack(_) => {
                            log::info!("client {handle} acknowledged the connection!");
                            self.state = State::Sending;
                        }
                        // client disconnected
                        ProtoEvent::Leave(_, along) => {
                            log::info!("releasing capture: left remote client device region");
                            // the pointer comes back at the height / width it left the other device
                            let edge = self.active_client.map(|h| to_proto_pos(self.get_pos(h)));
                            self.release_capture(capture).await?;
                            if let (Some(edge), Some(along)) = (edge, along) {
                                cursor::move_to_edge(edge, along);
                            }
                        },
                        _ => {}
                    }
                },
                e = self.request_rx.recv() => match e.expect("channel closed") {
                    CaptureRequest::Reenable => { /* already active */ },
                    CaptureRequest::Release => self.release_capture(capture).await?,
                    CaptureRequest::Create(h, p, t) => {
                        self.add_capture(h, p, t);
                        match t {
                            // start connecting now, `update_armed` arms it once ready
                            CaptureType::Default => self.conn.connect(h).await,
                            CaptureType::EnterOnly => capture.create(h, p).await?,
                        }
                    }
                    CaptureRequest::Destroy(h) => {
                        // If the capture we're tearing down is the
                        // currently-active one, treat this as a
                        // release for hook purposes. The release_capture
                        // path also clears active_client and flushes
                        // pressed-key state to the peer; without this,
                        // `cli deactivate` (or a hostname change
                        // re-creating the client) would skip leave_hook.
                        if self.active_client == Some(h) {
                            self.release_capture(capture).await?;
                        }
                        let has_barrier = self.armed.remove(&h)
                            || self
                                .captures
                                .iter()
                                .any(|&(c, _, t)| c == h && t == CaptureType::EnterOnly);
                        self.remove_capture(h);
                        if has_barrier {
                            capture.destroy(h).await?;
                        }
                    }
                    CaptureRequest::SetReleaseBind(bind) => {
                        self.release_bind.borrow_mut().clone_from(&bind);
                    }
                    CaptureRequest::SetMacKeys(mac_keys) => {
                        // release first, so keys held down are released with the old mapping
                        self.release_capture(capture).await?;
                        self.mac_keys = mac_keys.into();
                    }
                    CaptureRequest::SetReverseScroll(reverse) => self.reverse_scroll = reverse,
                },
                _ = self.cancellation_token.cancelled() => break,
            }
        }
        Ok(())
    }

    async fn handle_capture_event(
        &mut self,
        capture: &mut InputCapture,
        event: (CaptureHandle, CaptureEvent),
    ) -> Result<(), CaptureError> {
        let (handle, mut event) = event;
        log::trace!("({handle}): {event:?}");

        if capture.keys_pressed(&self.release_bind.borrow()) {
            log::info!("releasing capture: release-bind pressed");
            return self.release_capture(capture).await;
        }

        let pos = self.get_pos(handle);
        // enter only capture (for incoming connections)
        let enter_only = self.get_type(handle) == CaptureType::EnterOnly;
        // every capture at an edge gets each event: an edge with a client is
        // handled through the client's capture, so it is pushed only once
        if enter_only && self.is_default_capture_at(pos) {
            return Ok(());
        }

        // resist crossing over: only switch to the client, or hand an incoming
        // device's mouse back to it, once the mouse kept pushing against the
        // edge, so brushing it does nothing
        if self.active_client != Some(handle) && self.edge_resistance > 0.0 {
            match event {
                CaptureEvent::Begin => {
                    self.edge_push = Some(EdgePush {
                        handle,
                        distance: 0.0,
                        last_progress: Instant::now(),
                    });
                    self.indicator.push(self.get_pos(handle), 0.0);
                    return Ok(());
                }
                CaptureEvent::Input(Event::Pointer(PointerEvent::Motion { dx, dy, .. })) => {
                    let pos = self.get_pos(handle);
                    let outward = outward_motion(pos, dx, dy);
                    let Some(push) = self.edge_push.as_mut().filter(|push| push.handle == handle)
                    else {
                        return self.release_capture(capture).await;
                    };
                    push.distance += outward;
                    if outward > 0.0 {
                        push.last_progress = Instant::now();
                    }
                    if push.distance < -EDGE_PUSH_BACKOFF {
                        // moved away from the edge
                        self.edge_push = None;
                        return self.release_capture(capture).await;
                    }
                    if push.distance < self.edge_resistance {
                        self.indicator
                            .push(pos, push.distance / self.edge_resistance);
                        return Ok(());
                    }
                    log::debug!(
                        "pushed {:.0}px against the edge, crossing over",
                        push.distance
                    );
                    self.edge_push = None;
                    event = CaptureEvent::Begin;
                }
                // letting go of a key or button is no reason to stay, nor is a
                // modifier update (sent along when another device releases its mouse)
                CaptureEvent::Input(Event::Keyboard(
                    KeyboardEvent::Key { state: 0, .. } | KeyboardEvent::Modifiers { .. },
                ))
                | CaptureEvent::Input(Event::Pointer(PointerEvent::Button { state: 0, .. })) => {
                    return Ok(());
                }
                // clicking, scrolling or typing: the user wants to stay here
                _ => {
                    self.edge_push = None;
                    return self.release_capture(capture).await;
                }
            }
        }

        if event == CaptureEvent::Begin {
            self.hand_back_incoming(pos);
        }

        if enter_only {
            // no client at this edge, the mouse stays on this device
            log::info!("releasing capture: no active client at this position");
            if event == CaptureEvent::Begin {
                self.indicator.crossed(pos);
            } else {
                self.indicator.cancel();
            }
            return capture.release().await;
        }

        // activated a new client
        if event == CaptureEvent::Begin && Some(handle) != self.active_client {
            self.state = State::WaitingForAck;
            self.active_client.replace(handle);
            // the other device enters at the same relative height / width
            self.enter_along = cursor::position_along_edge(to_proto_pos(self.get_pos(handle)));
            self.cursor.hide();
            self.indicator.crossed(self.get_pos(handle));
            self.event_tx
                .send(ICaptureEvent::ClientEntered(handle))
                .expect("channel closed");
        }

        let opposite_pos = to_proto_pos(self.get_pos(handle).opposite());

        let event = match event {
            CaptureEvent::Begin => ProtoEvent::Enter(opposite_pos, self.enter_along),
            CaptureEvent::Input(e) => match self.state {
                // connection not acknowledged, repeat `Enter` event
                State::WaitingForAck => ProtoEvent::Enter(opposite_pos, self.enter_along),
                State::Sending => ProtoEvent::Input(self.adapt_event(handle, e)),
            },
        };

        if let Err(e) = self.conn.send(event, handle).await {
            const DUR: Duration = Duration::from_millis(500);
            debounce!(PREV_LOG, DUR, log::warn!("releasing capture: {e}"));
            // Funnel through release_capture (via disarm) so the leave_hook
            // fires and active_client is cleared (without this the
            // active_client field would stay stale until the next
            // Begin from a different handle). Disarming keeps the
            // edge from grabbing the mouse again until the client is
            // ready, instead of re-capturing on every touch.
            self.disarm(capture, handle).await?;
        }
        Ok(())
    }

    /// the mouse crossed the edge at `pos`: incoming devices there get their
    /// mouse back (the service sends them `Leave`), at the same height / width
    fn hand_back_incoming(&self, pos: Position) {
        let along = cursor::position_along_edge(to_proto_pos(pos));
        for &(handle, ..) in self
            .captures
            .iter()
            .filter(|&&(_, p, t)| p == pos && t == CaptureType::EnterOnly)
        {
            self.event_tx
                .send(ICaptureEvent::CaptureBegin(handle, along))
                .expect("channel closed");
        }
    }

    async fn release_capture(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        self.edge_push = None;
        self.indicator.cancel();
        self.cursor.show();
        // If we have an active client, notify them we're leaving
        if let Some(handle) = self.active_client.take() {
            // Surface the leave to the service layer so it can fire
            // the per-client leave_hook. Sent before the network
            // teardown below so we never race against the peer
            // disappearing.
            self.event_tx
                .send(ICaptureEvent::ClientLeft(handle))
                .expect("channel closed");
            // Synthesize key-up events for every key still held in the
            // capture's pressed_keys set BEFORE sending Leave. Without
            // this, pressing the release-bind chord (typically all four
            // modifiers) leaves the peer with phantom held modifiers:
            // the down events were forwarded while capture was active,
            // but the matching up events arrive after the local tap
            // flips to passthrough and never reach the peer. The peer
            // then runs every subsequent keystroke through those held
            // mods until its watchdog times out (1+ s) or our Leave
            // arrives — and Leave can be lost over UDP/DTLS.
            for key in capture.take_pressed_keys() {
                // adapted like the key-down was, so a swapped Ctrl/Cmd can't get stuck
                let key_up = ProtoEvent::Input(self.adapt_event(
                    handle,
                    Event::Keyboard(KeyboardEvent::Key {
                        time: 0,
                        key: key as u32,
                        state: 0,
                    }),
                ));
                if let Err(e) = self.conn.send(key_up, handle).await {
                    log::warn!("failed to send key-up to client {handle}: {e}");
                }
            }
            // Reset the modifier mask too. The peer's input-emulation
            // layer keeps a separate XKB-style modifier state that's
            // updated by KeyboardEvent::Modifiers, distinct from the
            // pressed_keys set drained above. Without this, an
            // already-locked CapsLock would survive the release.
            let mods_zero = ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Modifiers {
                depressed: 0,
                latched: 0,
                locked: 0,
                group: 0,
            }));
            if let Err(e) = self.conn.send(mods_zero, handle).await {
                log::warn!("failed to reset modifiers on client {handle}: {e}");
            }

            log::info!("sending Leave event to client {handle}");
            if let Err(e) = self.conn.send(ProtoEvent::Leave(0, None), handle).await {
                log::warn!("failed to send Leave to client {handle}: {e}");
            }
        }
        capture.release().await
    }
}

thread_local! {
    static PREV_LOG: Cell<Option<Instant>> = const { Cell::new(None) };
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    WaitingForAck,
    Sending,
}

/// how far the mouse may move back from the edge (px) before a push is cancelled
const EDGE_PUSH_BACKOFF: f64 = 2.0;
/// a push is cancelled when the mouse stops pushing against the edge for this long
const EDGE_PUSH_IDLE: Duration = Duration::from_millis(300);

/// the mouse pushing against a screen edge, not yet far enough to cross over
struct EdgePush {
    handle: CaptureHandle,
    /// how far the mouse was pushed past the edge (px)
    distance: f64,
    /// when the mouse last moved outward
    last_progress: Instant,
}

/// part of a motion going out through the screen edge at `pos` (px)
fn outward_motion(pos: Position, dx: f64, dy: f64) -> f64 {
    match pos {
        Position::Left => -dx,
        Position::Right => dx,
        Position::Top => -dy,
        Position::Bottom => dy,
    }
}

/// resolves once an unfinished push against an edge has been idle for too long
async fn edge_push_idle(push: &Option<EdgePush>) {
    match push {
        Some(push) => tokio::time::sleep_until((push.last_progress + EDGE_PUSH_IDLE).into()).await,
        None => std::future::pending().await,
    }
}

/// XKB modifier masks with the evdev scancode of their left key
const MODIFIERS: [(u32, u32); 4] = [
    (scancode::Linux::KeyLeftShift as u32, 1 << 0),
    (scancode::Linux::KeyLeftCtrl as u32, 1 << 2),
    (scancode::Linux::KeyLeftAlt as u32, 1 << 3),
    (scancode::Linux::KeyLeftMeta as u32, 1 << 6),
];

/// XKB modifier mask of a modifier key
fn modifier_mask(key: u32) -> Option<u32> {
    use scancode::Linux::{
        KeyLeftAlt, KeyLeftCtrl, KeyLeftMeta, KeyLeftShift, KeyRightCtrl, KeyRightShift,
        KeyRightalt, KeyRightmeta,
    };
    match scancode::Linux::try_from(key).ok()? {
        KeyLeftShift | KeyRightShift => Some(1 << 0),
        KeyLeftCtrl | KeyRightCtrl => Some(1 << 2),
        KeyLeftAlt | KeyRightalt => Some(1 << 3),
        KeyLeftMeta | KeyRightmeta => Some(1 << 6),
        _ => None,
    }
}

/// Adapts keyboard events sent to a device where exactly one side is a Mac,
/// e.g. swapping Ctrl and Cmd so shortcuts use the key each side is used to.
struct KeyAdapter {
    enabled: bool,
    /// key on this device -> key sent to the other device
    map: HashMap<u32, u32>,
}

impl From<MacKeys> for KeyAdapter {
    fn from(mac_keys: MacKeys) -> Self {
        Self {
            enabled: mac_keys.enabled,
            map: mac_keys.map.into_iter().collect(),
        }
    }
}

impl KeyAdapter {
    fn event(&self, event: KeyboardEvent) -> KeyboardEvent {
        match event {
            KeyboardEvent::Key { time, key, state } => KeyboardEvent::Key {
                time,
                key: self.key(key),
                state,
            },
            KeyboardEvent::Modifiers {
                depressed,
                latched,
                locked,
                group,
            } => KeyboardEvent::Modifiers {
                depressed: self.mask(depressed),
                latched: self.mask(latched),
                locked: self.mask(locked),
                group,
            },
        }
    }

    fn key(&self, key: u32) -> u32 {
        self.map.get(&key).copied().unwrap_or(key)
    }

    /// move modifier bits along with their remapped keys
    fn mask(&self, mask: u32) -> u32 {
        let all = MODIFIERS.iter().fold(0, |all, (_, bit)| all | bit);
        let mut adapted = mask & !all;
        for (key, bit) in MODIFIERS {
            if mask & bit != 0 {
                // a modifier mapped to a regular key no longer counts as a modifier
                adapted |= modifier_mask(self.key(key)).unwrap_or(0);
            }
        }
        adapted
    }
}

fn to_capture_pos(pos: lan_mouse_ipc::Position) -> input_capture::Position {
    match pos {
        lan_mouse_ipc::Position::Left => input_capture::Position::Left,
        lan_mouse_ipc::Position::Right => input_capture::Position::Right,
        lan_mouse_ipc::Position::Top => input_capture::Position::Top,
        lan_mouse_ipc::Position::Bottom => input_capture::Position::Bottom,
    }
}

fn to_proto_pos(pos: input_capture::Position) -> lan_mouse_proto::Position {
    match pos {
        input_capture::Position::Left => lan_mouse_proto::Position::Left,
        input_capture::Position::Right => lan_mouse_proto::Position::Right,
        input_capture::Position::Top => lan_mouse_proto::Position::Top,
        input_capture::Position::Bottom => lan_mouse_proto::Position::Bottom,
    }
}

struct DropGuard<T> {
    tx: Sender<T>,
    on_drop: Option<T>,
}

impl<T> DropGuard<T> {
    fn new(tx: Sender<T>, on_new: T, on_drop: T) -> Self {
        tx.send(on_new).expect("channel closed");
        let on_drop = Some(on_drop);
        Self { tx, on_drop }
    }
}

impl<T> Drop for DropGuard<T> {
    fn drop(&mut self) {
        self.tx
            .send(self.on_drop.take().expect("item"))
            .expect("channel closed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scancode::Linux::{
        KeyA, KeyCapsLock, KeyLeftAlt, KeyLeftCtrl, KeyLeftMeta, KeyRightCtrl, KeyRightmeta,
    };

    fn key(key: scancode::Linux, state: u8) -> KeyboardEvent {
        KeyboardEvent::Key {
            time: 0,
            key: key as u32,
            state,
        }
    }

    const SHIFT: u32 = 1 << 0;
    const CONTROL: u32 = 1 << 2;
    const ALT: u32 = 1 << 3;
    const META: u32 = 1 << 6;

    #[test]
    fn default_swaps_ctrl_and_cmd() {
        let adapter = KeyAdapter::from(MacKeys::default());
        assert_eq!(adapter.event(key(KeyLeftCtrl, 1)), key(KeyLeftMeta, 1));
        assert_eq!(adapter.event(key(KeyLeftMeta, 0)), key(KeyLeftCtrl, 0));
        assert_eq!(adapter.event(key(KeyRightCtrl, 1)), key(KeyRightmeta, 1));
        assert_eq!(adapter.event(key(KeyRightmeta, 1)), key(KeyRightCtrl, 1));
        assert_eq!(adapter.event(key(KeyA, 1)), key(KeyA, 1));

        assert_eq!(adapter.mask(CONTROL), META);
        assert_eq!(adapter.mask(META | SHIFT), CONTROL | SHIFT);
        assert_eq!(adapter.mask(CONTROL | META), CONTROL | META);
        assert_eq!(adapter.mask(SHIFT), SHIFT);
    }

    #[test]
    fn custom_mapping() {
        // Alt becomes Cmd, Caps Lock becomes Ctrl, everything else stays
        let adapter = KeyAdapter::from(MacKeys {
            enabled: true,
            map: vec![
                (KeyLeftAlt as u32, KeyLeftMeta as u32),
                (KeyCapsLock as u32, KeyLeftCtrl as u32),
            ],
        });
        assert_eq!(adapter.event(key(KeyLeftAlt, 1)), key(KeyLeftMeta, 1));
        assert_eq!(adapter.event(key(KeyCapsLock, 1)), key(KeyLeftCtrl, 1));
        assert_eq!(adapter.event(key(KeyLeftCtrl, 1)), key(KeyLeftCtrl, 1));
        assert_eq!(adapter.mask(ALT | CONTROL), META | CONTROL);
    }

    #[test]
    fn edge_push_direction() {
        assert_eq!(outward_motion(Position::Left, -5.0, 1.0), 5.0);
        assert_eq!(outward_motion(Position::Left, 3.0, 0.0), -3.0);
        assert_eq!(outward_motion(Position::Right, 4.0, 9.0), 4.0);
        assert_eq!(outward_motion(Position::Top, 9.0, -2.0), 2.0);
        assert_eq!(outward_motion(Position::Bottom, 0.0, 6.0), 6.0);
    }
}
