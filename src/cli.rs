use clap::{Parser, Subcommand, ValueEnum};

use crate::target::TwitchTarget;

#[derive(Debug, Parser)]
#[command(
    name = "termchat",
    version,
    about = "Watch livestream chat in your terminal"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Watch a Twitch channel without signing in.
    Twitch {
        /// One or more Twitch channel names or twitch.tv URLs.
        #[arg(required = true, num_args = 1.., value_parser = parse_twitch_target)]
        targets: Vec<TwitchTarget>,

        /// Choose whether terminal image protocols may be used for emotes.
        #[arg(long, value_enum, default_value_t = ImageMode::Auto)]
        images: ImageMode,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum ImageMode {
    #[default]
    Auto,
    Off,
}

fn parse_twitch_target(value: &str) -> Result<TwitchTarget, String> {
    TwitchTarget::parse(value).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiple_channels_and_image_mode() {
        let cli = Cli::try_parse_from(["termchat", "twitch", "first", "second", "--images", "off"])
            .unwrap();
        let Command::Twitch { targets, images } = cli.command;
        assert_eq!(
            targets
                .iter()
                .map(TwitchTarget::channel)
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
        assert_eq!(images, ImageMode::Off);
    }
}
