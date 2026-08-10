use std::{
    collections::HashMap,
    ops::Range,
    sync::{Arc, RwLock},
};

use twitch_irc::message::Emote as TwitchEmote;

use crate::model::{ChatFragment, EmoteProvider, EmoteRef};

#[derive(Clone, Debug, Default)]
pub struct SharedEmoteRegistry(Arc<RwLock<EmoteRegistry>>);

impl SharedEmoteRegistry {
    pub fn replace_global(&self, set_id: String, emotes: Vec<EmoteRef>) {
        let mut registry = self.0.write().unwrap_or_else(|error| error.into_inner());
        registry.global_set_id = Some(set_id);
        registry.global = by_name(emotes);
    }

    pub fn replace_channel(&self, user_id: String, set_id: String, emotes: Vec<EmoteRef>) {
        let mut registry = self.0.write().unwrap_or_else(|error| error.into_inner());
        registry.user_id = Some(user_id);
        registry.channel_set_id = Some(set_id);
        registry.channel = by_name(emotes);
    }

    pub fn snapshot_ids(&self) -> RegistryIds {
        let registry = self.0.read().unwrap_or_else(|error| error.into_inner());
        RegistryIds {
            user_id: registry.user_id.clone(),
            global_set_id: registry.global_set_id.clone(),
            channel_set_id: registry.channel_set_id.clone(),
        }
    }

    pub fn resolve(&self, name: &str) -> Option<EmoteRef> {
        let registry = self.0.read().unwrap_or_else(|error| error.into_inner());
        registry
            .channel
            .get(name)
            .or_else(|| registry.global.get(name))
            .cloned()
    }

