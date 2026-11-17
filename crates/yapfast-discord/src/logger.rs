//! Messages kept after they're deleted or edited: a port of the
//! MessageLoggerV2 plugin for BetterDiscord, without its log window.
//!
//! A deleted message, or one of a purge, stays in the channel it was in, marked
//! [`Message::deleted`], and an edited one keeps what it said before in
//! [`Message::history`]. Only messages Yapfast holds can be kept, which are
//! those of the channels it has open and the newest of those it showed lately:
//! the cache of `discord-messages/` (see [`crate::cache`]) writes them with
//! these marks, encrypted, so they're still there after a restart. A ghost
//! ping is a kept message that mentions this account, which the timeline
//! already highlights.
//!
//! One setting for the process, as the account's are, which the application
//! sets when the switches change.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::message::Message;

static DELETED: AtomicBool = AtomicBool::new(false);
static EDITS: AtomicBool = AtomicBool::new(false);
static BOTS: AtomicBool = AtomicBool::new(false);

/// How many earlier versions of a message are kept.
pub const HISTORY: usize = 10;

/// What is kept.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// Deleted messages stay, marked.
    pub deleted: bool,
    /// Edited messages keep what they said before.
    pub edits: bool,
    /// Bots' and webhooks' messages are kept too, which they aren't unless
    /// this is set.
    pub bots: bool,
}

/// Sets what is kept.
pub fn set(options: Options) {
    DELETED.store(options.deleted, Ordering::Relaxed);
    EDITS.store(options.edits, Ordering::Relaxed);
    BOTS.store(options.bots, Ordering::Relaxed);
}

fn wanted(message: &Message) -> bool {
    BOTS.load(Ordering::Relaxed) || !message.bot
}

/// Whether kept deleted messages show.
pub fn shows_deleted() -> bool {
    DELETED.load(Ordering::Relaxed)
}

/// Whether kept earlier versions show.
pub fn shows_edits() -> bool {
    EDITS.load(Ordering::Relaxed)
}

/// Whether `message` is one the log holds: deleted, or with earlier versions.
pub fn is_logged(message: &Message) -> bool {
    message.deleted || !message.history.is_empty()
}

/// Whether `message`, deleted now, stays.
pub fn keeps_deleted(message: &Message) -> bool {
    DELETED.load(Ordering::Relaxed) && !message.deleted && wanted(message)
}

/// `old`, which an edit changed into `new`, with what it said before: its
/// earlier versions and now the one it had, or its own if edits aren't kept
/// or its words didn't change.
pub fn history_after_edit(old: &Message, new: &Message) -> Vec<String> {
    let mut history = old.history.clone();
    if EDITS.load(Ordering::Relaxed)
        && wanted(old)
        && !old.content.is_empty()
        && old.content != new.content
    {
        history.push(old.content.clone());
        if history.len() > HISTORY {
            history.remove(0);
        }
    }
    history
}

/// `message` as it shows: its earlier versions quoted above it. A deleted one
/// shows its words as they were, which the timeline tints.
pub fn shown_content(message: &Message) -> std::borrow::Cow<'_, str> {
    if message.history.is_empty() {
        return std::borrow::Cow::Borrowed(&message.content);
    }
    let mut shown = String::new();
    for before in &message.history {
        for line in before.lines() {
            shown.push_str("> ");
            shown.push_str(line);
            shown.push('\n');
        }
    }
    shown.push_str(&message.content);
    std::borrow::Cow::Owned(shown)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn said(content: &str) -> Message {
        Message {
            content: content.into(),
            ..Message::default()
        }
    }

    #[test]
    fn keeps_what_the_switches_say() {
        set(Options::default());
        assert!(!keeps_deleted(&said("hi")));
        set(Options {
            deleted: true,
            edits: true,
            bots: false,
        });
        assert!(keeps_deleted(&said("hi")));
        let bot = Message {
            bot: true,
            ..said("beep")
        };
        assert!(!keeps_deleted(&bot));
        assert!(!keeps_deleted(&Message {
            deleted: true,
            ..said("again")
        }));
        let history = history_after_edit(&said("one"), &said("two"));
        assert_eq!(history, ["one"]);
        assert_eq!(history_after_edit(&said("same"), &said("same")), Vec::<String>::new());
        let mut old = said("v");
        old.history = (0..HISTORY).map(|n| n.to_string()).collect();
        let history = history_after_edit(&old, &said("w"));
        assert_eq!(history.len(), HISTORY);
        assert_eq!(history.last().map(String::as_str), Some("v"));
        assert_eq!(history[0], "1");
        set(Options::default());
        assert!(history_after_edit(&said("one"), &said("two")).is_empty());
    }

    #[test]
    fn shows_the_earlier_versions_quoted() {
        let mut message = said("final");
        assert_eq!(shown_content(&message), "final");
        message.history = vec!["first\nline".into(), "second".into()];
        message.deleted = true;
        assert_eq!(
            shown_content(&message),
            "> first\n> line\n> second\nfinal"
        );
    }
}
