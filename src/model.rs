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
    Status(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatMessage {
    pub id: String,
    pub sender: String,
    pub color: Option<RgbColor>,
    pub fragments: Vec<ChatFragment>,
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
        format!("{}-{}", self.provider.as_str(), self.id)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum EmoteProvider {
    Twitch,
    SevenTv,
}

impl EmoteProvider {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Twitch => "twitch",
            Self::SevenTv => "7tv",
        }
    }
}
