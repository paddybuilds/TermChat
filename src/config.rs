use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::{
    cli::{BadgeMode, ImageMode},
    target::{ChatTarget, KickTarget, PlatformKind, TwitchTarget},
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SavedChannel {
    pub platform: PlatformKind,
    pub channel: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SavedChannelWire {
    Legacy(String),
    Current(SavedChannel),
}

fn deserialize_channels<'de, D>(deserializer: D) -> Result<Vec<SavedChannel>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let channels = Vec::<SavedChannelWire>::deserialize(deserializer)?;
    Ok(channels
        .into_iter()
        .map(|channel| match channel {
            SavedChannelWire::Legacy(channel) => SavedChannel {
                platform: PlatformKind::Twitch,
                channel,
            },
            SavedChannelWire::Current(channel) => channel,
        })
        .collect())
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Settings {
    #[serde(default, deserialize_with = "deserialize_channels")]
    pub channels: Vec<SavedChannel>,
    #[serde(default)]
    pub images: ImageMode,
    #[serde(default)]
    pub badges: BadgeMode,
}

impl Settings {
    pub async fn load() -> Result<Self> {
        Self::load_from(settings_path()).await
    }

    async fn load_from(path: PathBuf) -> Result<Self> {
        let bytes = match fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error).context("read TermChat settings"),
        };
        serde_json::from_slice(&bytes).context("parse TermChat settings")
    }

    pub async fn save(&self) -> Result<()> {
        self.save_to(settings_path()).await
    }

    async fn save_to(&self, path: PathBuf) -> Result<()> {
        let parent = path.parent().context("settings path has no parent")?;
        fs::create_dir_all(parent)
            .await
            .context("create TermChat settings directory")?;
        let bytes = serde_json::to_vec_pretty(self).context("serialize TermChat settings")?;
        let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
        fs::write(&temporary, bytes)
            .await
            .context("write TermChat settings")?;
        if path.exists() {
            fs::remove_file(&path)
                .await
                .context("replace TermChat settings")?;
        }
        fs::rename(&temporary, &path)
            .await
            .context("commit TermChat settings")
    }

    pub fn merge_targets(&mut self, targets: impl IntoIterator<Item = ChatTarget>) {
        for target in targets {
            if !self.channels.iter().any(|channel| {
                channel.platform == target.platform() && channel.channel == target.channel()
            }) {
                self.channels.push(SavedChannel {
                    platform: target.platform(),
                    channel: target.channel().to_owned(),
                });
            }
        }
    }

    pub fn targets(&self) -> Vec<ChatTarget> {
        self.channels
            .iter()
            .filter_map(|saved| match saved.platform {
                PlatformKind::Twitch => TwitchTarget::parse(&saved.channel)
                    .ok()
                    .map(ChatTarget::Twitch),
                PlatformKind::Kick => KickTarget::parse(&saved.channel).ok().map(ChatTarget::Kick),
            })
            .collect()
    }
}

pub fn settings_path() -> PathBuf {
    ProjectDirs::from("io", "termchat", "TermChat")
        .map(|directories| directories.config_dir().join("settings.json"))
        .unwrap_or_else(|| std::env::temp_dir().join("termchat-settings.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_normalized_targets_without_duplicates() {
        let mut settings = Settings {
            channels: vec![SavedChannel {
                platform: PlatformKind::Twitch,
                channel: "first".to_owned(),
            }],
            images: ImageMode::Auto,
            badges: BadgeMode::On,
        };
        settings.merge_targets([
            ChatTarget::from(TwitchTarget::parse("FIRST").unwrap()),
            ChatTarget::from(TwitchTarget::parse("Second").unwrap()),
            ChatTarget::from(KickTarget::parse("First").unwrap()),
        ]);
        assert_eq!(settings.channels.len(), 3);
        assert_eq!(settings.channels[1].channel, "second");
        assert_eq!(settings.channels[2].platform, PlatformKind::Kick);
    }

    #[tokio::test]
    async fn settings_round_trip_through_json() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested").join("settings.json");
        let settings = Settings {
            channels: vec![
                SavedChannel {
                    platform: PlatformKind::Twitch,
                    channel: "first".to_owned(),
                },
                SavedChannel {
                    platform: PlatformKind::Kick,
                    channel: "second".to_owned(),
                },
            ],
            images: ImageMode::Off,
            badges: BadgeMode::Off,
        };

        settings.save_to(path.clone()).await.unwrap();

        assert_eq!(Settings::load_from(path).await.unwrap(), settings);
    }

    #[test]
    fn legacy_string_channels_migrate_to_twitch() {
        let settings: Settings =
            serde_json::from_str(r#"{"channels":["first","second"],"images":"auto"}"#).unwrap();
        assert!(
            settings
                .channels
                .iter()
                .all(|channel| channel.platform == PlatformKind::Twitch)
        );
        assert_eq!(settings.targets().len(), 2);
        assert_eq!(settings.badges, BadgeMode::On);
    }
}
