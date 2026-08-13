# TermChat

TermChat is a lightweight, read-only Twitch and Kick chat viewer for the terminal. It connects to one or more channels without an account and displays native platform and 7TV emotes, including live 7TV emote-set changes.

## Install

Download a standalone archive from the GitHub Releases page, or install the Rust package:

```console
cargo install termchat-live
```

The package installs a command named `termchat`.

## Usage

```console
termchat
termchat twitch twitchdev
termchat twitch https://www.twitch.tv/twitchdev
termchat twitch twitchdev cmgriffing moonmoon
termchat twitch twitchdev --images off
termchat twitch twitchdev --badges off
termchat kick xqc
termchat kick https://kick.com/xqc
termchat kick xqc trainwreckstv --images off
termchat kick xqc --badges off
```

Running `termchat` opens the TUI with your saved channels. Open the command palette with `Super-K` (or `Ctrl-K` when the terminal does not report the Super key) to add a Twitch or Kick channel by name or URL; the new connection starts immediately. Command-line channels are optional startup additions and are saved too.

Each channel gets an independent platform-labelled tab, connection, history, scroll position, and 7TV emote set. Twitch and Kick channels with the same name can be open together. Channel changes, the image preference, and the badge preference are stored automatically in the operating system's configuration directory.

Each tab has a compact connection indicator: green means connected, yellow means connecting or reconnecting, and red means disconnected.

Moderation events remain visible in the feed. Deleted messages are retained, dimmed, and labelled instead of disappearing; timeouts, bans, unbans, and full-chat clears appear as moderation notices. A timeout, ban, or clear also dims matching messages already in the local history. Moderator names are shown when the platform includes them in the event.

Twitch and Kick identity badges are displayed as compact colored chips before the username. Common roles include broadcaster (`B`), moderator (`M`), VIP (`V`), subscriber (`S`), founder (`F`), staff/admin (`A`), verified (`✓`), bot, artist, OG, and Kick level badges; unknown platform badges receive an abbreviated fallback chip. Badges are enabled by default and can be hidden with `--badges off` or the command palette.

TermChat detects Kitty, iTerm2, and Sixel graphics support and falls back to a Unicode half-block renderer in other terminals. Windows Terminal 1.22 and newer are recognized explicitly so their Sixel support is used even when the terminal omits font-cell dimensions from its capability response. When an image cannot be loaded, the original emote name remains visible. Animated Kick and 7TV emotes render their first frame; zero-width overlays use their text names in version 1.

Messages containing recognized emotes are staged briefly while their images load, so they enter the feed once in their final rendered form instead of changing from text to images on screen. Message order is preserved, and a failed image request falls back to its emote name after a bounded timeout.

### Keys

| Key | Action |
| --- | --- |
| `Super-K` / `Ctrl-K` | Open or close the searchable command palette |
| Left / Right | Previous / next channel |
| Up / Down | Scroll one row |
| `Page Up` / `Page Down` | Scroll one page |
| `Home` / `End` | Oldest messages / resume following |
| `q` or `Ctrl-C` | Quit |

The command palette contains channel add/remove/switch commands, emote-image and identity-badge toggles, and quit. Type to filter it, use Up/Down to select a command, Enter to run it, and Esc to close it.

## Platform behavior

TermChat uses Twitch's legacy anonymous IRC compatibility mode (`justinfan`, without a password). Twitch still accepts this for read-only chat, but it is not part of Twitch's current documented authentication contract. If Twitch removes it, TermChat reports the rejection instead of requesting or storing credentials.

Kick's official receive-chat API delivers events to a publicly accessible webhook, which is not a natural fit for a standalone local terminal application. TermChat therefore resolves channels through Kick's internal website API and reads its public Pusher WebSocket without authentication. Both interfaces are undocumented and may change without notice; failures are reported in the affected tab and retried automatically. See [Kick's official Events API documentation](https://github.com/KickEngineering/KickDevDocs/blob/main/events/introduction.md) for the supported webhook alternative.

Kick channel resolution requires `curl` on `PATH`. It is included with Windows 10 and newer and is commonly preinstalled on macOS and Linux.

7TV is optional and non-blocking. Twitch and Kick chat continue when the 7TV API, EventAPI, CDN, or an individual image is unavailable. Emote images are cached in the operating system's cache directory with a 64 MiB limit.

## Development

Rust 1.88 or newer is required.

```console
cargo fmt --check
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
```

The public chat model, target model, and `PlatformAdapter` trait are platform-neutral so additional transports can be added without changing the feed renderer.

## License

MIT
