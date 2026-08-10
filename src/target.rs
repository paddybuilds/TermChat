use std::fmt;

use thiserror::Error;
use url::Url;

#[derive(Clone, Debug, Eq, PartialEq)]
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
    #[error("unsupported URL host: {0}")]
    UnsupportedHost(String),
    #[error("the Twitch URL does not contain a channel")]
    MissingChannel,
    #[error("invalid Twitch channel name: {0}")]
    InvalidChannel(String),
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
}
