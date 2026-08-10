use std::{
    collections::VecDeque,
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
use futures_util::StreamExt;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use ratatui_image::{
    Image,
    picker::{Picker, ProtocolType},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    cli::ImageMode,
    image_store::{EMOTE_HEIGHT, EMOTE_WIDTH, ImageStore},
    model::{ChatEvent, ChatFragment, ChatMessage, ConnectionState, EmoteRef},
    platform::PlatformAdapter,
};

const MAX_MESSAGES: usize = 1_000;
type PanicHook = Box<dyn Fn(&panic::PanicHookInfo<'_>) + Send + Sync + 'static>;

pub async fn run(adapters: Vec<Arc<dyn PlatformAdapter>>, image_mode: ImageMode) -> Result<()> {
    anyhow::ensure!(!adapters.is_empty(), "at least one channel is required");
    let targets: Vec<String> = adapters
        .iter()
        .map(|adapter| adapter.display_target().to_owned())
        .collect();
    let mut terminal = TerminalSession::enter()?;
    let picker = select_picker(image_mode);
    let mut images = ImageStore::new(picker);
    let mut event_stream = EventStream::new();
    let (event_tx, mut event_rx) = mpsc::channel::<(usize, ChatEvent)>(512);
    let cancellation = CancellationToken::new();
    let mut adapter_tasks = Vec::with_capacity(adapters.len() * 2);
    for (index, adapter) in adapters.into_iter().enumerate() {
        let adapter_cancellation = cancellation.clone();
        let (adapter_events, mut adapter_rx) = mpsc::channel(256);
        let tagged_events = event_tx.clone();
        adapter_tasks.push(tokio::spawn(async move {
            if let Err(error) = adapter
                .run(adapter_events.clone(), adapter_cancellation)
                .await
            {
                let _ = adapter_events
                    .send(ChatEvent::Disconnected {
                        reason: format!("{error:#}"),
                    })
                    .await;
            }
        }));
        adapter_tasks.push(tokio::spawn(async move {
            while let Some(event) = adapter_rx.recv().await {
                if tagged_events.send((index, event)).await.is_err() {
                    return;
                }
            }
        }));
    }
    drop(event_tx);

    let mut app = AppState::new(targets, images.enabled());
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    let result = loop {
        terminal
            .terminal
            .draw(|frame| draw(frame, &mut app, &mut images))
            .context("draw terminal UI")?;

        tokio::select! {
            _ = ticker.tick() => {
                for error in images.drain_completed() {
                    app.channels[app.active].status =
                        format!("An emote image could not be rendered: {error}");
                }
            }
            event = event_rx.recv() => {
                match event {
                    Some((channel, event)) => app.handle_chat_event(channel, event, &mut images),
                    None => break Ok(()),
                }
            }
            terminal_event = event_stream.next() => {
                match terminal_event {
                    Some(Ok(event)) => {
                        if app.handle_terminal_event(event) {
                            break Ok(());
                        }
                    }
                    Some(Err(error)) => break Err(error).context("read terminal input"),
                    None => break Ok(()),
                }
            }
        }
    };

    cancellation.cancel();
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        futures_util::future::join_all(adapter_tasks),
    )
    .await;
    result
}

fn select_picker(mode: ImageMode) -> Option<Picker> {
    if mode == ImageMode::Off {
        return None;
    }
    let picker = Picker::from_query_stdio().ok()?;
    (picker.protocol_type() != ProtocolType::Halfblocks).then_some(picker)
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
}

struct ChannelState {
    target: String,
    messages: VecDeque<ChatMessage>,
    connection: ConnectionState,
    status: String,
    follow: bool,
    viewport_start: usize,
    last_total_rows: usize,
    last_viewport_rows: usize,
}

impl AppState {
    fn new(targets: Vec<String>, images_enabled: bool) -> Self {
        Self {
            channels: targets.into_iter().map(ChannelState::new).collect(),
            active: 0,
            images_enabled,
        }
    }

    fn handle_chat_event(&mut self, channel: usize, event: ChatEvent, images: &mut ImageStore) {
        let Some(state) = self.channels.get_mut(channel) else {
            return;
        };
        state.handle_chat_event(event, images);
    }

    fn handle_terminal_event(&mut self, event: Event) -> bool {
        let Event::Key(key) = event else {
            return false;
        };
        if key.kind == crossterm::event::KeyEventKind::Release {
            return false;
        }
        if key.code == KeyCode::Char('q') || is_control_key(key, 'c') {
            return true;
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
            _ => self.channels[self.active].handle_scroll_key(key.code),
        }
        false
    }
}

impl ChannelState {
    fn new(target: String) -> Self {
        Self {
            target,
            messages: VecDeque::new(),
            connection: ConnectionState::Connecting,
            status: "Starting Twitch chat…".to_owned(),
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
                self.messages.push_back(message);
                if self.messages.len() > MAX_MESSAGES {
                    self.messages.pop_front();
                    if !self.follow {
                        self.viewport_start = self.viewport_start.saturating_sub(1);
                    }
                }
            }
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

fn is_control_key(key: KeyEvent, character: char) -> bool {
    key.code == KeyCode::Char(character) && key.modifiers.contains(KeyModifiers::CONTROL)
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
    let [header, feed, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let active_index = app.active;
    let active_connection = &app.channels[active_index].connection;
    let state_color = match active_connection {
        ConnectionState::Connected => Color::Green,
        ConnectionState::Connecting | ConnectionState::Reconnecting => Color::Yellow,
        ConnectionState::Disconnected => Color::Red,
    };
    let mut tabs = vec![Span::styled(
        " TermChat ",
        Style::default().add_modifier(Modifier::BOLD),
    )];
    for (index, channel) in app.channels.iter().enumerate() {
        let style = if index == active_index {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        tabs.push(Span::styled(format!(" #{} ", channel.target), style));
    }
    tabs.push(Span::raw(" "));
    tabs.push(Span::styled(
        connection_label(active_connection),
        Style::default().fg(state_color),
    ));
    frame.render_widget(Paragraph::new(Line::from(tabs)), header);

    let active = &mut app.channels[active_index];
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Chat #{} ", active.target));
    let inner = block.inner(feed);
    frame.render_widget(block, feed);
    if inner.width > 0 && inner.height > 0 {
        let rows = layout_messages(&active.messages, inner.width, images);
        render_rows(frame, inner, &rows, active, images);
    }

    let image_label = if app.images_enabled {
        "images:auto"
    } else {
        "images:text"
    };
    let follow_label = if active.follow { "following" } else { "paused" };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(format!(" {} ", active.status)),
            Span::styled(
                format!(
                    "| {image_label} | {follow_label} | Left/Right channel | Up/Down/PgUp/PgDn scroll | q quit"
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ])),
        footer,
    );
}

fn connection_label(state: &ConnectionState) -> &'static str {
    match state {
        ConnectionState::Connecting => "connecting",
        ConnectionState::Connected => "connected",
        ConnectionState::Reconnecting => "reconnecting",
        ConnectionState::Disconnected => "disconnected",
    }
}

fn layout_messages(
    messages: &VecDeque<ChatMessage>,
    width: u16,
    images: &ImageStore,
) -> Vec<FeedRow> {
    let mut rows = Vec::new();
    for message in messages {
        let mut row = FeedRow::new();
        let mut x = 0;
        let color = message
            .color
            .map(|color| Color::Rgb(color.red, color.green, color.blue))
            .unwrap_or(Color::Cyan);
        push_text(
            &mut rows,
            &mut row,
            &mut x,
            width,
            &format!("{}: ", message.sender),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        );
        for fragment in &message.fragments {
            match fragment {
                ChatFragment::Text(text) => {
                    push_text(&mut rows, &mut row, &mut x, width, text, Style::default())
                }
                ChatFragment::Emote(emote) if image_is_ready(emote, images) => {
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
                    Style::default().fg(Color::LightMagenta),
                ),
            }
        }
        rows.push(row);
    }
    rows
}

fn image_is_ready(emote: &EmoteRef, images: &ImageStore) -> bool {
    !emote.animated && images.contains(&emote.cache_key())
}

fn push_text(
    rows: &mut Vec<FeedRow>,
    row: &mut FeedRow,
    x: &mut u16,
    width: u16,
    text: &str,
    style: Style,
) {
    for character in text.chars() {
        if character == '\n' {
            rows.push(std::mem::replace(row, FeedRow::new()));
            *x = 0;
            continue;
        }
        let char_width = character.width().unwrap_or(0).min(u16::MAX as usize) as u16;
        if char_width > 0 && *x > 0 && x.saturating_add(char_width) > width {
            rows.push(std::mem::replace(row, FeedRow::new()));
            *x = 0;
        }
        append_text_item(row, *x, character, style);
        *x = x.saturating_add(char_width).min(width);
    }
}

fn append_text_item(row: &mut FeedRow, x: u16, character: char, style: Style) {
    if let Some(RowItem::Text {
        x: previous_x,
        value,
        style: previous_style,
    }) = row.items.last_mut()
        && *previous_style == style
        && previous_x.saturating_add(value.width() as u16) == x
    {
        value.push(character);
        return;
    }
    row.items.push(RowItem::Text {
        x,
        value: character.to_string(),
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
    use super::*;
    use crate::model::{ChatFragment, RgbColor};
    use ratatui::backend::TestBackend;

    #[test]
    fn text_layout_wraps_and_keeps_sender_style() {
        let message = ChatMessage {
            id: "1".to_owned(),
            sender: "alice".to_owned(),
            color: Some(RgbColor {
                red: 1,
                green: 2,
                blue: 3,
            }),
            fragments: vec![ChatFragment::Text("hello world".to_owned())],
        };
        let rows = layout_messages(&VecDeque::from([message]), 10, &ImageStore::new(None));
        assert!(rows.len() >= 2);
        assert!(matches!(rows[0].items[0], RowItem::Text { .. }));
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
        let mut app = AppState::new(vec!["channel".to_owned()], false);
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
        let mut app = AppState::new(
            vec!["one".to_owned(), "two".to_owned(), "three".to_owned()],
            false,
        );
        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Right,
            KeyModifiers::NONE,
        )));
        assert_eq!(app.active, 1);
        app.handle_terminal_event(Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)));
        assert_eq!(app.active, 0);
        app.handle_terminal_event(Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)));
        assert_eq!(app.active, 2);
    }

    #[test]
    fn text_only_ui_snapshot_contains_core_chrome_and_message() {
        let backend = TestBackend::new(60, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = AppState::new(vec!["twitchdev".to_owned(), "second".to_owned()], false);
        let active = &mut app.channels[0];
        active.connection = ConnectionState::Connected;
        active.status = "Connected anonymously".to_owned();
        active.messages.push_back(ChatMessage {
            id: "1".to_owned(),
            sender: "alice".to_owned(),
            color: None,
            fragments: vec![ChatFragment::Text("hello chat".to_owned())],
        });
        let mut images = ImageStore::new(None);

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
        assert!(rendered.contains("TermChat"));
        assert!(rendered.contains("#twitchdev"));
        assert!(rendered.contains("#second"));
        assert!(rendered.contains("alice: hello chat"));
        assert!(rendered.contains("Connected anonymously"));
    }
}
