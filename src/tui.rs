//! A terminal front end on the same core the web page uses: the catalogue from `qobuz`, the
//! session from `connect`. No HTTP server involved.

use std::sync::Arc;
use std::time::{Duration, Instant};

use qobuz_connect::proto::qconnect::PlayingState;
use qobuz_connect::ControllerCommand;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::connect::{Cmd, Handle, State};
use crate::qobuz::Qobuz;

/// Where a list of tracks comes from.
#[derive(Clone, PartialEq, Eq)]
enum Source {
    Playlist { id: String, name: String },
    Album { id: String, name: String },
}

impl Source {
    fn name(&self) -> &str {
        match self {
            Self::Playlist { name, .. } | Self::Album { name, .. } => name,
        }
    }
}

struct Track {
    id: u32,
    title: String,
    artist: String,
    seconds: u64,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Focus {
    Sources,
    Tracks,
}

pub async fn run(qobuz: Qobuz, session: Handle) -> std::io::Result<()> {
    let state = session.state.borrow().clone();
    let mut app = App {
        state,
        qobuz,
        session,
        sources: Vec::new(),
        source_at: ListState::default(),
        tracks: Vec::new(),
        track_at: ListState::default(),
        heading: "nothing loaded".to_owned(),
        focus: Focus::Sources,
        search: None,
        status: "loading your library…".to_owned(),
        reported: (0, Instant::now()),
    };
    app.load_sources().await;

    let mut terminal = ratatui::init();
    let result = app.main_loop(&mut terminal).await;
    ratatui::restore();
    result
}

struct App {
    qobuz: Qobuz,
    session: Handle,
    state: Arc<State>,
    sources: Vec<Source>,
    source_at: ListState,
    tracks: Vec<Track>,
    track_at: ListState,
    heading: String,
    focus: Focus,
    /// The query being typed, while it is being typed.
    search: Option<String>,
    status: String,
    /// The last position a renderer reported, and when it arrived.
    reported: (u64, Instant),
}

impl App {
    async fn main_loop(&mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        // crossterm reads blocking, so it gets a thread of its own rather than the runtime.
        let (keys, mut pressed) = mpsc::unbounded_channel();
        std::thread::spawn(move || {
            while let Ok(event) = event::read() {
                if keys.send(event).is_err() {
                    break;
                }
            }
        });
        let mut states = self.session.state.clone();

        loop {
            terminal.draw(|frame| self.draw(frame))?;
            tokio::select! {
                event = pressed.recv() => {
                    let Some(event) = event else { return Ok(()) };
                    if self.on_key(event).await {
                        return Ok(());
                    }
                }
                changed = states.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                    self.state = states.borrow_and_update().clone();
                    self.reported = (self.state.position_ms, Instant::now());
                }
                // Nothing happened, but the clock still has to move.
                () = tokio::time::sleep(Duration::from_millis(500)) => {}
            }
        }
    }

