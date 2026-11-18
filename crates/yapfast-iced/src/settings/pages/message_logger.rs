//! Message Logger: what the chat keeps of deleted and edited messages, and
//! the `MsgLogData` folder that saves messages and their pictures, GIFs and
//! videos for the servers and channels listed.

use iced::{Element, widget::text};

use super::{Pref, pref};
use crate::settings::{Event, Page};
use crate::{
    app::Message,
    controls::{Kind, action, choices},
    form::{self},
    pictures::Icon,
};

/// The sizes a kind of file may be, as Settings chooses them.
const SIZES: [&str; 5] = ["5 MB", "25 MB", "100 MB", "500 MB", "No limit"];

impl<'a> Page<'a> {
    pub(in crate::settings) fn message_logger_page(self) -> Element<'a, Message> {
        let theme = self.cx.look.theme;
        let prefs = &self.cx.layout.prefs;
        let ink = crate::theme::rgb(theme.text);
        let deleted_log = action(
            theme,
            "Open logs of deleted servers and channels",
            Kind::Plain,
            Some(Message::Settings(Event::OpenDeletedLog)),
        )
        .icon(self.cx.look.icon(Icon::History, 18, ink));
        form::page([
            form::title(theme, "Message Logger"),
            form::heading(theme, "IN THE CHAT"),
            self.flip(
                "Keep deleted messages",
                "A message someone deletes, or a purge removes, stays in the chat, tinted red. A deleted message that mentioned you stays highlighted.",
                prefs.keep_deleted,
                Pref::KeepDeleted,
            ),
            self.flip(
                "Keep edit history",
                "A message that is edited shows what it said before, quoted above it, up to the last 10 versions.",
                prefs.keep_edits,
                Pref::KeepEdits,
            ),
            self.flip(
                "Include bots' messages",
                "Also logs messages of bots and webhooks, which are left out otherwise.",
                prefs.log_bots,
                Pref::LogBots,
            ),
            form::heading(theme, "MSGLOGDATA FOLDER"),
            self.flip(
                "Save messages and media",
                "Writes every message you get, and its pictures, GIFs and videos, to the MsgLogData folder in Yapfast's data folder, whichever channel it is in. The files are not encrypted and no login owns them: signing out or resetting your password leaves them be, and only you delete the folder.",
                prefs.archive,
                Pref::Archive,
            ),
            form::heading(theme, "WHICH SERVERS AND CHANNELS"),
            text("Right-click a server or channel and choose Message log to list it.")
                .size(13)
                .color(theme.muted)
                .into(),
            choices(
                theme,
                &["Log all but listed", "Log only listed"],
                usize::from(self.cx.state.log_whitelist),
                |index| pref(Pref::LogMode(index)),
            ),
            form::heading(theme, "DELETED SERVERS AND CHANNELS"),
            text("What the log holds of a channel or server that was deleted, or that you left, shows as deleted. A server's logs also list its deleted channels.")
                .size(13)
                .color(theme.muted)
                .into(),
            form::beside(text(""), deleted_log),
            form::heading(theme, "LARGEST FILE TO SAVE"),
            text("Pictures").size(14).color(theme.secondary).into(),
            choices(
                theme,
                &SIZES,
                prefs.log_image_size.unwrap_or(1).clamp(0, 4) as usize,
                |index| pref(Pref::LogImageSize(index)),
            ),
            text("GIFs").size(14).color(theme.secondary).into(),
            choices(
                theme,
                &SIZES,
                prefs.log_gif_size.unwrap_or(1).clamp(0, 4) as usize,
                |index| pref(Pref::LogGifSize(index)),
            ),
            text("Videos").size(14).color(theme.secondary).into(),
            choices(
                theme,
                &SIZES,
                prefs.log_video_size.unwrap_or(2).clamp(0, 4) as usize,
                |index| pref(Pref::LogVideoSize(index)),
            ),
        ])
    }
}
