use std::{
    collections::{HashMap, VecDeque},
    io::{self, Stdout},
    panic,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::{StreamExt, future::join_all};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use ratatui_image::{
    Image,
    picker::{Picker, ProtocolType},
};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::{
    cli::{BadgeMode, ImageMode},
    config::{SavedChannel, Settings},
    emotes::{SharedEmoteRegistry, SharedGlobalEmotes},
    image_store::{EMOTE_HEIGHT, EMOTE_WIDTH, ImageStore},
    model::{
        ChatBadge, ChatEvent, ChatFragment, ChatMessage, ConnectionState, EmoteRef,
        MessageModeration, ModerationEvent,
    },
    platform::{PlatformAdapter, kick::KickAdapter, twitch::TwitchAdapter},
    seventv::SevenTvClient,
    target::{ChatTarget, KickTarget, PlatformKind, TwitchTarget},
};

const MAX_MESSAGES: usize = 1_000;
type PanicHook = Box<dyn Fn(&panic::PanicHookInfo<'_>) + Send + Sync + 'static>;

pub async fn run(targets: Vec<ChatTarget>, mut settings: Settings) -> Result<()> {
    let global_emotes = SharedGlobalEmotes::default();
    let global_emote_error = SevenTvClient::new().load_global(&global_emotes).await.err();
    let mut terminal = TerminalSession::enter()?;
    let picker = select_picker();
    let mut images = ImageStore::new(Some(picker), settings.images == ImageMode::Auto);
    let mut event_stream = EventStream::new();
    let (event_tx, mut event_rx) = mpsc::channel::<(u64, ChatEvent)>(512);
    let mut runtimes = HashMap::new();
    let mut app = AppState::new_with_badges(
        settings.images == ImageMode::Auto,
        images.supported(),
        settings.badges == BadgeMode::On,
    );
    if let Some(error) = global_emote_error {
        app.notice =
            format!("7TV global emotes unavailable; channel connections will retry: {error:#}");
    }
    for target in targets {
        let id = app.add_channel(target.clone());
        runtimes.insert(
            id,
            spawn_channel(id, target, global_emotes.clone(), event_tx.clone()),
        );
    }
    sync_settings(&mut app, &mut settings).await;
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    let result = loop {
        terminal
            .terminal
            .draw(|frame| draw(frame, &mut app, &mut images))
            .context("draw terminal UI")?;

        tokio::select! {
            _ = ticker.tick() => {
                for error in images.drain_completed() {
                    app.set_notice(format!("An emote image could not be rendered: {error}"));
                }
                app.flush_ready_messages(&images);
            }
            event = event_rx.recv() => {
                if let Some((channel, event)) = event {
                    app.handle_chat_event(channel, event, &mut images);
                }
            }
            terminal_event = event_stream.next() => {
                match terminal_event {
                    Some(Ok(event)) => {
                        match app.handle_terminal_event(event) {
                            UiAction::None => {}
                            UiAction::Quit => break Ok(()),
                            UiAction::Add(target) => {
                                let id = app.add_channel(target.clone());
                                runtimes.insert(id, spawn_channel(id, target, global_emotes.clone(), event_tx.clone()));
                                sync_settings(&mut app, &mut settings).await;
                            }
                            UiAction::Remove(id) => {
                                if let Some(runtime) = runtimes.remove(&id) {
                                    runtime.stop().await;
                                }
                                app.remove_channel(id);
                                sync_settings(&mut app, &mut settings).await;
                            }
                            UiAction::ToggleImages => {
                                let enabled = !app.images_enabled;
                                images.set_enabled(enabled);
                                app.images_enabled = enabled;
                                if enabled && !images.supported() {
                                    app.set_notice("This terminal does not advertise Kitty, iTerm2, or Sixel graphics".to_owned());
                                } else if enabled {
                                    app.request_all_emotes(&mut images);
                                }
                                app.flush_ready_messages(&images);
                                sync_settings(&mut app, &mut settings).await;
                            }
                            UiAction::ToggleBadges => {
                                app.badges_enabled = !app.badges_enabled;
                                sync_settings(&mut app, &mut settings).await;
                            }
                            UiAction::SwitchChannel(id) => {
                                if let Some(index) = app.channels.iter().position(|channel| channel.id == id) {
                                    app.active = index;
                                }
                            }
                        }
                    }
                    Some(Err(error)) => break Err(error).context("read terminal input"),
                    None => break Ok(()),
                }
            }
        }
    };

    for (_, runtime) in runtimes {
        runtime.stop().await;
    }
    result
}

fn select_picker() -> Picker {
    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
    apply_terminal_graphics_hint(picker, std::env::var_os("WT_SESSION").is_some())
}

fn apply_terminal_graphics_hint(mut picker: Picker, windows_terminal: bool) -> Picker {
    // Windows Terminal 1.22+ supports Sixel, but does not always include a font
    // cell size in its capability response. ratatui-image treats the missing
    // size as a reason to return its half-block default, even when Sixel was
    // advertised. WT_SESSION is set by Windows Terminal itself, so it is a
    // sufficiently narrow hint and avoids emitting Sixel into legacy consoles.
    if windows_terminal && picker.protocol_type() == ProtocolType::Halfblocks {
        picker.set_protocol_type(ProtocolType::Sixel);
    }
    picker
}

struct ChannelRuntime {
    cancellation: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
}

impl ChannelRuntime {
    async fn stop(self) {
        self.stop_with_timeout(Duration::from_secs(2)).await;
    }

    async fn stop_with_timeout(self, timeout: Duration) {
        self.cancellation.cancel();
        let mut tasks = self.tasks;
        if tokio::time::timeout(timeout, join_all(tasks.iter_mut()))
            .await
            .is_err()
        {
            for task in &tasks {
                task.abort();
            }
            join_all(tasks).await;
        }
    }
}

fn spawn_channel(
    id: u64,
    target: ChatTarget,
    global_emotes: SharedGlobalEmotes,
    tagged_events: mpsc::Sender<(u64, ChatEvent)>,
) -> ChannelRuntime {
    let cancellation = CancellationToken::new();
    let adapter_cancellation = cancellation.clone();
    let emotes = SharedEmoteRegistry::with_global(global_emotes);
    let adapter: Arc<dyn PlatformAdapter> = match target {
        ChatTarget::Twitch(target) => Arc::new(TwitchAdapter::new(target, emotes)),
        ChatTarget::Kick(target) => Arc::new(KickAdapter::new(target, emotes)),
    };
    let (adapter_events, mut adapter_rx) = mpsc::channel(256);
    let adapter_sender = adapter_events.clone();
    let adapter_task = tokio::spawn(async move {
        if let Err(error) = adapter
            .run(adapter_sender.clone(), adapter_cancellation)
            .await
        {
            let _ = adapter_sender
                .send(ChatEvent::Disconnected {
                    reason: format!("{error:#}"),
                })
                .await;
        }
    });
    let bridge_task = tokio::spawn(async move {
        while let Some(event) = adapter_rx.recv().await {
            if tagged_events.send((id, event)).await.is_err() {
                return;
            }
        }
    });
    ChannelRuntime {
        cancellation,
        tasks: vec![adapter_task, bridge_task],
    }
}

async fn sync_settings(app: &mut AppState, settings: &mut Settings) {
    settings.channels = app
        .channels
        .iter()
        .map(|channel| SavedChannel {
            platform: channel.target.platform(),
            channel: channel.target.channel().to_owned(),
        })
        .collect();
    settings.images = if app.images_enabled {
        ImageMode::Auto
    } else {
        ImageMode::Off
    };
    settings.badges = if app.badges_enabled {
        BadgeMode::On
    } else {
        BadgeMode::Off
    };
    if let Err(error) = settings.save().await {
        app.set_notice(format!("Could not save settings: {error:#}"));
    }
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    previous_panic_hook: Option<PanicHook>,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("enable terminal raw mode")?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error).context("enter alternate screen");
        }
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend).context("initialize terminal")?;
        let previous = panic::take_hook();
        panic::set_hook(Box::new(|info| {
            restore_terminal();
            eprintln!("{info}");
        }));
        Ok(Self {
            terminal,
            previous_panic_hook: Some(previous),
        })
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        restore_terminal();
        if let Some(previous) = self.previous_panic_hook.take() {
            panic::set_hook(previous);
        }
    }
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
}

