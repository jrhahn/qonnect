# qonnect

A Qobuz Connect controller you run yourself. Browse your Qobuz library, pick your streamer, press
play — what the Qobuz app does on a phone, from a machine that has no Qobuz app.

The audio never passes through qonnect. It only tells the streamer what to play; the streamer
pulls the stream from Qobuz itself, at full resolution, and keeps playing when you close the page
or suspend the machine.

Unofficial, and not affiliated with Qobuz. Built on the
[`qobuz-connect`](https://github.com/ciaens/qobuz-connect) crate, whose protocol schema was
lifted from the official web player.

## How it works

Qobuz Connect has three parts, and the interesting one is invisible from outside.

**The session** lives in the Qobuz cloud. Controllers and renderers connect to it over a
WebSocket and agree on a queue, a position and which renderer is active. qonnect joins it as a
controller, and as a renderer it never uses — joining always announces a device.

**The renderer** is your streamer. A phone registers itself with the cloud on its own. A HEOS
device, such as a Denon or Marantz, never does: it advertises `_qobuz-connect._tcp` on the LAN
and waits for an app to walk up and hand it a session, together with the tokens to join it with.
No app, no device — which is why a controller that only talks to the cloud sees nothing at all.
qonnect does that LAN handshake itself.

**The catalogue** is plain HTTP. qonnect reads your playlists, favourites and searches straight
from the Qobuz API and sends the resulting track ids into the session. It never resolves a stream
URL, because it never plays anything.

```
    your browser ──HTTP──► qonnect ──WebSocket──► Qobuz cloud session ◄──WebSocket── streamer
                              │                                                          ▲
                              ├──HTTP──► Qobuz API (playlists, search)                    │
                              └──HTTP──► streamer on the LAN (hand over the session) ─────┘

                                         audio: Qobuz ─────────────────────────────► streamer
```

## The catch

A device will join a session qonnect hands it, take a queue and obey every transport command —
and then refuse to play. Every track ends in `10001: Too many playback errors` and the device
leaves. The tokens qonnect mints carry the app id of the Qobuz **web player**, which is not a
Connect controller, and Qobuz appears to tie the right to stream on a user's behalf to the app a
token was issued for. Signing in under another app id does not help: the code exchange needs that
app's own `private_key`.

So let the official Qobuz app hand the device its session once. It passes tokens that do stream.
From then on qonnect drives that device completely — its own queue, its own transport — because
the session is shared, not owned by either app. qonnect never takes over a device that is already
in a session, precisely so it cannot replace the only tokens that work.

If you know which app the native handover mints its tokens under, please open an issue.

## Quick start

```sh
qonnect login          # browser sign-in, writes the config
qonnect                # the terminal UI
qonnect serve          # the same thing as a page on http://127.0.0.1:7777
```

The first time, and after the device has been off: open the Qobuz app on your phone, pick the
device there and start anything. That is the handover from the catch above. Then put the phone
away.

```
 qonnect — Marantz PM7000N
┌ Playlists and albums ──────┐┌ Amore — Wanda ───────────────────────────────────┐
│▸ 80er-Pop-Hits             ││   1. Bologna                 Wanda           4:21 │
│  Jazz at Night             ││ ▸ 2. Columbo                 Wanda           3:47 │
│  Wanda — Amore             ││   3. Niente                  Wanda           3:12 │
└────────────────────────────┘└──────────────────────────────────────────────────┘
┌──────────────────────────────────────────────────────────────────────────────┐
│████████████████  ▶ Columbo · Wanda   1:28 / 3:47   ♪ 20                       │
└──────────────────────────────────────────────────────────────────────────────┘
 ␣ play  n/p track  ←→ seek  +/- volume  d device  r rescan  / search  q quit
```

`Tab` switches panes, `Enter` opens a playlist or starts a track and everything after it, `d`
steps through the devices in the session.

The page is the same core with a different front end — handy from another room, or a phone.

## Install

```sh
nix run github:jrhahn/qonnect        # or, in a checkout:
nix build && ./result/bin/qonnect
```

Without Nix, `cargo build --release`. No C dependencies: TLS is rustls, and the protobuf schema
ships generated, so `protoc` is not needed either. If `cargo` and `rustc` on your machine come
from different places — a distribution cargo next to a rustup `rustc` shim, say — the build dies
in a dependency with `E0514: compiled by an incompatible version of rustc`. `nix develop -c cargo
build`, or `rustup run stable cargo build`, gives it a matching pair.

Linux, macOS and Windows: nothing here is tied to one of them, though only Linux is tested. The
browser opens with whichever of `xdg-open`, `open` or `start` exists, and the config lives at
`$XDG_CONFIG_HOME/qonnect/config`, else `~/.config/qonnect/config`, else
`%APPDATA%\qonnect\config`.

## Sign in

```sh
qonnect login
```

It reads the production app id out of the web player bundle, opens your browser at the Qobuz
sign-in page, catches the redirect on a local port, trades the code for a user auth token and
writes the config with mode 600. Your password never passes through qonnect: the browser handles
the sign-in, exactly as it does for the web player.

Qobuz no longer accepts password logins over the API. `user/login` with an email and an md5
answers `401 User authentication is required` for every shape of the request, so the browser
redirect is the only way in — and it is what the web player itself uses.

To skip the flow, take the two values out of the browser instead: open <https://play.qobuz.com>,
developer tools, tab **Network**, pick a request to `www.qobuz.com/api.json/0.2/...` and copy the
`X-App-Id` and `X-User-Auth-Token` request headers into the config.

```ini
app_id = 798273057
user_auth_token = ey...

# Optional: make this renderer active as soon as it appears, by name.
renderer = Marantz PM7000N

# Optional, defaults to 127.0.0.1:7777.
bind = 127.0.0.1:7777
```

`QOBUZ_APP_ID`, `QOBUZ_USER_AUTH_TOKEN`, `QONNECT_RENDERER` and `QONNECT_BIND` override the file.
The token is your account: keep the file to yourself, and out of any repository.

## What works

Device discovery and selection, playlists, favourite albums, search, playing a list from any
track, play and pause, next and previous, seek, volume, live position from the renderer, and the
LAN handover — from the terminal or from the page, whichever you start.

Not yet: queue editing, shuffle and repeat, browsing by artist or label.

## Things that cost us hours

- **Auto standby.** A sleeping device still answers on the LAN and still accepts a handover. It
  simply never joins and never plays. Turn auto standby off before you debug anything else.
- **The session is not your account's.** Connect before the device and you land in a different
  session and see no renderers at all. qonnect watches for this: when a device on the LAN reports
  another session, it drops its own and joins again.
- **One socket per token.** The cloud serves one connection per token. qonnect mints its own, so
  it coexists with the phone app, but two copies of qonnect sharing a config will not.
- **qonnect shows up as a device** in the official apps, because joining a session always
  announces one. It will not play if you pick it there.
- **A Varnish in front of Qobuz** answers some clients with `403 Forbidden` and a "Guru
  Meditation" page, so both HTTP clients here send a browser user agent. A 403 in your own
  browser is usually a stale cookie.
- **HEOS firmware.** Denon and Marantz need a recent one before they speak Connect at all; on the
  PM7000N, HEOS 3.67.460 or newer.
- **Bind to localhost.** The server holds your credentials and authenticates nobody. On the LAN,
  anything on the LAN can use your Qobuz account.

## Protocol notes

Measured against a Marantz PM7000N (HEOS 3.139.173, `sdk_version=1.1.0-b840`), in case they save
somebody else the afternoon.

The device advertises `_qobuz-connect._tcp` with `path`, `device_uuid`, `type` and
`sdk_version` — and no `Name`, so a browser has to fall back to the mDNS instance name. Three
calls live under that path: `GET get-display-info`, `GET get-connect-info` (which reports the
session it is in, empty when it is in none) and `POST connect-to-qconnect`.

The handover body is validated strictly. Both tokens need an `endpoint`, the expiry is in
seconds, and any unknown field is answered with `400 Invalid request structure`:

```json
{
  "session_id": "<session uuid, hyphenated>",
  "jwt_qconnect": {"endpoint": "wss://qws-eu-prod.qobuz.com/ws", "jwt": "...", "exp": 1791018336},
  "jwt_api": {"endpoint": "https://www.qobuz.com/api.json/0.2", "jwt": "...", "exp": 1791018336},
  "become_active": true
}
```

`jwt_qconnect` comes from `qws/createToken` with `jwt=jwt_qws`, the only value that endpoint
accepts. `jwt_api` comes from `qws/refreshToken` with `jwt=jwt_api`; the user auth token is not a
substitute, and a device given one holds the queue, reports itself as playing and sits at
position zero forever.

An empty JSON body takes the device's HTTP server down for about ten seconds.

## The code

| | |
|---|---|
| `src/qobuz.rs` | the Qobuz HTTP API: tokens and catalogue. Never resolves a stream URL |
| `src/lan.rs` | the LAN handshake, from the app side: browse, then hand the session over |
| `src/connect.rs` | one task owning the session, folding its events into the state the UI sees |
| `src/login.rs` | the browser sign-in |
| `src/tui.rs` | the terminal UI, on the same core |
| `src/main.rs` | config, HTTP routes, server-sent events |
| `src/ui.html` | the page, compiled in, no assets to serve |

## License

MIT
