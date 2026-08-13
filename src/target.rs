use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PlatformKind {
    Twitch,
    Kick,
}

impl PlatformKind {
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Twitch => "Twitch",
            Self::Kick => "Kick",
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ChatTarget {
    Twitch(TwitchTarget),
    Kick(KickTarget),
}

impl ChatTarget {
    pub const fn platform(&self) -> PlatformKind {
        match self {
            Self::Twitch(_) => PlatformKind::Twitch,
            Self::Kick(_) => PlatformKind::Kick,
        }
    }

    pub fn channel(&self) -> &str {
        match self {
            Self::Twitch(target) => target.channel(),
            Self::Kick(target) => target.channel(),
        }
    }

    pub fn display_label(&self) -> String {
        format!("[{}] #{}", self.platform().display_name(), self.channel())
    }
}

impl From<TwitchTarget> for ChatTarget {
    fn from(target: TwitchTarget) -> Self {
        Self::Twitch(target)
    }
}

impl From<KickTarget> for ChatTarget {
    fn from(target: KickTarget) -> Self {
        Self::Kick(target)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct TwitchTarget {
    channel: String,
}

impl TwitchTarget {
    pub fn parse(input: &str) -> Result<Self, TargetError> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(TargetError::Empty);
        }

        let candidate = if trimmed.contains("://") {
            let url = Url::parse(trimmed).map_err(|_| TargetError::InvalidUrl)?;
            let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
            if host != "twitch.tv" && host != "www.twitch.tv" && host != "m.twitch.tv" {
                return Err(TargetError::UnsupportedHost(host));
            }
            url.path_segments()
                .and_then(|mut segments| segments.find(|segment| !segment.is_empty()))
                .ok_or(TargetError::MissingChannel)?
                .to_owned()
        } else {
            trimmed.trim_start_matches('#').to_owned()
        };

        let channel = candidate.to_ascii_lowercase();
        if channel.len() > 25
            || channel.len() < 3
            || !channel
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            return Err(TargetError::InvalidChannel(candidate));
        }

        Ok(Self { channel })
    }

    pub fn channel(&self) -> &str {
        &self.channel
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct KickTarget {
    channel: String,
}

impl KickTarget {
    pub fn parse(input: &str) -> Result<Self, TargetError> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(TargetError::Empty);
        }

        let candidate = if trimmed.contains("://") {
            let url = Url::parse(trimmed).map_err(|_| TargetError::InvalidKickUrl)?;
            let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
            if host != "kick.com" && host != "www.kick.com" && host != "m.kick.com" {
                return Err(TargetError::UnsupportedHost(host));
            }
            let segments: Vec<&str> = url
                .path_segments()
                .map(|segments| segments.filter(|segment| !segment.is_empty()).collect())
                .unwrap_or_default();
            match segments.as_slice() {
                ["popout", channel, "chat", ..] => (*channel).to_owned(),
                [channel, ..] => (*channel).to_owned(),
                _ => return Err(TargetError::MissingChannel),
            }
        } else {
            trimmed.trim_start_matches('#').to_owned()
        };

        let channel = candidate.to_ascii_lowercase();
        if channel.is_empty()
            || channel.len() > 64
            || !channel.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
            })
        {
            return Err(TargetError::InvalidKickChannel(candidate));
        }

        Ok(Self { channel })
    }

    pub fn channel(&self) -> &str {
        &self.channel
    }
}

impl fmt::Display for KickTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.channel.fmt(formatter)
    }
}

impl fmt::Display for TwitchTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.channel.fmt(formatter)
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum TargetError {
    #[error("channel cannot be empty")]
    Empty,
    #[error("invalid Twitch URL")]
    InvalidUrl,
    #[error("invalid Kick URL")]
    InvalidKickUrl,
    #[error("unsupported URL host: {0}")]
    UnsupportedHost(String),
    #[error("the URL does not contain a channel")]
    MissingChannel,
    #[error("invalid Twitch channel name: {0}")]
    InvalidChannel(String),
    #[error("invalid Kick channel name: {0}")]
    InvalidKickChannel(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_names_and_urls() {
        for (input, expected) in [
            ("Pajlada", "pajlada"),
            ("#twitchdev", "twitchdev"),
            ("https://www.twitch.tv/Some_Channel", "some_channel"),
            ("https://m.twitch.tv/shroud/videos", "shroud"),
        ] {
            assert_eq!(TwitchTarget::parse(input).unwrap().channel(), expected);
        }
    }

    #[test]
    fn rejects_invalid_targets() {
        assert!(matches!(TwitchTarget::parse(""), Err(TargetError::Empty)));
        assert!(matches!(
            TwitchTarget::parse("https://youtube.com/test"),
            Err(TargetError::UnsupportedHost(_))
        ));
        assert!(matches!(
            TwitchTarget::parse("not a channel"),
            Err(TargetError::InvalidChannel(_))
        ));
    }

    #[test]
    fn accepts_kick_names_and_urls() {
        for (input, expected) in [
            ("XQC", "xqc"),
            ("#some-channel", "some-channel"),
            ("https://kick.com/Trainwreckstv", "trainwreckstv"),
            ("https://www.kick.com/xqc/about", "xqc"),
            ("https://m.kick.com/xqc", "xqc"),
            ("https://kick.com/popout/xqc/chat", "xqc"),
        ] {
            assert_eq!(KickTarget::parse(input).unwrap().channel(), expected);
        }
    }

    #[test]
    fn rejects_invalid_kick_targets() {
        assert!(matches!(KickTarget::parse(""), Err(TargetError::Empty)));
        assert!(matches!(
            KickTarget::parse("https://twitch.tv/test"),
            Err(TargetError::UnsupportedHost(_))
        ));
        assert!(matches!(
            KickTarget::parse("not a channel"),
            Err(TargetError::InvalidKickChannel(_))
        ));
    }
}