struct AppState {
    channels: Vec<ChannelState>,
    active: usize,
    images_enabled: bool,
    images_supported: bool,
    badges_enabled: bool,
    next_channel_id: u64,
    overlay: Overlay,
    notice: String,
}

struct ChannelState {
    id: u64,
    target: ChatTarget,
    messages: VecDeque<FeedEntry>,
    pending_messages: VecDeque<FeedEntry>,
    connection: ConnectionState,
    status: String,
    follow: bool,
    viewport_start: usize,
    last_total_rows: usize,
    last_viewport_rows: usize,
}

#[derive(Clone, Debug)]
enum FeedEntry {
    Message(ChatMessage),
    Moderation(ModerationEvent),
}

impl FeedEntry {
    fn is_ready(&self, images: &ImageStore) -> bool {
        match self {
            Self::Moderation(_) => true,
            Self::Message(message) => message.fragments.iter().all(|fragment| match fragment {
                ChatFragment::Text(_) => true,
                ChatFragment::Emote(emote) => images.is_settled(&emote.cache_key()),
            }),
        }
    }
}

#[derive(Debug, Default, Eq, PartialEq)]
enum Overlay {
    #[default]
    None,
    CommandPalette {
        query: String,
        selected: usize,
    },
    AddChannel {
        platform: PlatformKind,
        input: String,
        error: String,
    },
    ConfirmRemove,
}

#[derive(Debug)]
enum UiAction {
    None,
    Quit,
    Add(ChatTarget),
    Remove(u64),
    ToggleImages,
    ToggleBadges,
    SwitchChannel(u64),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PaletteAction {
    AddChannel(PlatformKind),
    RemoveChannel(u64),
    ToggleImages,
    ToggleBadges,
    SwitchChannel(u64),
    Quit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PaletteItem {
    label: String,
    hint: String,
    action: PaletteAction,
}

impl AppState {
    #[cfg(test)]
    fn new(images_enabled: bool, images_supported: bool) -> Self {
        Self::new_with_badges(images_enabled, images_supported, true)
    }

    fn new_with_badges(images_enabled: bool, images_supported: bool, badges_enabled: bool) -> Self {
        Self {
            channels: Vec::new(),
            active: 0,
            images_enabled,
            images_supported,
            badges_enabled,
            next_channel_id: 1,
            overlay: Overlay::None,
            notice: "Open the command palette to add a Twitch or Kick channel".to_owned(),
        }
    }

    fn add_channel(&mut self, target: ChatTarget) -> u64 {
        let id = self.next_channel_id;
        self.next_channel_id = self.next_channel_id.wrapping_add(1).max(1);
        self.channels.push(ChannelState::new(id, target));
        self.active = self.channels.len() - 1;
        self.overlay = Overlay::None;
        id
    }

    fn remove_channel(&mut self, id: u64) {
        if let Some(index) = self.channels.iter().position(|channel| channel.id == id) {
            self.channels.remove(index);
            if self.channels.is_empty() {
                self.active = 0;
                self.notice = "No channels open. Use the command palette to add one.".to_owned();
            } else {
                self.active = self.active.min(self.channels.len() - 1);
            }
        }
        self.overlay = Overlay::None;
    }

    fn set_notice(&mut self, notice: String) {
        if let Some(channel) = self.channels.get_mut(self.active) {
            channel.status = notice;
        } else {
            self.notice = notice;
        }
    }

    fn request_all_emotes(&self, images: &mut ImageStore) {
        for channel in &self.channels {
            for entry in channel.messages.iter().chain(&channel.pending_messages) {
                let FeedEntry::Message(message) = entry else {
                    continue;
                };
                for fragment in &message.fragments {
                    if let ChatFragment::Emote(emote) = fragment {
                        images.request(emote);
                    }
                }
            }
        }
    }

    fn flush_ready_messages(&mut self, images: &ImageStore) {
        for channel in &mut self.channels {
            channel.flush_ready(images);
        }
    }

    fn handle_chat_event(&mut self, channel: u64, event: ChatEvent, images: &mut ImageStore) {
        let Some(state) = self.channels.iter_mut().find(|state| state.id == channel) else {
            return;
        };
        state.handle_chat_event(event, images);
    }

    fn handle_terminal_event(&mut self, event: Event) -> UiAction {
        let Event::Key(key) = event else {
            return UiAction::None;
        };
        if key.kind == crossterm::event::KeyEventKind::Release {
            return UiAction::None;
        }
        if is_control_key(key, 'c') {
            return UiAction::Quit;
        }
        if is_command_palette_key(key) {
            self.overlay = match self.overlay {
                Overlay::CommandPalette { .. } => Overlay::None,
                _ => Overlay::CommandPalette {
                    query: String::new(),
                    selected: 0,
                },
            };
            return UiAction::None;
        }
        if let Overlay::CommandPalette { query, selected } = &self.overlay {
            let mut query = query.clone();
            let mut selected = *selected;
            match key.code {
                KeyCode::Esc => {
                    self.overlay = Overlay::None;
                    return UiAction::None;
                }
                KeyCode::Backspace => {
                    query.pop();
                    selected = 0;
                }
                KeyCode::Char(character)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER) =>
                {
                    query.push(character);
                    selected = 0;
                }
                KeyCode::Up => selected = selected.saturating_sub(1),
                KeyCode::Down => selected = selected.saturating_add(1),
                KeyCode::Enter => {
                    let items = self.palette_items(&query);
                    if let Some(item) = items.get(selected.min(items.len().saturating_sub(1))) {
                        return self.execute_palette_action(item.action.clone());
                    }
                }
                _ => {}
            }
            let item_count = self.palette_items(&query).len();
            self.overlay = Overlay::CommandPalette {
                query,
                selected: selected.min(item_count.saturating_sub(1)),
            };
            return UiAction::None;
        }

        match &mut self.overlay {
            Overlay::CommandPalette { .. } => unreachable!("palette handled above"),
            Overlay::AddChannel {
                platform,
                input,
                error,
            } => match key.code {
                KeyCode::Esc => self.overlay = Overlay::None,
                KeyCode::Backspace => {
                    input.pop();
                    error.clear();
                }
                KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    input.push(character);
                    error.clear();
                }
                KeyCode::Enter => match parse_target(*platform, input) {
                    Ok(target) if self.channels.iter().any(|channel| channel.target == target) => {
                        *error = "That channel is already open".to_owned();
                    }
                    Ok(target) => return UiAction::Add(target),
                    Err(parse_error) => *error = parse_error.to_string(),
                },
                _ => {}
            },
            Overlay::ConfirmRemove => match key.code {
                KeyCode::Char('y' | 'Y') => {
                    if let Some(channel) = self.channels.get(self.active) {
                        return UiAction::Remove(channel.id);
                    }
                    self.overlay = Overlay::None;
                }
                KeyCode::Esc | KeyCode::Char('n' | 'N') => self.overlay = Overlay::None,
                _ => {}
            },
            Overlay::None => {
                if key.code == KeyCode::Char('q') {
                    return UiAction::Quit;
                }
                match key.code {
                    KeyCode::Left if self.channels.len() > 1 => {
                        self.active = self
                            .active
                            .checked_sub(1)
                            .unwrap_or(self.channels.len() - 1);
                    }
                    KeyCode::Right if self.channels.len() > 1 => {
                        self.active = (self.active + 1) % self.channels.len();
                    }
                    _ if !self.channels.is_empty() => {
                        self.channels[self.active].handle_scroll_key(key.code)
                    }
                    _ => {}
                }
            }
        }
        UiAction::None
    }

