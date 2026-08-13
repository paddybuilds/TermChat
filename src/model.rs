#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConnectionState {
    Connecting,
    Connected,
    Reconnecting,
    Disconnected,
}

#[derive(Clone, Debug)]
pub enum ChatEvent {
    Connected,
    Disconnected { reason: String },
    Message(ChatMessage),
    Moderation(ModerationEvent),
    Status(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatMessage {
    pub id: String,
    pub sender_id: Option<String>,
    pub sender: String,
    pub color: Option<RgbColor>,
    pub badges: Vec<ChatBadge>,
    pub fragments: Vec<ChatFragment>,
    pub moderation: Option<MessageModeration>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatBadge {
    pub name: String,
    pub version: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MessageModeration {
    Deleted,
    TimedOut { duration_seconds: Option<u64> },
    Banned,
    ChatCleared,
}

impl MessageModeration {
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Deleted => "deleted by a moderator",
            Self::TimedOut { .. } => "removed after a timeout",
            Self::Banned => "removed after a ban",
            Self::ChatCleared => "removed when chat was cleared",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModerationEvent {
    MessageDeleted {
        message_id: String,
        sender: Option<String>,
        moderator: Option<String>,
    },
    UserTimedOut {
        user_id: Option<String>,
        user: String,
        duration_seconds: Option<u64>,
        moderator: Option<String>,
    },
    UserBanned {
        user_id: Option<String>,
        user: String,
        moderator: Option<String>,
    },
    UserUnbanned {
        user_id: Option<String>,
        user: String,
        moderator: Option<String>,
    },
    ChatCleared {
        moderator: Option<String>,
    },
}

impl ModerationEvent {
    pub fn summary(&self) -> String {
        let by = |moderator: &Option<String>| {
            moderator
                .as_deref()
                .map(|name| format!(" by {name}"))
                .unwrap_or_default()
        };
        match self {
            Self::MessageDeleted {
                sender, moderator, ..
            } => format!(
                "Message{} was deleted{}",
                sender
                    .as_deref()
                    .map(|name| format!(" from {name}"))
                    .unwrap_or_default(),
                by(moderator)
            ),
            Self::UserTimedOut {
                user,
                duration_seconds,
                moderator,
                ..
            } => format!(
                "{user} was timed out{}{}",
                duration_seconds
                    .map(|seconds| format!(" for {}", format_duration(seconds)))
                    .unwrap_or_default(),
                by(moderator)
            ),
            Self::UserBanned {
                user, moderator, ..
            } => format!("{user} was banned{}", by(moderator)),
            Self::UserUnbanned {
                user, moderator, ..
            } => format!("{user} was unbanned{}", by(moderator)),
            Self::ChatCleared { moderator } => format!("Chat was cleared{}", by(moderator)),
        }
    }
}

fn format_duration(seconds: u64) -> String {
    if seconds.is_multiple_of(3_600) {
        format!("{}h", seconds / 3_600)
    } else if seconds.is_multiple_of(60) {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChatFragment {
    Text(String),
    Emote(EmoteRef),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RgbColor {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl RgbColor {
    pub fn from_hex(value: &str) -> Option<Self> {
        let value = value.strip_prefix('#').unwrap_or(value);
        if value.len() != 6 {
            return None;
        }
        Some(Self {
            red: u8::from_str_radix(&value[0..2], 16).ok()?,
            green: u8::from_str_radix(&value[2..4], 16).ok()?,
            blue: u8::from_str_radix(&value[4..6], 16).ok()?,
        })
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct EmoteRef {
    pub provider: EmoteProvider,
    pub id: String,
    pub name: String,
    pub image_url: String,
    pub animated: bool,
}

impl EmoteRef {
    pub fn cache_key(&self) -> String {
        match self.provider {
            // Keep high-resolution 7TV assets separate from older cached 2x files.
            EmoteProvider::SevenTv => format!("{}-hq-{}", self.provider.as_str(), self.id),
            _ => format!("{}-{}", self.provider.as_str(), self.id),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum EmoteProvider {
    Twitch,
    Kick,
    SevenTv,
}

impl EmoteProvider {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Twitch => "twitch",
            Self::Kick => "kick",
            Self::SevenTv => "7tv",
        }
    }
}
