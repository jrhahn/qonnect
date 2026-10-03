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
track, play/pause, next/previous, seek, volume, live state from the renderer.

Not there yet: queue editing, shuffle and repeat, artist and label browsing, LAN discovery.

## Install

Nix, natively, no container:

```sh
nix run github:youruser/qonnect     # or, in a checkout:
nix build && ./result/bin/qonnect
```

Without Nix, `cargo build --release` is enough. There are no C dependencies — TLS is rustls and
the protobuf schema ships generated, so `protoc` is not needed.

## Configure

```sh
qonnect login
```

It reads the production app id out of the web player bundle, asks for your email and password,
and writes `~/.config/qonnect/config` with mode 600. Only the token is stored; the password is
not kept.

If you would rather not hand over the password, take the two values from the browser instead:
open <https://play.qobuz.com>, developer tools, tab **Network**, pick a request to
`www.qobuz.com/api.json/0.2/...` and copy the `X-App-Id` and `X-User-Auth-Token` request headers.

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

A Denon or Marantz device with HEOS Built-in needs a current firmware before it speaks Qobuz
Connect — on the PM7000N, HEOS firmware 3.67.460 or newer.

## Notes

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