    fn palette_items(&self, query: &str) -> Vec<PaletteItem> {
        let mut items = vec![
            PaletteItem {
                label: "Add Twitch channel".to_owned(),
                hint: "Enter a name or URL".to_owned(),
                action: PaletteAction::AddChannel(PlatformKind::Twitch),
            },
            PaletteItem {
                label: "Add Kick channel".to_owned(),
                hint: "Enter a name or URL".to_owned(),
                action: PaletteAction::AddChannel(PlatformKind::Kick),
            },
        ];
        if let Some(channel) = self.channels.get(self.active) {
            items.push(PaletteItem {
                label: format!("Remove {}", channel.target.display_label()),
                hint: "Close the current tab".to_owned(),
                action: PaletteAction::RemoveChannel(channel.id),
            });
        }
        items.push(PaletteItem {
            label: if self.images_enabled {
                "Disable emote images".to_owned()
            } else {
                "Enable emote images".to_owned()
            },
            hint: if self.images_supported {
                "Saved automatically".to_owned()
            } else {
                "Unsupported by this terminal".to_owned()
            },
            action: PaletteAction::ToggleImages,
        });
        items.push(PaletteItem {
            label: if self.badges_enabled {
                "Disable identity badges".to_owned()
            } else {
                "Enable identity badges".to_owned()
            },
            hint: "Saved automatically".to_owned(),
            action: PaletteAction::ToggleBadges,
        });
        for channel in &self.channels {
            items.push(PaletteItem {
                label: format!("Switch to {}", channel.target.display_label()),
                hint: if self
                    .channels
                    .get(self.active)
                    .is_some_and(|active| active.id == channel.id)
                {
                    "Current channel".to_owned()
                } else {
                    "Open channel tab".to_owned()
                },
                action: PaletteAction::SwitchChannel(channel.id),
            });
        }
        items.push(PaletteItem {
            label: "Quit TermChat".to_owned(),
            hint: "Close all connections".to_owned(),
            action: PaletteAction::Quit,
        });

        let words: Vec<String> = query
            .split_whitespace()
            .map(|word| word.to_ascii_lowercase())
            .collect();
        items
            .into_iter()
            .filter(|item| {
                let searchable = format!("{} {}", item.label, item.hint).to_ascii_lowercase();
                words.iter().all(|word| searchable.contains(word))
            })
            .collect()
    }

    fn execute_palette_action(&mut self, action: PaletteAction) -> UiAction {
        match action {
            PaletteAction::AddChannel(platform) => {
                self.overlay = Overlay::AddChannel {
                    platform,
                    input: String::new(),
                    error: String::new(),
                };
                UiAction::None
            }
            PaletteAction::RemoveChannel(_) => {
                self.overlay = Overlay::ConfirmRemove;
                UiAction::None
            }
            PaletteAction::ToggleImages => {
                self.overlay = Overlay::None;
                UiAction::ToggleImages
            }
            PaletteAction::ToggleBadges => {
                self.overlay = Overlay::None;
                UiAction::ToggleBadges
            }
            PaletteAction::SwitchChannel(id) => {
                self.overlay = Overlay::None;
                UiAction::SwitchChannel(id)
            }
            PaletteAction::Quit => UiAction::Quit,
        }
    }
}

impl ChannelState {
    fn new(id: u64, target: ChatTarget) -> Self {
        let platform = target.platform().display_name();
        Self {
            id,
            target,
            messages: VecDeque::new(),
            pending_messages: VecDeque::new(),
            connection: ConnectionState::Connecting,
            status: format!("Starting {platform} chat..."),
            follow: true,
            viewport_start: 0,
            last_total_rows: 0,
            last_viewport_rows: 1,
        }
    }

    fn handle_chat_event(&mut self, event: ChatEvent, images: &mut ImageStore) {
        match event {
            ChatEvent::Connected => {
                self.connection = ConnectionState::Connected;
                self.status = "Connected anonymously".to_owned();
            }
            ChatEvent::Disconnected { reason } => {
                self.connection = ConnectionState::Reconnecting;
                self.status = reason;
            }
            ChatEvent::Status(status) => self.status = status,
            ChatEvent::Message(message) => {
                for fragment in &message.fragments {
                    if let ChatFragment::Emote(emote) = fragment {
                        images.request(emote);
                    }
                }
                self.pending_messages.push_back(FeedEntry::Message(message));
                self.flush_ready(images);
            }
            ChatEvent::Moderation(event) => self.apply_moderation(event, images),
        }
    }

    fn apply_moderation(&mut self, event: ModerationEvent, images: &ImageStore) {
        match &event {
            ModerationEvent::MessageDeleted { message_id, .. } => {
                if let Some(message) = self
                    .messages
                    .iter_mut()
                    .chain(&mut self.pending_messages)
                    .find_map(|entry| match entry {
                        FeedEntry::Message(message) if message.id == *message_id => Some(message),
                        _ => None,
                    })
                {
                    message.moderation = Some(MessageModeration::Deleted);
                }
            }
            ModerationEvent::UserTimedOut {
                user_id,
                user,
                duration_seconds,
                ..
            } => self.mark_user_messages(
                user_id.as_deref(),
                user,
                MessageModeration::TimedOut {
                    duration_seconds: *duration_seconds,
                },
            ),
            ModerationEvent::UserBanned { user_id, user, .. } => {
                self.mark_user_messages(user_id.as_deref(), user, MessageModeration::Banned);
            }
            ModerationEvent::UserUnbanned { .. } => {}
            ModerationEvent::ChatCleared { .. } => {
                for entry in self.messages.iter_mut().chain(&mut self.pending_messages) {
                    if let FeedEntry::Message(message) = entry
                        && message.moderation.is_none()
                    {
                        message.moderation = Some(MessageModeration::ChatCleared);
                    }
                }
            }
        }
        self.pending_messages
            .push_back(FeedEntry::Moderation(event));
        self.flush_ready(images);
    }

