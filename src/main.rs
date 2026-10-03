//! A Qobuz Connect controller with a local web UI: the browser shows the library, the renderer
//! plays it. Audio never passes through this process.

mod connect;
mod login;
mod qobuz;

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;

use axum::extract::{Path, Query, State as AxumState};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, Sse};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt as _;
use qobuz_connect::proto::qconnect::PlayingState;
use qobuz_connect::ControllerCommand;
use serde::Deserialize;
use serde_json::Value;
use tokio_stream::wrappers::WatchStream;

use crate::connect::{Cmd, Handle};
use crate::qobuz::Qobuz;

#[derive(Clone)]
struct App {
    qobuz: Qobuz,
    session: Handle,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "qonnect=info,warn".into()),
        )
        .init();

    if std::env::args().nth(1).as_deref() == Some("login") {
        let Some(path) = Config::path() else {
            eprintln!("no HOME and no XDG_CONFIG_HOME, nowhere to write the config");
            std::process::exit(1);
        };
        if let Err(err) = login::run(&path).await {
            eprintln!("{err}");
            std::process::exit(1);
        }
        return;
    }

    let config = match Config::load() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("{err}\n\nRun `qonnect login` to write one, or see the README.");
            std::process::exit(1);
        }
    };

    let qobuz = Qobuz::new(config.app_id, config.user_auth_token);
    let session = connect::spawn(qobuz.clone(), config.renderer);
    let app = App { qobuz, session };

    let router = Router::new()
        .route("/", get(index))
        .route("/api/state", get(state))
        .route("/api/events", get(events))
        .route("/api/playlists", get(playlists))
        .route("/api/playlist/{id}", get(playlist))
        .route("/api/favorites", get(favorites))
        .route("/api/album/{id}", get(album))
        .route("/api/search", get(search))
        .route("/api/renderer/{id}", post(renderer))
        .route("/api/play", post(play))
        .route("/api/transport", post(transport))
        .route("/api/volume", post(volume))
        .with_state(app);

    let listener = match tokio::net::TcpListener::bind(config.bind).await {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("cannot listen on {}: {err}", config.bind);
            std::process::exit(1);
        }
    };
    println!("qonnect on http://{}", config.bind);
    if let Err(err) = axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
    {
        eprintln!("server failed: {err}");
    }
}

// --- configuration ----------------------------------------------------------------------------

struct Config {
    app_id: String,
    user_auth_token: String,
    /// Renderer to make active as soon as it shows up, by name.
    renderer: Option<String>,
    bind: SocketAddr,
}

impl Config {
    /// Environment first, then `$XDG_CONFIG_HOME/qonnect/config`, a file of `key = value` lines.
    fn load() -> Result<Self, String> {
        let file = Self::path().and_then(|path| std::fs::read_to_string(path).ok());
        let mut values: HashMap<String, String> = HashMap::new();
        for line in file.as_deref().unwrap_or_default().lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                values.insert(
                    key.trim().to_owned(),
                    value.trim().trim_matches('"').to_owned(),
                );
            }
        }
        let value = |key: &str, env: &str| {
            std::env::var(env)
                .ok()
                .filter(|value| !value.is_empty())
                .or_else(|| values.get(key).cloned())
        };

        Ok(Self {
            app_id: value("app_id", "QOBUZ_APP_ID").ok_or("no app_id configured")?,
            user_auth_token: value("user_auth_token", "QOBUZ_USER_AUTH_TOKEN")
                .ok_or("no user_auth_token configured")?,
            renderer: value("renderer", "QONNECT_RENDERER"),
            bind: value("bind", "QONNECT_BIND")
                .unwrap_or_else(|| "127.0.0.1:7777".to_owned())
                .parse()
                .map_err(|err| format!("bad bind address: {err}"))?,
        })
    }

    fn path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
        Some(base.join("qonnect").join("config"))
    }
}

// --- the UI and the session -------------------------------------------------------------------

async fn index() -> Html<&'static str> {
    Html(include_str!("ui.html"))
}

async fn state(AxumState(app): AxumState<App>) -> Json<Value> {
    Json(serde_json::to_value(&**app.session.state.borrow()).unwrap_or(Value::Null))
}