    pub fn parse_message(&self, text: &str, twitch_emotes: &[TwitchEmote]) -> Vec<ChatFragment> {
        parse_message(text, twitch_emotes, |name| self.resolve(name))
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RegistryIds {
    pub user_id: Option<String>,
    pub global_set_id: Option<String>,
    pub channel_set_id: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct EmoteRegistry {
    global: HashMap<String, EmoteRef>,
    channel: HashMap<String, EmoteRef>,
    user_id: Option<String>,
    global_set_id: Option<String>,
    channel_set_id: Option<String>,
}

fn by_name(emotes: Vec<EmoteRef>) -> HashMap<String, EmoteRef> {
    emotes
        .into_iter()
        .map(|emote| (emote.name.clone(), emote))
        .collect()
}

pub fn parse_message<F>(
    text: &str,
    twitch_emotes: &[TwitchEmote],
    mut resolve_seventv: F,
) -> Vec<ChatFragment>
where
    F: FnMut(&str) -> Option<EmoteRef>,
{
    let char_to_byte = char_boundaries(text);
    let mut ranges: Vec<(Range<usize>, &TwitchEmote)> = twitch_emotes
        .iter()
        .filter_map(|emote| {
            let start = *char_to_byte.get(emote.char_range.start)?;
            let end = *char_to_byte.get(emote.char_range.end)?;
            Some((start..end, emote))
        })
        .collect();
    ranges.sort_by_key(|(range, _)| range.start);

    let mut fragments = Vec::new();
    let mut cursor = 0;
    for (range, emote) in ranges {
        if range.start < cursor || range.end > text.len() {
            continue;
        }
        push_seventv_text(
            &mut fragments,
            &text[cursor..range.start],
            &mut resolve_seventv,
        );
        fragments.push(ChatFragment::Emote(EmoteRef {
            provider: EmoteProvider::Twitch,
            id: emote.id.clone(),
            name: emote.code.clone(),
            image_url: format!(
                "https://static-cdn.jtvnw.net/emoticons/v2/{}/default/dark/2.0",
                emote.id
            ),
            animated: false,
        }));
        cursor = range.end;
    }
    push_seventv_text(&mut fragments, &text[cursor..], &mut resolve_seventv);
    coalesce_text(fragments)
}

fn char_boundaries(text: &str) -> Vec<usize> {
    text.char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(text.len()))
        .collect()
}

fn push_seventv_text<F>(fragments: &mut Vec<ChatFragment>, text: &str, resolve: &mut F)
where
    F: FnMut(&str) -> Option<EmoteRef>,
{
    let mut start = 0;
    let mut in_whitespace = text.chars().next().is_none_or(char::is_whitespace);

    for (index, character) in text.char_indices() {
        let whitespace = character.is_whitespace();
        if whitespace != in_whitespace {
            push_token(fragments, &text[start..index], in_whitespace, resolve);
            start = index;
            in_whitespace = whitespace;
        }
    }
    push_token(fragments, &text[start..], in_whitespace, resolve);
}

fn push_token<F>(fragments: &mut Vec<ChatFragment>, token: &str, whitespace: bool, resolve: &mut F)
where
    F: FnMut(&str) -> Option<EmoteRef>,
{
    if token.is_empty() {
        return;
    }
    if !whitespace && let Some(emote) = resolve(token) {
        fragments.push(ChatFragment::Emote(emote));
    } else {
        fragments.push(ChatFragment::Text(token.to_owned()));
    }
}

fn coalesce_text(fragments: Vec<ChatFragment>) -> Vec<ChatFragment> {
    let mut result = Vec::with_capacity(fragments.len());
    for fragment in fragments {
        match fragment {
            ChatFragment::Text(text) => {
                if let Some(ChatFragment::Text(previous)) = result.last_mut() {
                    previous.push_str(&text);
                } else if !text.is_empty() {
                    result.push(ChatFragment::Text(text));
                }
            }
            emote => result.push(emote),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seventv(name: &str, id: &str) -> EmoteRef {
        EmoteRef {
            provider: EmoteProvider::SevenTv,
            id: id.to_owned(),
            name: name.to_owned(),
            image_url: format!("https://cdn.test/{id}.webp"),
            animated: false,
        }
    }

    #[test]
    fn twitch_ranges_are_unicode_safe_and_take_precedence() {
        let twitch = TwitchEmote {
            id: "25".to_owned(),
            char_range: 2..7,
            code: "Kappa".to_owned(),
        };
        let fragments = parse_message("😀 Kappa Wave", &[twitch], |name| {
            (name == "Wave").then(|| seventv("Wave", "7"))
        });

        assert_eq!(fragments.len(), 4);
        assert_eq!(fragments[0], ChatFragment::Text("😀 ".to_owned()));
        assert!(matches!(
            &fragments[1],
            ChatFragment::Emote(emote) if emote.provider == EmoteProvider::Twitch
        ));
        assert_eq!(fragments[2], ChatFragment::Text(" ".to_owned()));
        assert!(matches!(
            &fragments[3],
            ChatFragment::Emote(emote) if emote.provider == EmoteProvider::SevenTv
        ));
    }

    #[test]
    fn seventv_requires_an_exact_whitespace_delimited_name() {
        let fragments = parse_message("Wave Wave!  Wave", &[], |name| {
            (name == "Wave").then(|| seventv("Wave", "7"))
        });
        assert_eq!(
            fragments
                .iter()
                .filter(|fragment| matches!(fragment, ChatFragment::Emote(_)))
                .count(),
            2
        );
        assert!(
            fragments
                .iter()
                .any(|fragment| fragment == &ChatFragment::Text(" Wave!  ".to_owned()))
        );
    }

    #[test]
    fn channel_emotes_override_global_emotes() {
        let registry = SharedEmoteRegistry::default();
        registry.replace_global("global".to_owned(), vec![seventv("Same", "global")]);
        registry.replace_channel(
            "user".to_owned(),
            "channel".to_owned(),
            vec![seventv("Same", "channel")],
        );
        assert_eq!(registry.resolve("Same").unwrap().id, "channel");
    }
}