    fn mark_user_messages(
        &mut self,
        user_id: Option<&str>,
        user: &str,
        moderation: MessageModeration,
    ) {
        for entry in self.messages.iter_mut().chain(&mut self.pending_messages) {
            let FeedEntry::Message(message) = entry else {
                continue;
            };
            let id_matches = user_id.is_some_and(|id| message.sender_id.as_deref() == Some(id));
            if (id_matches || message.sender.eq_ignore_ascii_case(user))
                && message.moderation.is_none()
            {
                message.moderation = Some(moderation.clone());
            }
        }
    }

    fn push_entry(&mut self, entry: FeedEntry) {
        self.messages.push_back(entry);
        if self.messages.len() > MAX_MESSAGES {
            self.messages.pop_front();
            if !self.follow {
                self.viewport_start = self.viewport_start.saturating_sub(1);
            }
        }
    }

    fn flush_ready(&mut self, images: &ImageStore) {
        while self
            .pending_messages
            .front()
            .is_some_and(|entry| entry.is_ready(images))
        {
            let entry = self
                .pending_messages
                .pop_front()
                .expect("front checked above");
            self.push_entry(entry);
        }
    }

    fn handle_scroll_key(&mut self, key: KeyCode) {
        let page = self.last_viewport_rows.max(1);
        let max_start = self.last_total_rows.saturating_sub(1);
        match key {
            KeyCode::Up => self.scroll_up(1),
            KeyCode::PageUp => self.scroll_up(page),
            KeyCode::Home => {
                self.follow = false;
                self.viewport_start = 0;
            }
            KeyCode::Down => self.scroll_down(1, max_start),
            KeyCode::PageDown => self.scroll_down(page, max_start),
            KeyCode::End => self.follow = true,
            _ => {}
        }
    }

    fn scroll_up(&mut self, amount: usize) {
        if self.follow {
            self.viewport_start = self
                .last_total_rows
                .saturating_sub(self.last_viewport_rows)
                .saturating_sub(amount);
            self.follow = false;
        } else {
            self.viewport_start = self.viewport_start.saturating_sub(amount);
        }
    }

    fn scroll_down(&mut self, amount: usize, max_start: usize) {
        if self.follow {
            return;
        }
        self.viewport_start = (self.viewport_start + amount).min(max_start);
        if self.viewport_start + self.last_viewport_rows >= self.last_total_rows {
            self.follow = true;
        }
    }
}

fn parse_target(
    platform: PlatformKind,
    input: &str,
) -> Result<ChatTarget, crate::target::TargetError> {
    match platform {
        PlatformKind::Twitch => TwitchTarget::parse(input).map(ChatTarget::Twitch),
        PlatformKind::Kick => KickTarget::parse(input).map(ChatTarget::Kick),
    }
}

fn is_control_key(key: KeyEvent, character: char) -> bool {
    key.code == KeyCode::Char(character) && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn is_command_palette_key(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('k')
        && key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
}

#[derive(Clone)]
enum RowItem {
    Text { x: u16, value: String, style: Style },
    Image { x: u16, key: String },
}

#[derive(Clone, Default)]
struct FeedRow {
    items: Vec<RowItem>,
    height: u16,
}

impl FeedRow {
    fn new() -> Self {
        Self {
            items: Vec::new(),
            height: 1,
        }
    }
}

fn draw(frame: &mut Frame<'_>, app: &mut AppState, images: &mut ImageStore) {
    let [header, feed] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(3)]).areas(frame.area());

    let mut tabs = Vec::new();
    let active_index = app.active;
    for (index, channel) in app.channels.iter().enumerate() {
        let tab_style = if index == active_index {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let state_color = match channel.connection {
            ConnectionState::Connected => Color::Green,
            ConnectionState::Connecting | ConnectionState::Reconnecting => Color::Yellow,
            ConnectionState::Disconnected => Color::Red,
        };
        tabs.push(Span::styled(" ", tab_style));
        tabs.push(Span::styled(
            "●",
            Style::default()
                .fg(state_color)
                .bg(tab_style.bg.unwrap_or(Color::Reset)),
        ));
        tabs.push(Span::styled(
            format!(" {} ", channel.target.display_label()),
            tab_style,
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(tabs)), header);

    if let Some(active) = app.channels.get_mut(app.active) {
        if feed.width > 0 && feed.height > 0 {
            let rows = layout_messages_with_badges(
                &active.messages,
                feed.width,
                images,
                app.badges_enabled,
            );
            render_rows(frame, feed, &rows, active, images);
        }
    } else {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "No Twitch or Kick channels are open.",
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from("Press Super+K or Ctrl+K to open the command palette."),
                Line::from("Your channels and image preference are saved automatically."),
            ]),
            feed,
        );
    }

    draw_overlay(frame, app);
}

fn draw_overlay(frame: &mut Frame<'_>, app: &AppState) {
    match &app.overlay {
        Overlay::None => {}
        Overlay::CommandPalette { query, selected } => {
            let items = app.palette_items(query);
            let visible = items.len().min(8) as u16;
            let area = centered_rect(frame.area(), 68, 6 + visible);
            frame.render_widget(Clear, area);
            let block = Block::default()
                .borders(Borders::ALL)
                .title(" Command palette ");
            let inner = block.inner(area);
            frame.render_widget(block, area);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("> ", Style::default().fg(Color::Cyan)),
                    Span::styled(query.clone(), Style::default().add_modifier(Modifier::BOLD)),
                ])),
                Rect::new(inner.x, inner.y, inner.width, 1),
            );
            frame.render_widget(
                Paragraph::new(Span::styled(
                    app.channels
                        .get(app.active)
                        .map(|channel| {
                            format!("{} · {}", channel.target.display_label(), channel.status)
                        })
                        .unwrap_or_else(|| "No channel selected".to_owned()),
                    Style::default().fg(Color::DarkGray),
                )),
                Rect::new(inner.x, inner.y.saturating_add(1), inner.width, 1),
            );
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "Type to filter | Up/Down select | Enter run | Esc close",
                    Style::default().fg(Color::DarkGray),
                )),
                Rect::new(inner.x, inner.y.saturating_add(2), inner.width, 1),
            );
            if items.is_empty() {
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        "No matching commands",
                        Style::default().fg(Color::DarkGray),
                    )),
                    Rect::new(inner.x, inner.y.saturating_add(4), inner.width, 1),
                );
            } else {
                let selected = (*selected).min(items.len() - 1);
                let start = selected.saturating_sub(7);
                for (row, (index, item)) in items.iter().enumerate().skip(start).take(8).enumerate()
                {
                    let style = if index == selected {
                        Style::default().fg(Color::Black).bg(Color::Cyan)
                    } else {
                        Style::default()
                    };
                    frame.render_widget(
                        Paragraph::new(Line::from(vec![
                            Span::styled(format!(" {}", item.label), style),
                            Span::styled(
                                format!("  {}", item.hint),
                                style.fg(if index == selected {
                                    Color::Black
                                } else {
                                    Color::DarkGray
                                }),
                            ),
                        ]))
                        .style(style),
                        Rect::new(
                            inner.x,
                            inner.y.saturating_add(4 + row as u16),
                            inner.width,
                            1,
                        ),
                    );
                }
            }
            let cursor_x = inner
                .x
                .saturating_add(2)
                .saturating_add(query.width().min(u16::MAX as usize) as u16)
                .min(inner.right().saturating_sub(1));
            frame.set_cursor_position((cursor_x, inner.y));
        }
        Overlay::AddChannel {
            platform,
            input,
            error,
        } => {
            let area = centered_rect(frame.area(), 62, 7);
            frame.render_widget(Clear, area);
            let block = Block::default()
                .borders(Borders::ALL)
                .title(format!(" Add {} channel ", platform.display_name()));
            let inner = block.inner(area);
            frame.render_widget(block, area);
            let error_line = if error.is_empty() {
                Line::from(Span::styled(
                    "Enter add | Esc cancel",
                    Style::default().fg(Color::DarkGray),
                ))
            } else {
                Line::from(Span::styled(error.clone(), Style::default().fg(Color::Red)))
            };
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(format!(
                        "Channel name or {} URL:",
                        match platform {
                            PlatformKind::Twitch => "twitch.tv",
                            PlatformKind::Kick => "kick.com",
                        }
                    )),
                    Line::from(Span::styled(
                        format!("> {input}"),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )),
                    Line::from(""),
                    error_line,
                ]),
                inner,
            );
            let cursor_x = inner
                .x
                .saturating_add(2)
                .saturating_add(input.width().min(u16::MAX as usize) as u16)
                .min(inner.right().saturating_sub(1));
            frame.set_cursor_position((cursor_x, inner.y.saturating_add(1)));
        }
        Overlay::ConfirmRemove => {
            let Some(channel) = app.channels.get(app.active) else {
                return;
            };
            let area = centered_rect(frame.area(), 52, 5);
            frame.render_widget(Clear, area);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(format!("Remove {}?", channel.target.display_label())),
                    Line::from(""),
                    Line::from(Span::styled(
                        "y remove | n/Esc cancel",
                        Style::default().fg(Color::Yellow),
                    )),
                ])
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Remove channel "),
                ),
                area,
            );
        }
    }
}

