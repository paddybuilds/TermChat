use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

use crate::target::{KickTarget, TwitchTarget};

#[derive(Debug, Parser)]
#[command(
    name = "termchat",
    version,
    about = "Watch livestream chat in your terminal"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Watch a Twitch channel without signing in.
    Twitch {
        /// One or more Twitch channel names or twitch.tv URLs.
        #[arg(num_args = 0.., value_parser = parse_twitch_target)]
        targets: Vec<TwitchTarget>,

        /// Choose whether terminal image protocols may be used for emotes.
        #[arg(long, value_enum)]
        images: Option<ImageMode>,

        /// Choose whether identity badges are shown before usernames.
        #[arg(long, value_enum)]
        badges: Option<BadgeMode>,
    },
    /// Watch a Kick channel without signing in.
    Kick {
        /// One or more Kick channel names or kick.com URLs.
        #[arg(num_args = 0.., value_parser = parse_kick_target)]
        targets: Vec<KickTarget>,

        /// Choose whether terminal image protocols may be used for emotes.
        #[arg(long, value_enum)]
        images: Option<ImageMode>,

        /// Choose whether identity badges are shown before usernames.
        #[arg(long, value_enum)]
        badges: Option<BadgeMode>,
    },
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum BadgeMode {
    #[default]
    On,
    Off,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ImageMode {
    #[default]
    Auto,
    Off,
}

fn parse_twitch_target(value: &str) -> Result<TwitchTarget, String> {
    TwitchTarget::parse(value).map_err(|error| error.to_string())
}

fn parse_kick_target(value: &str) -> Result<KickTarget, String> {
    KickTarget::parse(value).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiple_channels_and_image_mode() {
        let cli = Cli::try_parse_from(["termchat", "twitch", "first", "second", "--images", "off"])
            .unwrap();
        let Some(Command::Twitch {
            targets,
            images,
            badges,
        }) = cli.command
        else {
            panic!("expected twitch command");
        };
        assert_eq!(
            targets
                .iter()
                .map(TwitchTarget::channel)
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
        assert_eq!(images, Some(ImageMode::Off));
        assert_eq!(badges, None);
    }

    #[test]
    fn accepts_no_subcommand_or_empty_twitch_command() {
        assert!(Cli::try_parse_from(["termchat"]).unwrap().command.is_none());
        let cli = Cli::try_parse_from(["termchat", "twitch"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Twitch {
                targets,
                images: None,
                badges: None,
            }) if targets.is_empty()
        ));
    }

    #[test]
    fn parses_kick_channels() {
        let cli = Cli::try_parse_from([
            "termchat",
            "kick",
            "XQC",
            "https://kick.com/trainwreckstv",
            "--images",
            "off",
            "--badges",
            "off",
        ])
        .unwrap();
        let Some(Command::Kick {
            targets,
            images,
            badges,
        }) = cli.command
        else {
            panic!("expected kick command");
        };
        assert_eq!(
            targets.iter().map(KickTarget::channel).collect::<Vec<_>>(),
            vec!["xqc", "trainwreckstv"]
        );
        assert_eq!(images, Some(ImageMode::Off));
        assert_eq!(badges, Some(BadgeMode::Off));
    }
}