    /// Answers whether to quit.
    async fn on_key(&mut self, event: Event) -> bool {
        let Event::Key(key) = event else { return false };
        if key.kind != KeyEventKind::Press {
            return false;
        }
        if let Some(query) = self.search.as_mut() {
            match key.code {
                KeyCode::Esc => self.search = None,
                KeyCode::Backspace => {
                    query.pop();
                }
                KeyCode::Char(c) => query.push(c),
                KeyCode::Enter => {
                    let query = self.search.take().unwrap_or_default();
                    self.load_search(&query).await;
                }
                _ => {}
            }
            return false;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Sources => Focus::Tracks,
                    Focus::Tracks => Focus::Sources,
                };
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Enter => self.enter().await,
            KeyCode::Char(' ') => {
                let playing = if self.state.playing {
                    PlayingState::Paused
                } else {
                    PlayingState::Playing
                };
                self.control(ControllerCommand::SetPlayerState {
                    playing: Some(playing),
                    position: None,
                    queue_item_id: None,
                });
            }
            KeyCode::Char('n') => self.session.send(Cmd::Next),
            KeyCode::Char('p') => self.session.send(Cmd::Previous),
            KeyCode::Right => self.seek(10_000),
            KeyCode::Left => self.seek(-10_000),
            KeyCode::Char('+') => self.volume(5),
            KeyCode::Char('-') => self.volume(-5),
            KeyCode::Char('d') => self.next_device(),
            KeyCode::Char('r') => {
                self.session.send(Cmd::Discover);
                self.status = "looking for devices on the LAN…".to_owned();
            }
            KeyCode::Char('/') => self.search = Some(String::new()),
            _ => {}
        }
        false
    }

    fn move_by(&mut self, delta: isize) {
        let (at, len) = match self.focus {
            Focus::Sources => (&mut self.source_at, self.sources.len()),
            Focus::Tracks => (&mut self.track_at, self.tracks.len()),
        };
        if len == 0 {
            return;
        }
        let now = at.selected().unwrap_or(0) as isize;
        at.select(Some(now.saturating_add(delta).clamp(0, len as isize - 1) as usize));
    }

    async fn enter(&mut self) {
        match self.focus {
            Focus::Sources => {
                let Some(source) = self.source_at.selected().and_then(|at| self.sources.get(at))
                else {
                    return;
                };
                let source = source.clone();
                self.load_tracks(&source).await;
                self.focus = Focus::Tracks;
            }
            Focus::Tracks => {
                let Some(at) = self.track_at.selected() else {
                    return;
                };
                if self.tracks.is_empty() {
                    return;
                }
                self.session.send(Cmd::Play {
                    track_ids: self.tracks.iter().map(|track| track.id).collect(),
                    position: u32::try_from(at).unwrap_or(0),
                });
            }
        }
    }

    fn control(&self, command: ControllerCommand) {
        self.session.send(Cmd::Control(command));
    }

    fn seek(&mut self, delta_ms: i64) {
        let at = i64::try_from(self.position()).unwrap_or(0).saturating_add(delta_ms);
        self.control(ControllerCommand::SetPlayerState {
            playing: None,
            position: Some(Duration::from_millis(at.max(0).unsigned_abs())),
            queue_item_id: None,
        });
    }

    fn volume(&mut self, delta: i32) {
        let Some(renderer_id) = self.state.active else {
            self.status = "no device to set the volume on".to_owned();
            return;
        };
        self.control(ControllerCommand::ChangeVolume { renderer_id, delta });
    }

    /// Makes the next renderer in the list the active one.
    fn next_device(&mut self) {
        if self.state.renderers.is_empty() {
            self.status = "no devices in the session".to_owned();
            return;
        }
        let at = self
            .state
            .renderers
            .iter()
            .position(|renderer| Some(renderer.id) == self.state.active)
            .map_or(0, |at| (at + 1) % self.state.renderers.len());
        if let Some(renderer) = self.state.renderers.get(at) {
            self.control(ControllerCommand::SetActiveRenderer(renderer.id));
        }
    }

    /// The position to show: renderers report every few seconds, so fill the gaps.
    fn position(&self) -> u64 {
        let (reported, at) = self.reported;
        if self.state.playing {
            reported.saturating_add(u64::try_from(at.elapsed().as_millis()).unwrap_or(0))
        } else {
            reported
        }
    }

    // --- the catalogue ------------------------------------------------------------------------

    async fn load_sources(&mut self) {
        let mut sources = Vec::new();
        match self.qobuz.get("playlist/getUserPlaylists", &[("limit", "500")]).await {
            Ok(body) => sources.extend(items(&body, "playlists").iter().filter_map(|item| {
                Some(Source::Playlist {
                    id: id_of(item)?,
                    name: item.get("name")?.as_str()?.to_owned(),
                })
            })),
            Err(err) => self.status = err.to_string(),
        }
        match self
            .qobuz
            .get("favorite/getUserFavorites", &[("type", "albums"), ("limit", "500")])
            .await
        {
            Ok(body) => sources.extend(items(&body, "albums").iter().filter_map(|item| {
                let artist = item
                    .pointer("/artist/name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let title = item.get("title")?.as_str()?;
                Some(Source::Album {
                    id: id_of(item)?,
                    name: format!("{artist} — {title}"),
                })
            })),
            Err(err) => self.status = err.to_string(),
        }
        if !sources.is_empty() {
            self.source_at.select(Some(0));
            self.status = format!("{} playlists and albums", sources.len());
        }
        self.sources = sources;
    }

    async fn load_tracks(&mut self, source: &Source) {
        let answer = match source {
            Source::Playlist { id, .. } => {
                self.qobuz
                    .get(
                        "playlist/get",
                        &[("playlist_id", id), ("extra", "tracks"), ("limit", "500")],
                    )
                    .await
            }
            Source::Album { id, .. } => self.qobuz.get("album/get", &[("album_id", id)]).await,
        };
        match answer {
            Ok(body) => {
                self.tracks = items(&body, "tracks").iter().filter_map(track).collect();
                self.heading = source.name().to_owned();
                self.track_at.select((!self.tracks.is_empty()).then_some(0));
                self.status = format!("{} tracks", self.tracks.len());
            }
            Err(err) => self.status = err.to_string(),
        }
    }

    async fn load_search(&mut self, query: &str) {
        match self
            .qobuz
            .get("catalog/search", &[("query", query), ("limit", "50")])
            .await
        {
            Ok(body) => {
                self.tracks = items(&body, "tracks").iter().filter_map(track).collect();
                self.heading = format!("search: {query}");
                self.track_at.select((!self.tracks.is_empty()).then_some(0));
                self.focus = Focus::Tracks;
                self.status = format!("{} tracks", self.tracks.len());
            }
            Err(err) => self.status = err.to_string(),
        }
    }

    // --- drawing ------------------------------------------------------------------------------

    fn draw(&mut self, frame: &mut Frame) {
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(4),
        ])
        .areas(frame.area());
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(32), Constraint::Min(20)]).areas(body);

        let device = self
            .state
            .renderers
            .iter()
            .find(|renderer| Some(renderer.id) == self.state.active)
            .map_or("no device", |renderer| renderer.name.as_str());
        let connection = if self.state.connected {
            ""
        } else {
            "  reconnecting…"
        };
        frame.render_widget(
            Paragraph::new(Line::from(format!(" qonnect — {device}{connection}")))
                .style(Style::default().fg(Color::Black).bg(Color::Cyan)),
            header,
        );

        let sources: Vec<ListItem> = self
            .sources
            .iter()
            .map(|source| ListItem::new(source.name().to_owned()))
            .collect();
        frame.render_stateful_widget(
            List::new(sources)
                .block(pane("Playlists and albums", self.focus == Focus::Sources))
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                .highlight_symbol("▸ "),
            left,
            &mut self.source_at,
        );

        let playing = self.playing_track_id();
        let tracks: Vec<ListItem> = self
            .tracks
            .iter()
            .enumerate()
            .map(|(at, track)| {
                let line = format!(
                    "{:>3}. {:<40} {:<24} {}",
                    at + 1,
                    cut(&track.title, 40),
                    cut(&track.artist, 24),
                    clock(track.seconds * 1000)
                );
                let item = ListItem::new(line);
                if Some(track.id) == playing {
                    item.style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
                } else {
                    item
                }
            })
            .collect();
        frame.render_stateful_widget(
            List::new(tracks)
                .block(pane(&self.heading, self.focus == Focus::Tracks))
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                .highlight_symbol("▸ "),
            right,
            &mut self.track_at,
        );

        self.draw_footer(frame, footer);
    }

    fn draw_footer(&self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let [bar, keys] =
            Layout::vertical([Constraint::Length(3), Constraint::Length(1)]).areas(area);

        let at = self.position();
        let total = self.state.duration_ms;
        let title = self
            .playing_track_id()
            .and_then(|id| self.tracks.iter().find(|track| track.id == id))
            .map_or_else(
                || "—".to_owned(),
                |track| format!("{} · {}", track.title, track.artist),
            );
        let volume = self
            .state
            .volume
            .map_or_else(String::new, |volume| format!("   ♪ {volume}"));
        let label = format!(
            "{} {}  {} / {}{volume}",
            if self.state.playing { "▶" } else { "⏸" },
            cut(&title, 48),
            clock(at),
            clock(total)
        );
        frame.render_widget(
            Gauge::default()
                .block(Block::default().borders(Borders::ALL))
                .gauge_style(Style::default().fg(Color::Cyan))
                .ratio(if total == 0 {
                    0.0
                } else {
                    (at as f64 / total as f64).clamp(0.0, 1.0)
                })
                .label(label),
            bar,
        );

        let line = match &self.search {
            Some(query) => format!(" /{query}▏"),
            None => format!(
                " ␣ play  n/p track  ←→ seek  +/- volume  d device  r rescan  / search  q quit   {}",
                self.state.message.clone().unwrap_or_else(|| self.status.clone())
            ),
        };
        frame.render_widget(
            Paragraph::new(Line::from(line)).style(Style::default().fg(Color::DarkGray)),
            keys,
        );
    }

    /// The track the renderer is on, looked up through the queue.
    fn playing_track_id(&self) -> Option<u32> {
        let current = self.state.current?;
        self.state
            .queue
            .iter()
            .find(|item| item.queue_item_id == current)
            .map(|item| item.track_id)
    }
}