fn centered_rect(area: Rect, width_percent: u16, height: u16) -> Rect {
    let width = area
        .width
        .saturating_mul(width_percent)
        .saturating_div(100)
        .max(20);
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

#[cfg(test)]
fn layout_messages(
    messages: &VecDeque<FeedEntry>,
    width: u16,
    images: &ImageStore,
) -> Vec<FeedRow> {
    layout_messages_with_badges(messages, width, images, true)
}

fn layout_messages_with_badges(
    messages: &VecDeque<FeedEntry>,
    width: u16,
    images: &ImageStore,
    badges_enabled: bool,
) -> Vec<FeedRow> {
    let mut rows = Vec::new();
    for entry in messages {
        let FeedEntry::Message(message) = entry else {
            let FeedEntry::Moderation(event) = entry else {
                unreachable!();
            };
            let mut row = FeedRow::new();
            let mut x = 0;
            push_text(
                &mut rows,
                &mut row,
                &mut x,
                width,
                &format!("• Mod: {}", event.summary()),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::DIM | Modifier::ITALIC),
            );
            rows.push(row);
            continue;
        };
        let message_start = rows.len();
        let mut row = FeedRow::new();
        let mut x = 0;
        let moderated = message.moderation.is_some();
        let dimmed = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM);
        let color = message
            .color
            .map(|color| Color::Rgb(color.red, color.green, color.blue))
            .unwrap_or(Color::Cyan);
        for badge in message.badges.iter().filter(|_| badges_enabled) {
            let (label, badge_color) = badge_chip(badge);
            push_text(
                &mut rows,
                &mut row,
                &mut x,
                width,
                &format!("[{label}] "),
                if moderated {
                    dimmed
                } else {
                    Style::default()
                        .fg(badge_color)
                        .add_modifier(Modifier::BOLD)
                },
            );
        }
        push_text(
            &mut rows,
            &mut row,
            &mut x,
            width,
            &format!("{}: ", message.sender),
            if moderated {
                dimmed
            } else {
                Style::default().fg(color).add_modifier(Modifier::BOLD)
            },
        );
        for fragment in &message.fragments {
            match fragment {
                ChatFragment::Text(text) => push_text(
                    &mut rows,
                    &mut row,
                    &mut x,
                    width,
                    text,
                    if moderated { dimmed } else { Style::default() },
                ),
                ChatFragment::Emote(emote) if !moderated && image_is_ready(emote, images) => {
                    if x > 0 && x.saturating_add(EMOTE_WIDTH) > width {
                        rows.push(row);
                        row = FeedRow::new();
                        x = 0;
                    }
                    row.height = EMOTE_HEIGHT.min(2);
                    row.items.push(RowItem::Image {
                        x,
                        key: emote.cache_key(),
                    });
                    x = x.saturating_add(EMOTE_WIDTH).min(width);
                }
                ChatFragment::Emote(emote) => push_text(
                    &mut rows,
                    &mut row,
                    &mut x,
                    width,
                    &emote.name,
                    if moderated {
                        dimmed
                    } else {
                        Style::default().fg(Color::LightMagenta)
                    },
                ),
            }
        }
        if let Some(moderation) = &message.moderation {
            push_text(
                &mut rows,
                &mut row,
                &mut x,
                width,
                &format!("  [{}]", moderation.label()),
                dimmed.add_modifier(Modifier::ITALIC),
            );
        }
        rows.push(row);
        if images.enabled() && !moderated {
            ensure_message_height(&mut rows, message_start, EMOTE_HEIGHT);
        }
    }
    rows
}

fn badge_chip(badge: &ChatBadge) -> (String, Color) {
    let name = badge.name.to_ascii_lowercase().replace(['-', '_'], " ");
    match name.as_str() {
        "broadcaster" | "host" | "owner" => ("B".to_owned(), Color::Red),
        "moderator" | "mod" => ("M".to_owned(), Color::Green),
        "vip" => ("V".to_owned(), Color::Magenta),
        "subscriber" | "sub" => ("S".to_owned(), Color::Blue),
        "founder" => ("F".to_owned(), Color::LightBlue),
        "staff" | "admin" | "global mod" => ("A".to_owned(), Color::Red),
        "verified" | "partner" => ("✓".to_owned(), Color::LightBlue),
        "bot" => ("BOT".to_owned(), Color::DarkGray),
        "artist" => ("ART".to_owned(), Color::LightMagenta),
        "og" => ("OG".to_owned(), Color::Yellow),
        "level" => (
            badge
                .version
                .as_deref()
                .map(|level| format!("L{level}"))
                .unwrap_or_else(|| "L".to_owned()),
            Color::Yellow,
        ),
        _ => (
            name.split_whitespace()
                .filter_map(|part| part.chars().next())
                .take(3)
                .collect::<String>()
                .to_ascii_uppercase(),
            Color::DarkGray,
        ),
    }
}

fn ensure_message_height(rows: &mut [FeedRow], message_start: usize, minimum: u16) {
    let current: u16 = rows[message_start..].iter().map(|row| row.height).sum();
    if current < minimum
        && let Some(last) = rows.last_mut()
    {
        last.height = last.height.saturating_add(minimum - current);
    }
}

fn image_is_ready(emote: &EmoteRef, images: &ImageStore) -> bool {
    images.contains(&emote.cache_key())
}

fn push_text(
    rows: &mut Vec<FeedRow>,
    row: &mut FeedRow,
    x: &mut u16,
    width: u16,
    text: &str,
    style: Style,
) {
    for grapheme in text.graphemes(true) {
        if grapheme == "\n" {
            rows.push(std::mem::replace(row, FeedRow::new()));
            *x = 0;
            continue;
        }
        let grapheme_width = grapheme.width().min(u16::MAX as usize) as u16;
        if grapheme_width > 0 && *x > 0 && x.saturating_add(grapheme_width) > width {
            rows.push(std::mem::replace(row, FeedRow::new()));
            *x = 0;
        }
        append_text_item(row, *x, grapheme, style);
        *x = x.saturating_add(grapheme_width).min(width);
    }
}