/// Pushes the session state to the browser whenever it changes.
async fn events(
    AxumState(app): AxumState<App>,
) -> Sse<impl futures_util::Stream<Item = Result<SseEvent, Infallible>>> {
    let stream = WatchStream::new(app.session.state.clone())
        .map(|state| Ok(SseEvent::default().json_data(&*state).unwrap_or_default()));
    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

// --- the catalogue ----------------------------------------------------------------------------

type ApiResult = Result<Json<Value>, (StatusCode, String)>;

fn fail(err: reqwest::Error) -> (StatusCode, String) {
    (StatusCode::BAD_GATEWAY, err.to_string())
}

async fn playlists(AxumState(app): AxumState<App>) -> ApiResult {
    app.qobuz
        .get("playlist/getUserPlaylists", &[("limit", "500")])
        .await
        .map(Json)
        .map_err(fail)
}

async fn playlist(AxumState(app): AxumState<App>, Path(id): Path<String>) -> ApiResult {
    app.qobuz
        .get(
            "playlist/get",
            &[
                ("playlist_id", id.as_str()),
                ("extra", "tracks"),
                ("limit", "500"),
            ],
        )
        .await
        .map(Json)
        .map_err(fail)
}

async fn favorites(AxumState(app): AxumState<App>) -> ApiResult {
    app.qobuz
        .get(
            "favorite/getUserFavorites",
            &[("type", "albums"), ("limit", "500")],
        )
        .await
        .map(Json)
        .map_err(fail)
}

async fn album(AxumState(app): AxumState<App>, Path(id): Path<String>) -> ApiResult {
    app.qobuz
        .get("album/get", &[("album_id", id.as_str())])
        .await
        .map(Json)
        .map_err(fail)
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
}

async fn search(AxumState(app): AxumState<App>, Query(query): Query<SearchQuery>) -> ApiResult {
    app.qobuz
        .get(
            "catalog/search",
            &[("query", query.q.as_str()), ("limit", "50")],
        )
        .await
        .map(Json)
        .map_err(fail)
}

// --- playback ---------------------------------------------------------------------------------

async fn renderer(AxumState(app): AxumState<App>, Path(id): Path<i32>) -> impl IntoResponse {
    app.session
        .send(Cmd::Control(ControllerCommand::SetActiveRenderer(id)));
    StatusCode::ACCEPTED
}

#[derive(Deserialize)]
struct Play {
    track_ids: Vec<u32>,
    #[serde(default)]
    position: u32,
}

async fn play(AxumState(app): AxumState<App>, Json(body): Json<Play>) -> impl IntoResponse {
    app.session.send(Cmd::Play {
        track_ids: body.track_ids,
        position: body.position,
    });
    StatusCode::ACCEPTED
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
enum Transport {
    Play,
    Pause,
    Next,
    Previous,
    Seek { position_ms: u64 },
}

async fn transport(
    AxumState(app): AxumState<App>,
    Json(body): Json<Transport>,
) -> impl IntoResponse {
    let set = |playing, position| {
        Cmd::Control(ControllerCommand::SetPlayerState {
            playing,
            position,
            queue_item_id: None,
        })
    };
    app.session.send(match body {
        Transport::Play => set(Some(PlayingState::Playing), None),
        Transport::Pause => set(Some(PlayingState::Paused), None),
        Transport::Next => Cmd::Next,
        Transport::Previous => Cmd::Previous,
        Transport::Seek { position_ms } => {
            set(None, Some(std::time::Duration::from_millis(position_ms)))
        }
    });
    StatusCode::ACCEPTED
}

#[derive(Deserialize)]
struct Volume {
    volume: u32,
}

async fn volume(AxumState(app): AxumState<App>, Json(body): Json<Volume>) -> impl IntoResponse {
    let Some(renderer_id) = app.session.state.borrow().active else {
        return StatusCode::CONFLICT;
    };
    app.session.send(Cmd::Control(ControllerCommand::SetVolume {
        renderer_id,
        volume: body.volume,
    }));
    StatusCode::ACCEPTED
}
