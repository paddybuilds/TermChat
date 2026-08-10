# TermChat

TermChat is a lightweight, read-only Twitch chat viewer for the terminal. It connects to one or more channels without a Twitch account and displays native Twitch and 7TV emotes, including live 7TV emote-set changes.

## Install

Download a standalone archive from the GitHub Releases page, or install the Rust package:

```console
cargo install termchat-live
```

The package installs a command named `termchat`.

## Usage

```console
termchat twitch twitchdev
termchat twitch https://www.twitch.tv/twitchdev
termchat twitch twitchdev cmgriffing moonmoon
termchat twitch twitchdev --images off
```

When several channels are supplied, each gets an independent tab, connection, history, scroll position, and 7TV emote set. Use the Left and Right arrow keys to move between tabs.

TermChat detects Kitty, iTerm2, and Sixel graphics support. When the terminal does not advertise a supported image protocol—or when an image cannot be loaded—the original emote name remains visible. Animated and zero-width 7TV emotes intentionally use their text names in version 1.

### Keys

| Key | Action |
| --- | --- |
| `←` / `→` | Previous / next channel |
| `↑` / `↓` | Scroll one row |
| `Page Up` / `Page Down` | Scroll one page |
| `Home` / `End` | Oldest messages / resume following |
| `q` or `Ctrl-C` | Quit |

## Platform behavior

TermChat uses Twitch's legacy anonymous IRC compatibility mode (`justinfan`, without a password). Twitch still accepts this for read-only chat, but it is not part of Twitch's current documented authentication contract. If Twitch removes it, TermChat reports the rejection instead of requesting or storing credentials.

7TV is optional and non-blocking. Twitch chat continues when the 7TV API, EventAPI, CDN, or an individual image is unavailable. Emote images are cached in the operating system's cache directory with a 64 MiB limit.

## Development

Rust 1.88 or newer is required.

```console
cargo fmt --check
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
```

The public chat model and `PlatformAdapter` trait are platform-neutral so YouTube and Kick transports can be added without changing the TUI.

## License

MIT