fn append_text_item(row: &mut FeedRow, x: u16, grapheme: &str, style: Style) {
    if let Some(RowItem::Text {
        x: previous_x,
        value,
        style: previous_style,
    }) = row.items.last_mut()
        && *previous_style == style
        && previous_x.saturating_add(value.width() as u16) == x
    {
        value.push_str(grapheme);
        return;
    }
    row.items.push(RowItem::Text {
        x,
        value: grapheme.to_owned(),
        style,
    });
}

fn render_rows(
    frame: &mut Frame<'_>,
    area: Rect,
    rows: &[FeedRow],
    channel: &mut ChannelState,
    images: &mut ImageStore,
) {
    channel.last_total_rows = rows.len();
    let start = if channel.follow {
        bottom_start(rows, area.height)
    } else {
        channel.viewport_start.min(rows.len().saturating_sub(1))
    };
    if channel.follow {
        channel.viewport_start = start;
    }

    let mut y = area.y;
    let mut rendered_rows = 0;
    for row in rows.iter().skip(start) {
        if y >= area.bottom() || row.height > area.bottom().saturating_sub(y) {
            break;
        }
        for item in &row.items {
            match item {
                RowItem::Text { x, value, style } => frame.render_widget(
                    Paragraph::new(Span::styled(value.clone(), *style)),
                    Rect::new(
                        area.x.saturating_add(*x),
                        y,
                        area.width.saturating_sub(*x),
                        1,
                    ),
                ),
                RowItem::Image { x, key } => {
                    if let Some(protocol) = images.protocol(key) {
                        frame.render_widget(
                            Image::new(protocol).allow_clipping(true),
                            Rect::new(area.x.saturating_add(*x), y, EMOTE_WIDTH, EMOTE_HEIGHT),
                        );
                    }
                }
            }
        }
        y = y.saturating_add(row.height);
        rendered_rows += 1;
    }
    channel.last_viewport_rows = rendered_rows.max(1);
}

