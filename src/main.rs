use anyhow::{Context, Result};
use clap::Parser;
use termchat_live::{
    cli::{Cli, Command},
    config::Settings,
    target::ChatTarget,
    tui,
};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let mut settings = Settings::load().await.context("load TermChat settings")?;
    apply_command(&mut settings, cli.command);
    let targets = settings.targets();
    tui::run(targets, settings).await
}

fn apply_command(settings: &mut Settings, command: Option<Command>) {
    match command {
        Some(Command::Twitch {
            targets,
            images,
            badges,
        }) => {
            if let Some(images) = images {
                settings.images = images;
            }
            if let Some(badges) = badges {
                settings.badges = badges;
            }
            settings.merge_targets(targets.into_iter().map(ChatTarget::from));
        }
        Some(Command::Kick {
            targets,
            images,
            badges,
        }) => {
            if let Some(images) = images {
                settings.images = images;
            }
            if let Some(badges) = badges {
                settings.badges = badges;
            }
            settings.merge_targets(targets.into_iter().map(ChatTarget::from));
        }
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use termchat_live::cli::{BadgeMode, ImageMode};

    use super::*;

    #[test]
    fn omitted_flags_preserve_saved_preferences() {
        let mut settings = Settings {
            images: ImageMode::Off,
            badges: BadgeMode::Off,
            ..Settings::default()
        };
        let cli =
            Cli::try_parse_from(["termchat", "twitch", "twitchdev"]).expect("valid command line");

        apply_command(&mut settings, cli.command);

        assert_eq!(settings.images, ImageMode::Off);
        assert_eq!(settings.badges, BadgeMode::Off);
        assert_eq!(settings.targets().len(), 1);
    }

    #[test]
    fn explicit_flags_override_saved_preferences() {
        let mut settings = Settings {
            images: ImageMode::Off,
            badges: BadgeMode::Off,
            ..Settings::default()
        };
        let cli = Cli::try_parse_from([
            "termchat", "kick", "xqc", "--images", "auto", "--badges", "on",
        ])
        .expect("valid command line");

        apply_command(&mut settings, cli.command);

        assert_eq!(settings.images, ImageMode::Auto);
        assert_eq!(settings.badges, BadgeMode::On);
    }
}
