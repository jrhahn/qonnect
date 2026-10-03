//! The Qobuz Connect session, owned by one task, driven by commands from the HTTP layer.
//!
//! The session is both a renderer and a controller: joining always announces this device, so
//! "qonnect" shows up in the official apps as well. It never plays anything, it only steers the
//! renderer the user picked.

use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::sync::Arc;
use std::time::Duration;

use qobuz_connect::proto::qconnect::{AudioQuality, DeviceType, PlayingState};
use qobuz_connect::{
    Autoplay, ControllerCommand, Device, Event, QueueEvent, RendererEvent, Session,
};
use serde::Serialize;
use tokio::sync::{mpsc, watch};

use crate::qobuz::Qobuz;

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Renderer {
    pub id: i32,
    pub name: String,
    pub brand: String,
    pub model: String,
    pub kind: String,
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct QueueItem {
    pub queue_item_id: i32,
    pub track_id: u32,
}

/// Everything the browser is shown about the session.
#[derive(Serialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct State {
    pub connected: bool,
    /// The session this controller is in, as the LAN handshake spells it.
    pub session_id: Option<String>,
    pub renderers: Vec<Renderer>,
    pub active: Option<i32>,
    pub playing: bool,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub current: Option<i32>,
    pub volume: Option<u32>,
    pub queue: Vec<QueueItem>,
    pub message: Option<String>,
}

impl State {
    /// Folds one session event into the state. Pure, so the queue arithmetic below is testable.
    pub fn apply(&mut self, event: &Event) {
        match event {
            Event::Disconnected => self.connected = false,
            Event::Reconnected | Event::Registered { .. } => {
                self.connected = true;
                self.message = None;
            }
            Event::Session(session) => {
                self.active = session.active_renderer_id;
                self.playing = session.playing == PlayingState::Playing;
            }
            Event::Renderer(RendererEvent::Added { id, device })
            | Event::Renderer(RendererEvent::Updated { id, device }) => {
                let renderer = Renderer {
                    id: *id,
                    name: device.name.clone(),
                    brand: device.brand.clone(),
                    model: device.model.clone(),
                    kind: format!("{:?}", device.kind),
                };
                match self.renderers.iter_mut().find(|r| r.id == *id) {
                    Some(slot) => *slot = renderer,
                    None => self.renderers.push(renderer),
                }
            }
            Event::Renderer(RendererEvent::Removed { id }) => {
                self.renderers.retain(|r| r.id != *id);
                if self.active == Some(*id) {
                    self.active = None;
                }
            }
            Event::Renderer(RendererEvent::ActiveChanged { id }) => self.active = *id,
            Event::Renderer(RendererEvent::StateUpdated { id, state, .. }) => {
                if self.active == Some(*id) {
                    if let Some(state) = state {
                        self.playing = state.playing == PlayingState::Playing;
                        self.position_ms = ms(state.position);
                        self.duration_ms = ms(state.duration);
                        self.current = state.current_queue_item_id;
                    }
                }
            }
            Event::Renderer(RendererEvent::Volume { id, volume }) => {
                if self.active == Some(*id) {
                    self.volume = Some(*volume);
                }
            }
            Event::Queue(QueueEvent::Loaded(loaded)) => {
                self.queue = loaded
                    .tracks
                    .iter()
                    .map(|track| QueueItem {
                        queue_item_id: track.queue_item_id,
                        track_id: track.track_id,
                    })
                    .collect();
            }
            Event::Queue(QueueEvent::State(queue)) => {
                self.queue = queue
                    .tracks
                    .iter()
                    .map(|track| QueueItem {
                        queue_item_id: track.queue_item_id,
                        track_id: track.track_id,
                    })
                    .collect();
            }
            Event::Queue(QueueEvent::Cleared(_)) => self.queue.clear(),
            Event::Error { code, message } => self.message = Some(format!("{code}: {message}")),
            Event::PlaybackError(error) => self.message = Some(format!("{error:?}")),
            _ => {}
        }
    }

    fn step(&self, delta: isize) -> Option<i32> {
        let current = self.current?;
        let at = self
            .queue
            .iter()
            .position(|item| item.queue_item_id == current)?;
        let next = at.checked_add_signed(delta)?;
        self.queue.get(next).map(|item| item.queue_item_id)
    }
}

/// A uuid as the LAN handshake spells it, `None` while the session has none yet.
fn hyphenated(uuid: &[u8]) -> Option<String> {
    let bytes: [u8; 16] = uuid.try_into().ok()?;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let mut out = String::with_capacity(36);
    for (at, char) in hex.chars().enumerate() {
        if [8, 12, 16, 20].contains(&at) {
            out.push('-');
        }
        out.push(char);
    }
    Some(out)
}

fn ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// How long to wait before offering the session to a silent device again, and the ceiling that
/// wait grows to.
const FIRST_RETRY: Duration = Duration::from_secs(20);
const LAST_RETRY: Duration = Duration::from_secs(600);