fn pane(title: &str, focused: bool) -> Block<'_> {
    let border = if focused { Color::Cyan } else { Color::DarkGray };
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(format!(" {title} "))
}

fn items<'a>(body: &'a Value, key: &str) -> &'a [Value] {
    body.pointer(&format!("/{key}/items"))
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// Playlist ids arrive as numbers, album ids as strings.
fn id_of(item: &Value) -> Option<String> {
    let id = item.get("id")?;
    id.as_str()
        .map(str::to_owned)
        .or_else(|| id.as_u64().map(|id| id.to_string()))
}

fn track(item: &Value) -> Option<Track> {
    Some(Track {
        id: u32::try_from(item.get("id")?.as_u64()?).ok()?,
        title: item.get("title")?.as_str()?.to_owned(),
        artist: ["/performer/name", "/album/artist/name", "/artist/name"]
            .iter()
            .find_map(|at| item.pointer(at).and_then(Value::as_str))
            .unwrap_or_default()
            .to_owned(),
        seconds: item.get("duration").and_then(Value::as_u64).unwrap_or(0),
    })
}

fn clock(ms: u64) -> String {
    let seconds = ms / 1000;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// Cut to a width in characters, not bytes: track titles are not ascii.
fn cut(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    text.chars().take(width.saturating_sub(1)).chain(['…']).collect()
}

#[cfg(test)]
mod tests {
    use super::{clock, cut, id_of, track};
    use serde_json::json;

    #[test]
    fn reads_a_track_whatever_the_artist_is_called() {
        let performer = json!({"id": 410_602_642, "title": "Bologna", "duration": 261,
                               "performer": {"name": "Wanda"}});
        let parsed = track(&performer).expect("a track");
        assert_eq!((parsed.id, parsed.artist.as_str(), parsed.seconds), (410_602_642, "Wanda", 261));

        // An album's own tracks name the artist one level further out.
        let album = json!({"id": 1, "title": "Columbo", "album": {"artist": {"name": "Wanda"}}});
        assert_eq!(track(&album).expect("a track").artist, "Wanda");

        // No title, no track: everything else can be missing.
        assert!(track(&json!({"id": 1})).is_none());
    }

    #[test]
    fn takes_ids_as_numbers_and_as_strings() {
        assert_eq!(id_of(&json!({"id": 12345})).as_deref(), Some("12345"));
        assert_eq!(id_of(&json!({"id": "kd6wl1pgdi050"})).as_deref(), Some("kd6wl1pgdi050"));
        assert_eq!(id_of(&json!({})), None);
    }

    #[test]
    fn formats_times_and_cuts_on_characters() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(261_877), "4:21");
        assert_eq!(cut("short", 40), "short");
        // Cutting on bytes would split the umlaut and panic.
        assert_eq!(cut("Grüße aus Köln", 6), "Grüße…");
    }
}