fn bottom_start(rows: &[FeedRow], available_height: u16) -> usize {
    let mut height = 0_u16;
    for (index, row) in rows.iter().enumerate().rev() {
        if height.saturating_add(row.height) > available_height {
            return index + 1;
        }
        height = height.saturating_add(row.height);
    }
    0
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::model::{ChatFragment, EmoteProvider, RgbColor};
    use ratatui::backend::TestBackend;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn command_key(modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char('k'), modifiers))
    }

    fn open_palette(app: &mut AppState) {
        assert!(matches!(
            app.handle_terminal_event(command_key(KeyModifiers::CONTROL)),
            UiAction::None
        ));
        assert!(matches!(app.overlay, Overlay::CommandPalette { .. }));
    }

    #[test]
    fn windows_terminal_hint_uses_sixel_instead_of_halfblocks() {
        let picker = apply_terminal_graphics_hint(Picker::halfblocks(), true);
        assert_eq!(picker.protocol_type(), ProtocolType::Sixel);
    }

    #[test]
    fn halfblocks_remain_the_fallback_without_a_terminal_hint() {
        let picker = apply_terminal_graphics_hint(Picker::halfblocks(), false);
        assert_eq!(picker.protocol_type(), ProtocolType::Halfblocks);
    }

    #[tokio::test]
    async fn channel_shutdown_aborts_a_task_that_ignores_cancellation() {
        struct DropFlag(Arc<AtomicBool>);

        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let task_dropped = dropped.clone();
        let task = tokio::spawn(async move {
            let _flag = DropFlag(task_dropped);
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        let runtime = ChannelRuntime {
            cancellation: CancellationToken::new(),
            tasks: vec![task],
        };

        runtime.stop_with_timeout(Duration::from_millis(1)).await;

        assert!(dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn text_layout_wraps_and_keeps_sender_style() {
        let message = ChatMessage {
            id: "1".to_owned(),
            sender_id: Some("alice-id".to_owned()),
            sender: "alice".to_owned(),
            color: Some(RgbColor {
                red: 1,
                green: 2,
                blue: 3,
            }),
            badges: vec![],
            fragments: vec![ChatFragment::Text("hello world".to_owned())],
            moderation: None,
        };
        let rows = layout_messages(
            &VecDeque::from([FeedEntry::Message(message)]),
            10,
            &ImageStore::new(None, false),
        );
        assert!(rows.len() >= 2);
        assert!(matches!(rows[0].items[0], RowItem::Text { .. }));
    }

    #[test]
    fn text_layout_measures_joined_emoji_as_graphemes() {
        let mut rows = Vec::new();
        let mut row = FeedRow::new();
        let mut x = 0;

        push_text(&mut rows, &mut row, &mut x, 20, "👨‍👩‍👧‍👦x", Style::default());

        assert_eq!(x, "👨‍👩‍👧‍👦x".width() as u16);
        assert_eq!(rows.len(), 0);
        assert!(matches!(
            &row.items[..],
            [RowItem::Text { x: 0, value, .. }] if value == "👨‍👩‍👧‍👦x"
        ));
    }

    #[test]
    fn joined_emoji_wraps_as_one_indivisible_glyph() {
        let mut rows = Vec::new();
        let mut row = FeedRow::new();
        let mut x = 0;

        push_text(&mut rows, &mut row, &mut x, 2, "a👨‍👩‍👧‍👦", Style::default());

        assert_eq!(rows.len(), 1);
        assert_eq!(x, 2);
        assert!(matches!(
            &row.items[..],
            [RowItem::Text { x: 0, value, .. }] if value == "👨‍👩‍👧‍👦"
        ));
    }

    #[test]
    fn chat_badges_render_before_the_username() {
        let message = ChatMessage {
            id: "1".to_owned(),
            sender_id: Some("alice-id".to_owned()),
            sender: "alice".to_owned(),
            color: None,
            badges: vec![
                ChatBadge {
                    name: "moderator".to_owned(),
                    version: Some("1".to_owned()),
                },
                ChatBadge {
                    name: "level".to_owned(),
                    version: Some("28".to_owned()),
                },
            ],
            fragments: vec![ChatFragment::Text("hello".to_owned())],
            moderation: None,
        };

        let rows = layout_messages(
            &VecDeque::from([FeedEntry::Message(message)]),
            80,
            &ImageStore::new(None, false),
        );
        let rendered = rows
            .iter()
            .flat_map(|row| &row.items)
            .filter_map(|item| match item {
                RowItem::Text { value, .. } => Some(value.as_str()),
                RowItem::Image { .. } => None,
            })
            .collect::<String>();

        assert!(rendered.starts_with("[M] [L28] alice: hello"));
        assert!(matches!(
            &rows[0].items[0],
            RowItem::Text { style, .. } if style.fg == Some(Color::Green)
        ));
    }

    #[test]
    fn chat_badges_can_be_hidden() {
        let message = ChatMessage {
            id: "1".to_owned(),
            sender_id: Some("alice-id".to_owned()),
            sender: "alice".to_owned(),
            color: None,
            badges: vec![ChatBadge {
                name: "moderator".to_owned(),
                version: Some("1".to_owned()),
            }],
            fragments: vec![ChatFragment::Text("hello".to_owned())],
            moderation: None,
        };

        let rows = layout_messages_with_badges(
            &VecDeque::from([FeedEntry::Message(message)]),
            80,
            &ImageStore::new(None, false),
            false,
        );
        let rendered = rows
            .iter()
            .flat_map(|row| &row.items)
            .filter_map(|item| match item {
                RowItem::Text { value, .. } => Some(value.as_str()),
                RowItem::Image { .. } => None,
            })
            .collect::<String>();

        assert_eq!(rendered, "alice: hello");
    }

    #[tokio::test]
    async fn emote_message_appears_only_after_its_image_settles() {
        let server = MockServer::start().await;
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(2, 2)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        Mock::given(path("/emote.png"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(png.into_inner()))
            .mount(&server)
            .await;
        let mut images = ImageStore::new(Some(Picker::halfblocks()), true);
        let mut channel =
            ChannelState::new(1, ChatTarget::from(TwitchTarget::parse("channel").unwrap()));
        channel.handle_chat_event(
            ChatEvent::Message(ChatMessage {
                id: "message-id".to_owned(),
                sender_id: None,
                sender: "alice".to_owned(),
                color: None,
                badges: vec![],
                fragments: vec![ChatFragment::Emote(EmoteRef {
                    provider: EmoteProvider::SevenTv,
                    id: "staged-test".to_owned(),
                    name: "Wave".to_owned(),
                    image_url: format!("{}/emote.png", server.uri()),
                    animated: false,
                })],
                moderation: None,
            }),
            &mut images,
        );

        assert!(channel.messages.is_empty());
        assert_eq!(channel.pending_messages.len(), 1);

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                assert!(images.drain_completed().is_empty());
                channel.flush_ready(&images);
                if channel.pending_messages.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        assert_eq!(channel.messages.len(), 1);
    }

    #[test]
    fn deleted_messages_are_retained_dimmed_and_labelled() {
        let mut channel =
            ChannelState::new(1, ChatTarget::from(TwitchTarget::parse("channel").unwrap()));
        channel.push_entry(FeedEntry::Message(ChatMessage {
            id: "message-id".to_owned(),
            sender_id: Some("user-id".to_owned()),
            sender: "alice".to_owned(),
            color: None,
            badges: vec![],
            fragments: vec![ChatFragment::Text("still visible".to_owned())],
            moderation: None,
        }));

        channel.apply_moderation(
            ModerationEvent::MessageDeleted {
                message_id: "message-id".to_owned(),
                sender: Some("alice".to_owned()),
                moderator: None,
            },
            &ImageStore::new(None, false),
        );

        let FeedEntry::Message(message) = &channel.messages[0] else {
            panic!("chat message should remain in the feed");
        };
        assert_eq!(message.moderation, Some(MessageModeration::Deleted));
        assert!(matches!(channel.messages[1], FeedEntry::Moderation(_)));

        let rows = layout_messages(&channel.messages, 80, &ImageStore::new(None, false));
        let rendered = rows
            .iter()
            .flat_map(|row| &row.items)
            .filter_map(|item| match item {
                RowItem::Text { value, .. } => Some(value.as_str()),
                RowItem::Image { .. } => None,
            })
            .collect::<String>();
        assert!(rendered.contains("alice: still visible"));
        assert!(rendered.contains("deleted by a moderator"));
        assert!(rendered.contains("Mod: Message from alice was deleted"));
        assert!(rows.iter().flat_map(|row| &row.items).any(|item| {
            matches!(item, RowItem::Text { style, .. } if style.add_modifier.contains(Modifier::DIM))
        }));
    }

    #[test]
    fn timeout_marks_all_visible_messages_from_the_user() {
        let mut channel =
            ChannelState::new(1, ChatTarget::from(KickTarget::parse("channel").unwrap()));
        for id in ["one", "two"] {
            channel.push_entry(FeedEntry::Message(ChatMessage {
                id: id.to_owned(),
                sender_id: Some("42".to_owned()),
                sender: "Alice".to_owned(),
                color: None,
                badges: vec![],
                fragments: vec![ChatFragment::Text("message".to_owned())],
                moderation: None,
            }));
        }

        channel.apply_moderation(
            ModerationEvent::UserTimedOut {
                user_id: Some("42".to_owned()),
                user: "alice".to_owned(),
                duration_seconds: Some(600),
                moderator: Some("modname".to_owned()),
            },
            &ImageStore::new(None, false),
        );

        assert!(channel.messages.iter().take(2).all(|entry| {
            matches!(
                entry,
                FeedEntry::Message(ChatMessage {
                    moderation: Some(MessageModeration::TimedOut {
                        duration_seconds: Some(600)
                    }),
                    ..
                })
            )
        }));
        let FeedEntry::Moderation(event) = &channel.messages[2] else {
            panic!("moderation notice should be retained");
        };
        assert_eq!(event.summary(), "alice was timed out for 10m by modname");
    }

    #[test]
    fn image_mode_reserves_consistent_height_for_text_only_messages() {
        let messages = VecDeque::from(
            [
                ChatMessage {
                    id: "1".to_owned(),
                    sender_id: None,
                    sender: "alice".to_owned(),
                    color: None,
                    badges: vec![],
                    fragments: vec![ChatFragment::Text("hello".to_owned())],
                    moderation: None,
                },
                ChatMessage {
                    id: "2".to_owned(),
                    sender_id: None,
                    sender: "bob".to_owned(),
                    color: None,
                    badges: vec![],
                    fragments: vec![ChatFragment::Text("world".to_owned())],
                    moderation: None,
                },
            ]
            .map(FeedEntry::Message),
        );
        let images = ImageStore::new(Some(Picker::halfblocks()), true);

        let rows = layout_messages(&messages, 40, &images);

        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.height == EMOTE_HEIGHT));
    }

    #[test]
    fn wrapped_messages_do_not_receive_extra_padding() {
        let message = ChatMessage {
            id: "1".to_owned(),
            sender_id: None,
            sender: "alice".to_owned(),
            color: None,
            badges: vec![],
            fragments: vec![ChatFragment::Text("a long message".to_owned())],
            moderation: None,
        };
        let images = ImageStore::new(Some(Picker::halfblocks()), true);

        let rows = layout_messages(&VecDeque::from([FeedEntry::Message(message)]), 8, &images);

        assert!(rows.len() >= 2);
        assert!(rows.iter().all(|row| row.height == 1));
    }

    #[test]
    fn bottom_start_accounts_for_tall_rows() {
        let rows = vec![
            FeedRow {
                items: vec![],
                height: 1,
            },
            FeedRow {
                items: vec![],
                height: 2,
            },
            FeedRow {
                items: vec![],
                height: 1,
            },
        ];
        assert_eq!(bottom_start(&rows, 3), 1);
        assert_eq!(bottom_start(&rows, 1), 2);
    }

    #[test]
    fn scrolling_disables_and_restores_follow_mode() {
        let mut app = AppState::new(false, false);
        app.add_channel(ChatTarget::from(TwitchTarget::parse("channel").unwrap()));
        let channel = &mut app.channels[0];
        channel.last_total_rows = 20;
        channel.last_viewport_rows = 5;
        channel.scroll_up(1);
        assert!(!channel.follow);
        channel.scroll_down(20, 19);
        assert!(channel.follow);
    }

    #[test]
    fn left_and_right_switch_channels_with_wrapping() {
        let mut app = AppState::new(false, false);
        for target in ["one", "two", "three"] {
            app.add_channel(ChatTarget::from(TwitchTarget::parse(target).unwrap()));
        }
        app.active = 0;
        app.handle_terminal_event(key(KeyCode::Right));
        assert_eq!(app.active, 1);
        app.handle_terminal_event(key(KeyCode::Left));
        assert_eq!(app.active, 0);
        app.handle_terminal_event(key(KeyCode::Left));
        assert_eq!(app.active, 2);
    }

    #[test]
    fn add_channel_overlay_validates_and_submits_targets() {
        let mut app = AppState::new(true, true);
        open_palette(&mut app);
        app.handle_terminal_event(key(KeyCode::Enter));
        for character in "My_Channel".chars() {
            app.handle_terminal_event(key(KeyCode::Char(character)));
        }
        let action = app.handle_terminal_event(key(KeyCode::Enter));
        assert!(matches!(
            action,
            UiAction::Add(target) if target.channel() == "my_channel"
        ));
    }

    #[test]
    fn duplicate_channels_show_an_inline_error() {
        let mut app = AppState::new(true, true);
        app.add_channel(ChatTarget::from(TwitchTarget::parse("existing").unwrap()));
        open_palette(&mut app);
        app.handle_terminal_event(key(KeyCode::Enter));
        for character in "EXISTING".chars() {
            app.handle_terminal_event(key(KeyCode::Char(character)));
        }
        assert!(matches!(
            app.handle_terminal_event(key(KeyCode::Enter)),
            UiAction::None
        ));
        assert!(matches!(
            &app.overlay,
            Overlay::AddChannel { error, .. } if error.contains("already open")
        ));
    }

    #[test]
    fn same_slug_on_twitch_and_kick_is_not_a_duplicate() {
        let mut app = AppState::new(true, true);
        app.add_channel(ChatTarget::from(TwitchTarget::parse("existing").unwrap()));
        open_palette(&mut app);
        app.handle_terminal_event(key(KeyCode::Down));
        app.handle_terminal_event(key(KeyCode::Enter));
        for character in "existing".chars() {
            app.handle_terminal_event(key(KeyCode::Char(character)));
        }
        assert!(matches!(
            app.handle_terminal_event(key(KeyCode::Enter)),
            UiAction::Add(ChatTarget::Kick(target)) if target.channel() == "existing"
        ));
    }

    #[test]
    fn removal_requires_confirmation_and_returns_stable_id() {
        let mut app = AppState::new(true, true);
        let id = app.add_channel(ChatTarget::from(TwitchTarget::parse("existing").unwrap()));
        open_palette(&mut app);
        app.handle_terminal_event(key(KeyCode::Down));
        app.handle_terminal_event(key(KeyCode::Down));
        app.handle_terminal_event(key(KeyCode::Enter));
        assert_eq!(app.overlay, Overlay::ConfirmRemove);
        assert!(matches!(
            app.handle_terminal_event(key(KeyCode::Char('y'))),
            UiAction::Remove(returned) if returned == id
        ));
    }

    #[test]
    fn image_preference_can_be_toggled_without_channels() {
        let mut app = AppState::new(false, true);
        open_palette(&mut app);
        app.handle_terminal_event(key(KeyCode::Down));
        app.handle_terminal_event(key(KeyCode::Down));
        assert!(matches!(
            app.handle_terminal_event(key(KeyCode::Enter)),
            UiAction::ToggleImages
        ));
    }

    #[test]
    fn badge_preference_can_be_toggled_without_channels() {
        let mut app = AppState::new_with_badges(false, true, true);
        open_palette(&mut app);
        for _ in 0..3 {
            app.handle_terminal_event(key(KeyCode::Down));
        }
        assert!(matches!(
            app.handle_terminal_event(key(KeyCode::Enter)),
            UiAction::ToggleBadges
        ));
    }

    #[test]
    fn super_and_control_k_toggle_the_command_palette() {
        let mut app = AppState::new(false, true);
        app.handle_terminal_event(command_key(KeyModifiers::SUPER));
        assert!(matches!(app.overlay, Overlay::CommandPalette { .. }));
        app.handle_terminal_event(command_key(KeyModifiers::CONTROL));
        assert_eq!(app.overlay, Overlay::None);
    }

    #[test]
    fn command_palette_filters_and_switches_channels_by_stable_id() {
        let mut app = AppState::new(false, true);
        app.add_channel(ChatTarget::from(TwitchTarget::parse("one").unwrap()));
        let second = app.add_channel(ChatTarget::from(TwitchTarget::parse("two").unwrap()));
        app.active = 0;
        open_palette(&mut app);
        for character in "switch two".chars() {
            app.handle_terminal_event(key(KeyCode::Char(character)));
        }
        assert!(matches!(
            app.handle_terminal_event(key(KeyCode::Enter)),
            UiAction::SwitchChannel(id) if id == second
        ));
    }

    #[test]
    fn command_palette_snapshot_contains_dynamic_commands() {
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new(true, true);
        app.add_channel(ChatTarget::from(TwitchTarget::parse("one").unwrap()));
        open_palette(&mut app);
        let mut images = ImageStore::new(None, false);

        terminal
            .draw(|frame| draw(frame, &mut app, &mut images))
            .unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("Command palette"));
        assert!(rendered.contains("Add Twitch channel"));
        assert!(rendered.contains("Add Kick channel"));
        assert!(rendered.contains("Remove [Twitch] #one"));
        assert!(rendered.contains("Disable emote images"));
        assert!(rendered.contains("Switch to [Twitch] #one"));
    }

    #[test]
    fn text_only_ui_snapshot_contains_core_chrome_and_message() {
        let backend = TestBackend::new(60, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new(false, false);
        app.add_channel(ChatTarget::from(TwitchTarget::parse("twitchdev").unwrap()));
        app.add_channel(ChatTarget::from(TwitchTarget::parse("second").unwrap()));
        app.active = 0;
        let active = &mut app.channels[0];
        active.connection = ConnectionState::Connected;
        active.status = "Connected anonymously".to_owned();
        active.messages.push_back(FeedEntry::Message(ChatMessage {
            id: "1".to_owned(),
            sender_id: None,
            sender: "alice".to_owned(),
            color: None,
            badges: vec![],
            fragments: vec![ChatFragment::Text("hello chat".to_owned())],
            moderation: None,
        }));
        let mut images = ImageStore::new(None, false);

        terminal
            .draw(|frame| draw(frame, &mut app, &mut images))
            .unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(!rendered.contains("TermChat"));
        assert!(rendered.contains("#twitchdev"));
        assert!(rendered.contains("#second"));
        assert!(rendered.contains('●'));
        assert!(rendered.contains("alice: hello chat"));
        assert!(!rendered.contains("Chat [Twitch] #twitchdev"));
        assert!(!rendered.contains("Connected anonymously"));
    }

    #[test]
    fn empty_ui_explains_how_to_add_a_channel() {
        let backend = TestBackend::new(60, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new(false, true);
        let mut images = ImageStore::new(None, false);
        terminal
            .draw(|frame| draw(frame, &mut app, &mut images))
            .unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("No Twitch or Kick channels are open"));
        assert!(rendered.contains("Super+K or Ctrl+K"));
        assert!(!rendered.contains("Welcome"));
    }
}