/// What the HTTP layer asks of the session.
#[derive(Debug)]
pub enum Cmd {
    Control(ControllerCommand),
    /// Look for devices on the LAN and hand each one this session.
    Discover,
    Next,
    Previous,
    /// Replaces the queue and starts at `position`.
    Play { track_ids: Vec<u32>, position: u32 },
}

#[derive(Clone)]
pub struct Handle {
    tx: mpsc::UnboundedSender<Cmd>,
    pub state: watch::Receiver<Arc<State>>,
}

impl Handle {
    pub fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }
}

/// Starts the session task. It reconnects on its own, so this never fails.
pub fn spawn(qobuz: Qobuz, prefer: Option<String>) -> Handle {
    let (tx, rx) = mpsc::unbounded_channel();
    let (state_tx, state_rx) = watch::channel(Arc::new(State::default()));
    tokio::spawn(run(qobuz, prefer, rx, state_tx));
    Handle {
        tx,
        state: state_rx,
    }
}

async fn run(
    qobuz: Qobuz,
    prefer: Option<String>,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
    state_tx: watch::Sender<Arc<State>>,
) {
    loop {
        let mint = {
            let qobuz = qobuz.clone();
            move || {
                let qobuz = qobuz.clone();
                async move { qobuz.token().await }
            }
        };
        let mut session = match Session::join_with(mint, device()).await {
            Ok(session) => session,
            Err(err) => {
                tracing::warn!("join failed: {err}");
                let state = State {
                    message: Some(err.to_string()),
                    ..State::default()
                };
                let _ = state_tx.send(Arc::new(state));
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };
        let mut state = State {
            connected: true,
            ..State::default()
        };
        let _ = state_tx.send(Arc::new(state.clone()));
        let _ = session.ask_queue_state();
        // Which queue item to start once the server has assigned the ids.
        let mut start_at: Option<u32> = None;
        // A device that is asleep answers the handover and never joins, so offer it again --
        // but backing off, because every offer costs the device a reconnection.
        let mut backoff = FIRST_RETRY;
        let mut retry_at = tokio::time::Instant::now();
        // Renderers do not report their position on their own, so ask.
        let mut poll = tokio::time::interval(Duration::from_secs(5));

        loop {
            tokio::select! {
                event = session.recv() => {
                    let Some(event) = event else { break };
                    tracing::debug!(?event);
                    let known = state.active;
                    state.apply(&event);
                    if !state.renderers.is_empty() {
                        backoff = FIRST_RETRY;
                    }
                    state.session_id = hyphenated(session.session_uuid());

                    // LoadTracks only fills the queue; the chosen track still has to be started,
                    // and its queue item id only exists once the server has answered.
                    if let (Some(index), Event::Queue(QueueEvent::Loaded(_))) = (start_at, &event) {
                        start_at = None;
                        if let Some(item) = state.queue.get(index as usize) {
                            let _ = session.control(jump(item.queue_item_id));
                        }
                    }
                    // Pick up where the last run left off, the way Spotify Connect remembers.
                    if known.is_none() && state.active.is_none() {
                        if let (Some(wanted), Event::Renderer(RendererEvent::Added { id, device })) =
                            (prefer.as_deref(), &event)
                        {
                            if device.name.eq_ignore_ascii_case(wanted) {
                                let _ = session.control(ControllerCommand::SetActiveRenderer(*id));
                            }
                        }
                    }
                    let _ = state_tx.send(Arc::new(state.clone()));
                }
                _ = poll.tick(), if state.active.is_some() => {
                    if let Some(id) = state.active {
                        let _ = session.ask_renderer_state(id);
                    }
                }
                // A LAN device stays invisible until it is handed the session.
                () = tokio::time::sleep_until(retry_at), if state.renderers.is_empty() => {
                    if let Some(session_id) = state.session_id.clone() {
                        spawn_handover(qobuz.clone(), session_id);
                    }
                    retry_at = tokio::time::Instant::now() + backoff;
                    backoff = (backoff * 2).min(LAST_RETRY);
                }
                cmd = rx.recv() => {
                    let Some(cmd) = cmd else { return };
                    let command = match cmd {
                        Cmd::Control(command) => Some(command),
                        Cmd::Next => state.step(1).map(jump),
                        Cmd::Previous => state.step(-1).map(jump),
                        Cmd::Discover => {
                            if let Some(session_id) = state.session_id.clone() {
                                spawn_handover(qobuz.clone(), session_id);
                            }
                            None
                        }
                        Cmd::Play { track_ids, position } => {
                            start_at = Some(position);
                            Some(ControllerCommand::LoadTracks {
                                track_ids,
                                position,
                                shuffle_seed: None,
                                shuffle_pivot_index: None,
                                autoplay: Autoplay::default(),
                            })
                        }
                    };
                    if let Some(command) = command {
                        if let Err(err) = session.control(command) {
                            tracing::warn!("control failed: {err}");
                            break;
                        }
                    }
                }
            }
        }
        tracing::warn!("session ended, rejoining");
        let _ = state_tx.send(Arc::new(State::default()));
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Hands every device on the LAN this session, in the background: browsing takes seconds and
/// the session must keep reading its events meanwhile.
fn spawn_handover(qobuz: Qobuz, session_id: String) {
    tokio::spawn(async move {
        match crate::lan::browse(Duration::from_secs(4)).await {
            Ok(devices) if devices.is_empty() => {
                tracing::info!("no Qobuz Connect devices on the LAN");
            }
            Ok(devices) => {
                for device in devices {
                    if let Err(err) = crate::lan::hand_over(&qobuz, &device, &session_id).await {
                        tracing::warn!("{err}");
                    }
                }
            }
            Err(err) => tracing::warn!("{err}"),
        }
    });
}

/// Start a queue item from its beginning.
fn jump(queue_item_id: i32) -> ControllerCommand {
    ControllerCommand::SetPlayerState {
        playing: Some(PlayingState::Playing),
        position: Some(Duration::ZERO),
        queue_item_id: Some(queue_item_id),
    }
}

fn device() -> Device {
    let name = "qonnect";
    Device {
        uuid: uuid(name),
        name: name.to_owned(),
        brand: "qonnect".to_owned(),
        model: "linux controller".to_owned(),
        kind: DeviceType::Computer,
        max_audio_quality: AudioQuality::HiresLevel3,
        volume_remote_control: false,
        software_version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

/// Stable across restarts, so the session does not collect a new device every time.
fn uuid(name: &str) -> [u8; 16] {
    let half = |salt: u8| {
        let mut hasher = DefaultHasher::new();
        name.hash(&mut hasher);
        salt.hash(&mut hasher);
        hasher.finish().to_be_bytes()
    };
    let (low, high) = (half(0), half(1));
    let mut uuid = [0; 16];
    let (first, second) = uuid.split_at_mut(8);
    first.copy_from_slice(&low);
    second.copy_from_slice(&high);
    uuid
}

#[cfg(test)]
mod tests {
    use qobuz_connect::proto::qconnect::{
        BufferState, QueueTrack, RendererStatus, SrvrCtrlQueueTracksLoaded,
    };
    use qobuz_connect::PlayerState;

    use super::*;

    fn renderer(id: i32, name: &str) -> Event {
        Event::Renderer(RendererEvent::Added {
            id,
            device: Device {
                uuid: [0; 16],
                name: name.to_owned(),
                brand: "Marantz".to_owned(),
                model: "PM7000N".to_owned(),
                kind: DeviceType::Streamer,
                max_audio_quality: AudioQuality::HiresLevel2,
                volume_remote_control: true,
                software_version: "1".to_owned(),
            },
        })
    }

    fn loaded(ids: &[(i32, u32)]) -> Event {
        Event::Queue(QueueEvent::Loaded(SrvrCtrlQueueTracksLoaded {
            tracks: ids
                .iter()
                .map(|(queue_item_id, track_id)| QueueTrack {
                    queue_item_id: *queue_item_id,
                    track_id: *track_id,
                })
                .collect(),
            ..Default::default()
        }))
    }

    fn playing(id: i32, item: i32) -> Event {
        Event::Renderer(RendererEvent::StateUpdated {
            id,
            status: RendererStatus::Unknown,
            state: Some(PlayerState {
                playing: PlayingState::Playing,
                buffer: BufferState::Unknown,
                position: Duration::from_secs(3),
                duration: Duration::from_secs(200),
                current_queue_item_id: Some(item),
                next_queue_item_id: None,
            }),
        })
    }

    #[test]
    fn follows_a_renderer_through_a_queue() {
        let mut state = State::default();
        assert_eq!(hyphenated(&[0x12, 0x34]), None);
        assert_eq!(
            hyphenated(&[0x0a, 0x1b, 0x2c, 0x3d, 0x4e, 0x5f, 0x60, 0x71,
                         0x82, 0x93, 0xa4, 0xb5, 0xc6, 0xd7, 0xe8, 0xf9]).as_deref(),
            Some("0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9")
        );
        state.apply(&renderer(7, "Marantz PM7000N"));
        assert_eq!(state.renderers.len(), 1);

        // A report from a renderer that is not active must not move the player.
        state.apply(&playing(7, 11));
        assert_eq!(state.current, None);

        state.apply(&Event::Renderer(RendererEvent::ActiveChanged { id: Some(7) }));
        state.apply(&loaded(&[(11, 101), (12, 102), (13, 103)]));
        state.apply(&playing(7, 12));
        assert!(state.playing);
        assert_eq!(state.position_ms, 3_000);
        assert_eq!(state.current, Some(12));

        assert_eq!(state.step(1), Some(13));
        assert_eq!(state.step(-1), Some(11));

        // No wrapping and no panic at either end.
        state.apply(&playing(7, 13));
        assert_eq!(state.step(1), None);
        state.apply(&playing(7, 11));
        assert_eq!(state.step(-1), None);

        state.apply(&Event::Renderer(RendererEvent::Removed { id: 7 }));
        assert!(state.renderers.is_empty());
        assert_eq!(state.active, None);
    }
}
