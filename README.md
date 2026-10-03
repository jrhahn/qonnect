# qonnect

A Qobuz Connect controller for Linux. Browse your Qobuz library in the browser, pick your Connect
device, press play — the way Spotify Connect works.

The audio never passes through this program. `qonnect` only tells the renderer what to play; the
renderer fetches the stream from Qobuz itself, at full resolution, and keeps playing if you close
the browser or suspend the machine.

Built on the [`qobuz-connect`](https://github.com/ciaens/qobuz-connect) crate, whose protocol
schema was lifted from the official web player. Unofficial, and not affiliated with Qobuz.

## Status

Works: device list and selection, playlists, favourite albums, search, play a list from any
track, play/pause, next/previous, seek, volume, live state from the renderer, finding devices on
the LAN and handing them the session.

Not there yet: queue editing, shuffle and repeat, artist and label browsing.

**One catch, and it decides how you use this.** A device will join a session qonnect hands it,
take its queue and obey every transport command — but it will not stream. It answers every track
with `10001: Too many playback errors` and drops out. The tokens qonnect mints carry the app id
of the Qobuz web player, which is no Qobuz Connect controller, and Qobuz appears to tie the right
to stream on a user's behalf to the app the token belongs to. Signing in under another app id
does not help: the code exchange needs that app's own `private_key`.

So pick the device once in the official Qobuz app. It hands the device tokens that do stream, the
session belongs to your account rather than to either app, and qonnect drives it from there --
its own queue, its own transport, everything below. qonnect never takes a device that is already
in a session, exactly so it cannot replace those working tokens with its own.

## Install

Nix, natively, no container:

```sh
nix run github:youruser/qonnect     # or, in a checkout:
nix build && ./result/bin/qonnect
```

Without Nix, `cargo build --release` is enough. There are no C dependencies — TLS is rustls and
the protobuf schema ships generated, so `protoc` is not needed.

Linux, macOS and Windows: nothing here is tied to one of them. Only Linux is tested, because that
is the machine it was written on and the one with no Qobuz app of its own. The config lives at
`$XDG_CONFIG_HOME/qonnect/config`, else `~/.config/qonnect/config`, else `%APPDATA%\qonnect\config`,
and the sign-in opens a browser with whichever of `xdg-open`, `open` or `start` the system has.

## Configure

```sh
qonnect login
```

It reads the production app id out of the web player bundle, opens your browser at the Qobuz
sign-in page, catches the redirect on a local port, trades the code for a user auth token and
writes `~/.config/qonnect/config` with mode 600. Your password never passes through qonnect; the
browser handles the sign-in, exactly as it does for the web player.

Qobuz no longer accepts password logins over the API — `user/login` with an email and an md5
answers `401 User authentication is required` for every shape of the request. The browser
redirect is what the web player itself uses.

To skip the flow, take the two values out of the browser instead: open <https://play.qobuz.com>,
developer tools, tab **Network**, pick a request to `www.qobuz.com/api.json/0.2/...` and copy the
`X-App-Id` and `X-User-Auth-Token` request headers.

```ini
app_id = 798273057
user_auth_token = ey...

# Optional: make this renderer active as soon as it appears, by name.
renderer = Marantz PM7000N

# Optional, defaults to 127.0.0.1:7777.
bind = 127.0.0.1:7777
```

`QOBUZ_APP_ID`, `QOBUZ_USER_AUTH_TOKEN`, `QONNECT_RENDERER` and `QONNECT_BIND` override the file.

The token is your account. Keep the file to yourself, and do not put it in a repository.

## Run

```sh
qonnect
# qonnect on http://127.0.0.1:7777
```

Open that address. Your Connect devices appear in the dropdown at the bottom right as soon as
they announce themselves.

A HEOS device never registers itself with the Qobuz cloud. It advertises `_qobuz-connect._tcp` on
the LAN and waits for an app to hand it a session, so qonnect looks for one and offers it the
session -- but see the catch under Status: for playback, let the official app hand over first.

A Denon or Marantz device with HEOS Built-in needs a current firmware before it speaks Qobuz
Connect — on the PM7000N, HEOS firmware 3.67.460 or newer.

## Notes

- **Auto standby.** A sleeping device still answers on the LAN and still accepts a handover; it
  simply never joins and never plays. Hours of confusing symptoms come from this. Turn auto
  standby off while testing.
- **One token per socket.** The Qobuz cloud serves one connection per token. Running `qonnect`
  and the Qobuz phone app on the same account at the same time will evict one of them.
- **`qonnect` also announces itself as a renderer**, because joining a session always does. It
  shows up in the official apps as a device named "qonnect" and will not play anything if you
  pick it there.
- **Qobuz sits behind a Varnish** that answers some clients with a `403 Forbidden` and a "Guru
  Meditation" page. Both HTTP clients here send a browser user agent for that reason. A 403 in
  your own browser is usually a stale cookie: reload hard, or clear the Qobuz cookies.
- **Bind to localhost.** The server holds your credentials and does no authentication of its own.
  If you expose it on the LAN, anything on the LAN can use your Qobuz account.

## License

MIT
