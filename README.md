# homeassistant-tui

A fast, keyboard-driven terminal UI for [Home Assistant](https://www.home-assistant.io/), written in Rust with [Ratatui](https://ratatui.rs).

It talks to Home Assistant over its WebSocket API, so states update live, and it never blocks on the network: failed calls surface as toasts and dropped connections reconnect automatically.

## Features

- **Entities**: browse by area or domain, with live state, a detail pane (attributes and available controls), and control of lights, switches, fans, climate, covers, media players, locks, vacuums, input_number/select, buttons, and more.
- **Scenes & Scripts**: run with Enter.
- **Automations**: enable or disable, trigger, see last triggered.
- **Logbook**: last 24h, newest first, optionally filtered to one entity.
- **History**: a 24h line chart for numeric sensors and a colored state timeline for everything else.
- **Fuzzy filter** (`/`) and **command palette** (`:` / `Ctrl-p`): jump to, toggle, or run anything by name.
- Confirmation before unlocking locks or moving garage doors and gates.

## Install

Prebuilt binaries for Linux, macOS and Windows are on the [releases page](https://github.com/agkrishnendu/ha-tui/releases/latest), or:

```sh
# Linux / macOS
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/agkrishnendu/ha-tui/releases/latest/download/homeassistant-tui-installer.sh | sh

# Windows (PowerShell)
powershell -ExecutionPolicy Bypass -c "irm https://github.com/agkrishnendu/ha-tui/releases/latest/download/homeassistant-tui-installer.ps1 | iex"

# With a Rust toolchain (1.88+)
cargo install homeassistant-tui     # or: cargo binstall homeassistant-tui
```

## Setup

Create a long-lived access token in Home Assistant (your profile → Security → Long-lived access tokens), save [`config.example.toml`](config.example.toml) as your config file and fill in the URL and token, then:

```sh
homeassistant-tui --dump     # connectivity check: prints entity counts and exits
homeassistant-tui
```

The config file lives at `~/.config/homeassistant-tui/config.toml` on Linux, `~/Library/Application Support/homeassistant-tui/config.toml` on macOS and `%APPDATA%\homeassistant-tui\config\config.toml` on Windows (or pass `--config <path>`). Instead of a config file you can use environment variables: `HA_URL=http://homeassistant.local:8123 HA_TOKEN=... homeassistant-tui`.

## Keys

| Key | Action |
| --- | --- |
| `1`–`4`, `Tab` | switch tab |
| `j`/`k`, arrows, `PgUp`/`PgDn`, `Home`/`End` | move |
| `h`/`l` | focus the group sidebar or the entity list |
| `g` | group by area ↔ domain |
| `a` | show / hide system & hidden entities |
| `/` | fuzzy filter (searches all entities); `Esc` clears |
| `:` or `Ctrl-p` | command palette |
| `Enter` / `Space` | toggle · run · press |
| `+` / `-` | brightness · target temp · volume · position · value |
| `[` / `]` | color temperature · fan speed |
| `m` | cycle HVAC mode · select option · media source |
| `o` / `c` / `s` | open · close · stop |
| `x` | trigger automation |
| `n` / `p` | next / previous track |
| `H` | 24h history |
| `f` (Logbook) | filter to the entity selected in Entities |
| `r` | resync (Logbook: refresh) |
| `?` | help |
| `q` | quit |

Hidden, config/diagnostic and housekeeping entities (backup, TTS, conversation, zones, update, event…) are left out of the lists by default; press `a` to show everything.

## Try it without Home Assistant

A mock server with sample devices ships as an example:

```sh
cargo run --example mock_ha                               # 127.0.0.1:8124, token "demo"
HA_URL=http://127.0.0.1:8124 HA_TOKEN=demo cargo run
```

## Development

```sh
cargo test                        # unit tests + WebSocket integration tests against the mock
cargo clippy --all-targets -- -D warnings
```

Layout:

- `src/ha/client.rs`: WebSocket actor (auth, bootstrap, subscription, requests, ping, reconnect)
- `src/ha/actions.rs`: pure mapping from (entity, key action) to service call
- `src/store.rs`: state mirror, area resolution, grouping
- `src/app.rs`: app state and key handling
- `src/ui/`: rendering
- `tests/support/mock.rs`: fake Home Assistant

## License

[0BSD](LICENSE): do whatever you like with it, no attribution required.
