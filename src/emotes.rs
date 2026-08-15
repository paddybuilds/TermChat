use std::{
    collections::HashMap,
    ops::Range,
    sync::{Arc, RwLock},
};

use twitch_irc::message::Emote as TwitchEmote;

use crate::{
    model::{ChatFragment, EmoteProvider, EmoteRef},
    text::DisplayTextSanitizer,
};

#[derive(Clone, Debug, Default)]
pub struct SharedGlobalEmotes(Arc<RwLock<GlobalEmotes>>);

#[derive(Clone, Debug)]
pub struct SharedEmoteRegistry {
    global: SharedGlobalEmotes,
    channel: Arc<RwLock<ChannelEmotes>>,
}

impl Default for SharedEmoteRegistry {
    fn default() -> Self {
        Self::with_global(SharedGlobalEmotes::default())
    }
}

impl SharedEmoteRegistry {
    pub fn with_global(global: SharedGlobalEmotes) -> Self {
        Self {
            global,
            channel: Arc::new(RwLock::new(ChannelEmotes::default())),
        }
    }

    pub fn replace_global(&self, set_id: String, emotes: Vec<EmoteRef>) {
        self.global.replace(set_id, emotes);
    }

    pub fn replace_channel(&self, user_id: String, set_id: String, emotes: Vec<EmoteRef>) {
        let mut registry = self
            .channel
            .write()
            .unwrap_or_else(|error| error.into_inner());
        registry.user_id = Some(user_id);
        registry.channel_set_id = Some(set_id);
        registry.emotes = by_name(emotes);
    }

    pub fn snapshot_ids(&self) -> RegistryIds {
        let global = self
            .global
            .0
            .read()
            .unwrap_or_else(|error| error.into_inner());
        let channel = self
            .channel
            .read()
            .unwrap_or_else(|error| error.into_inner());
        RegistryIds {
            user_id: channel.user_id.clone(),
            global_set_id: global.set_id.clone(),
            channel_set_id: channel.channel_set_id.clone(),
        }
    }

    pub fn has_global_emotes(&self) -> bool {
        self.global
            .0
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .set_id
            .is_some()
    }

    pub fn resolve(&self, name: &str) -> Option<EmoteRef> {
        let channel = self
            .channel
            .read()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(emote) = channel.emotes.get(name) {
            return Some(emote.clone());
        }
        self.global
            .0
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .emotes
            .get(name)
            .cloned()
    }

    pub fn parse_message(&self, text: &str, twitch_emotes: &[TwitchEmote]) -> Vec<ChatFragment> {
        parse_message(text, twitch_emotes, |name| self.resolve(name))
    }

    pub fn parse_kick_message(&self, text: &str) -> Vec<ChatFragment> {
        parse_kick_message(text, |name| self.resolve(name))
    }
}

impl SharedGlobalEmotes {
    pub fn replace(&self, set_id: String, emotes: Vec<EmoteRef>) {
        let mut global = self.0.write().unwrap_or_else(|error| error.into_inner());
        global.set_id = Some(set_id);
        global.emotes = by_name(emotes);
    }
}

