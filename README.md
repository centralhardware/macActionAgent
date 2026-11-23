# macActionAgent

A tiny macOS LaunchAgent that lets a machine on the private network run shell commands on
the Mac and stream their output back over a plain TCP socket.

- Runs each command with `zsh -lc` as the logged-in user, inside the GUI session, so
  `osascript`, `open`, `pbcopy`/`pbpaste`, `screencapture` and `shortcuts run` all work.
- **No token.** The only gate is the peer address, which must fall inside
  `AllowedNetworks`. Don't expose the port publicly.
- One command per connection. Output is streamed as it comes, stdin can be fed at any
  time, and dropping the connection kills the command's whole process group.

## Protocol

Newline-delimited JSON on TCP port 8765.

| dir | frame |
|---|---|
| → | `{"cmd":"pmset -g batt","timeout":600,"cwd":null,"shell":null}` (timeout `0` = none) |
| ← | `{"t":"start","pid":123}` |
| ← | `{"t":"out","d":"…"}` / `{"t":"err","d":"…"}` (streamed) |
| → | `{"t":"in","d":"…"}` / `{"t":"eof"}` / `{"t":"kill"}` (any time) |
| ← | `{"t":"exit","code":0,"signal":null,"timed_out":false,"killed":false,"duration_ms":12}` |
| ← | `{"t":"error","message":"…"}` (forbidden, bad request, spawn failure) |

```sh
printf '{"cmd":"sw_vers"}\n{"t":"eof"}\n' | nc 10.8.0.4 8765
```

## Settings

`defaults write me.centralhardware.mac-action-agent <Key> <value>`, then
`launchctl kickstart -k gui/$(id -u)/me.centralhardware.mac-action-agent`. Env vars
`MAA_<KEY>` override them.

| key | default |
|---|---|
| `Port` | `8765` |
| `AllowedNetworks` | `10.168.0.0/24,127.0.0.0/8,::1` |

Log: `/Users/Shared/mac-action-agent.log`. For protected data (Full Disk Access,
Accessibility, Automation) grant it to `/usr/local/libexec/mac-action-agent`; releases are
signed with the shared LocalCodeSign certificate (secrets `MACOS_CERT_P12` /
`MACOS_CERT_PASSWORD`), so grants survive updates.

## Install

```sh
brew tap centralhardware/mac-action-agent https://github.com/centralhardware/macActionAgent
brew install --cask mac-action-agent
```

## Release

Push a tag `vX.Y.Z`: CI builds a universal signed binary → pkg → dmg, publishes the
release and opens a PR bumping `Casks/mac-action-agent.rb`.