pub fn parse_kick_message<F>(text: &str, mut resolve_seventv: F) -> Vec<ChatFragment>
where
    F: FnMut(&str) -> Option<EmoteRef>,
{
    const PREFIX: &str = "[emote:";
    let mut fragments = Vec::new();
    let mut cursor = 0;
    let mut scan = 0;
    while let Some(relative_start) = text[scan..].find(PREFIX) {
        let start = scan + relative_start;
        let Some(relative_end) = text[start..].find(']') else {
            break;
        };
        let end = start + relative_end + 1;
        let body = &text[start + PREFIX.len()..end - 1];
        let Some((id, name)) = body.split_once(':') else {
            scan = end;
            continue;
        };
        if id.is_empty()
            || !id.chars().all(|character| character.is_ascii_digit())
            || name.is_empty()
        {
            scan = end;
            continue;
        }

        push_seventv_text(&mut fragments, &text[cursor..start], &mut resolve_seventv);
        fragments.push(ChatFragment::Emote(EmoteRef {
            provider: EmoteProvider::Kick,
            id: id.to_owned(),
            name: name.to_owned(),
            image_url: format!("https://files.kick.com/emotes/{id}/fullsize"),
            animated: false,
        }));
        cursor = end;
        scan = end;
    }
    push_seventv_text(&mut fragments, &text[cursor..], &mut resolve_seventv);
    sanitize_text_fragments(coalesce_text(fragments))
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RegistryIds {
    pub user_id: Option<String>,
    pub global_set_id: Option<String>,
    pub channel_set_id: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct GlobalEmotes {
    set_id: Option<String>,
    emotes: HashMap<String, EmoteRef>,
}

#[derive(Clone, Debug, Default)]
struct ChannelEmotes {
    user_id: Option<String>,
    channel_set_id: Option<String>,
    emotes: HashMap<String, EmoteRef>,
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
    sanitize_text_fragments(coalesce_text(fragments))
}

fn sanitize_text_fragments(fragments: Vec<ChatFragment>) -> Vec<ChatFragment> {
    let mut sanitizer = DisplayTextSanitizer::chat_message();
    fragments
        .into_iter()
        .filter_map(|fragment| match fragment {
            ChatFragment::Text(text) => {
                let text = sanitizer.sanitize(&text);
                (!text.is_empty()).then_some(ChatFragment::Text(text))
            }
            emote => Some(emote),
        })
        .collect()
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

    #[test]
    fn globals_are_shared_across_channels_while_channel_sets_stay_isolated() {
        let global = SharedGlobalEmotes::default();
        let first = SharedEmoteRegistry::with_global(global.clone());
        let second = SharedEmoteRegistry::with_global(global.clone());
        global.replace("global".to_owned(), vec![seventv("Global", "global")]);
        first.replace_channel(
            "first-user".to_owned(),
            "first-set".to_owned(),
            vec![seventv("Local", "local")],
        );

        assert_eq!(first.resolve("Global").unwrap().id, "global");
        assert_eq!(second.resolve("Global").unwrap().id, "global");
        assert_eq!(first.resolve("Local").unwrap().id, "local");
        assert!(second.resolve("Local").is_none());
        assert_eq!(
            second.snapshot_ids().global_set_id.as_deref(),
            Some("global")
        );
        assert_eq!(second.snapshot_ids().channel_set_id, None);
    }

    #[test]
    fn kick_emotes_take_precedence_over_seventv_and_keep_unicode() {
        let fragments = parse_kick_message("😀 [emote:123:Wave] Wave [emote:123:Wave]", |name| {
            (name == "Wave").then(|| seventv("Wave", "7"))
        });
        assert_eq!(
            fragments
                .iter()
                .filter(|fragment| matches!(fragment, ChatFragment::Emote(emote) if emote.provider == EmoteProvider::Kick))
                .count(),
            2
        );
        assert!(fragments.iter().any(
            |fragment| matches!(fragment, ChatFragment::Emote(emote) if emote.provider == EmoteProvider::SevenTv)
        ));
    }

    #[test]
    fn malformed_kick_markup_remains_text() {
        let fragments = parse_kick_message("before [emote:nope:Wave] after [emote:12", |_| None);
        assert_eq!(
            fragments,
            vec![ChatFragment::Text(
                "before [emote:nope:Wave] after [emote:12".to_owned()
            )]
        );
    }

    #[test]
    fn chat_text_is_sanitized_after_emote_parsing() {
        let fragments = parse_kick_message(
            "hello\x1b\u{202E} a\u{0301}\u{0301}\u{0301}\u{0301}\u{0301}\u{0301}\u{0301}\u{0301}\u{0301}",
            |_| None,
        );

        assert_eq!(
            fragments,
            vec![ChatFragment::Text(
                "hello a\u{0301}\u{0301}\u{0301}\u{0301}\u{0301}\u{0301}\u{0301}\u{0301}"
                    .to_owned()
            )]
        );
    }

    #[test]
    fn kick_emotes_have_distinct_cache_keys() {
        let fragments = parse_kick_message("[emote:123:Wave]", |_| None);
        let ChatFragment::Emote(emote) = &fragments[0] else {
            panic!("expected emote");
        };
        assert_eq!(emote.cache_key(), "kick-123");
    }
}
