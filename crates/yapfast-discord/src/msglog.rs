//! `MsgLogData`: a local database of messages and the pictures, GIFs and videos
//! attached to them, kept in a folder of Yapfast's data folder.
//!
//! Unlike the rest of what Yapfast keeps (see [`crate::cache`]), it is plain
//! files and is tied to no login: it isn't encrypted with the key in the
//! system's credential store and isn't deleted when an account is unlinked or
//! signs in again, so resetting a password can't take it away. Delete the
//! folder to clear it.
//!
//! ```text
//! MsgLogData/
//!   messages/{channel}.jsonl        one JSON line for each message, edit and deletion
//!   media/images/{channel}/...      attached pictures
//!   media/gifs/{channel}/...        attached GIFs
//!   media/videos/{channel}/...      attached videos
//! ```
//!
//! The session writes it as events arrive, for every channel the account
//! gets messages of, not only the open one: nothing here is held in memory.
//! [`logged`] folds a channel's lines into the messages that were deleted or
//! edited, which the channel shows again; see [`crate::logger`]. Each kind of
//! file has the largest size one may be, [`Options`].

use std::{
    collections::{BTreeMap, HashSet},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use serde::Deserialize;

use crate::{
    api::Api,
    message::{Attachment, Message, Shown},
};

const MB: u64 = 1024 * 1024;

static ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);
static MESSAGES: AtomicBool = AtomicBool::new(false);
static MEDIA: AtomicBool = AtomicBool::new(false);
static BOTS: AtomicBool = AtomicBool::new(false);
static LIMITS: [AtomicU64; 3] = [
    AtomicU64::new(25 * MB),
    AtomicU64::new(25 * MB),
    AtomicU64::new(100 * MB),
];
/// Downloads going at once, so a busy channel's files come one after another.
static TURNS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// A kind of file the log keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Image,
    Gif,
    Video,
}

impl Kind {
    fn index(self) -> usize {
        match self {
            Self::Image => 0,
            Self::Gif => 1,
            Self::Video => 2,
        }
    }

    fn folder(self) -> &'static str {
        match self {
            Self::Image => "images",
            Self::Gif => "gifs",
            Self::Video => "videos",
        }
    }

    /// The largest size of a file of this kind, in bytes; none for no limit.
    fn limit(self) -> Option<u64> {
        Some(LIMITS[self.index()].load(Ordering::Relaxed)).filter(|&limit| limit != u64::MAX)
    }
}

/// What the log keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// Messages, their edits and deletions are written.
    pub messages: bool,
    /// Attached pictures, GIFs and videos are downloaded too.
    pub media: bool,
    /// Bots' and webhooks' messages count.
    pub bots: bool,
    /// The largest picture, GIF and video, in MB; 0 for no limit.
    pub image_mb: u64,
    pub gif_mb: u64,
    pub video_mb: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            messages: false,
            media: false,
            bots: false,
            image_mb: 25,
            gif_mb: 25,
            video_mb: 100,
        }
    }
}

/// Sets what the log keeps.
pub fn set(options: Options) {
    MESSAGES.store(options.messages || options.media, Ordering::Relaxed);
    MEDIA.store(options.media, Ordering::Relaxed);
    BOTS.store(options.bots, Ordering::Relaxed);
    let bytes = |mb: u64| if mb == 0 { u64::MAX } else { mb.saturating_mul(MB) };
    LIMITS[0].store(bytes(options.image_mb), Ordering::Relaxed);
    LIMITS[1].store(bytes(options.gif_mb), Ordering::Relaxed);
    LIMITS[2].store(bytes(options.video_mb), Ordering::Relaxed);
}

/// Where the log is kept: the `MsgLogData` folder.
pub fn keep_in(root: PathBuf) {
    *ROOT.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(root);
}

fn root() -> Option<PathBuf> {
    ROOT.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// Whether messages are being written.
pub fn logging() -> bool {
    MESSAGES.load(Ordering::Relaxed) && root().is_some()
}

/// A channel's id is a number, which keeps it a plain file name.
fn channel_name(channel: &str) -> Option<&str> {
    (!channel.is_empty() && channel.bytes().all(|byte| byte.is_ascii_digit())).then_some(channel)
}

fn lines_file(channel: &str) -> Option<PathBuf> {
    Some(
        root()?
            .join("messages")
            .join(format!("{}.jsonl", channel_name(channel)?)),
    )
}

/// One line of a channel's file.
#[derive(Deserialize)]
#[serde(tag = "t")]
enum Entry {
    /// A message that came.
    #[serde(rename = "c")]
    Created { m: Message },
    /// A message whose words changed, as it is now.
    #[serde(rename = "e")]
    Edited { m: Message },
    /// A message that was deleted.
    #[serde(rename = "d")]
    Deleted { id: String },
}

/// What of a line tells which message it's of, without reading the rest.
#[derive(Deserialize)]
struct Light {
    t: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    m: Option<LightMessage>,
}

#[derive(Deserialize)]
struct LightMessage {
    id: String,
}

impl Light {
    fn message(&self) -> Option<&str> {
        self.id.as_deref().or(self.m.as_ref().map(|m| m.id.as_str()))
    }
}

fn append(channel: &str, lines: &[String]) {
    let Some(file) = lines_file(channel) else {
        return;
    };
    let written = (|| -> std::io::Result<()> {
        if let Some(folder) = file.parent() {
            std::fs::create_dir_all(folder)?;
        }
        let mut out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file)?;
        let mut text = String::new();
        for line in lines {
            text.push_str(line);
            text.push('\n');
        }
        out.write_all(text.as_bytes())
    })();
    if let Err(error) = written {
        tracing::warn!(%error, "could not write the message log");
    }
}

/// Logs a message that came, or whose words changed, and with the media
/// switch on, downloads its files. It runs apart from the session.
pub fn message(api: &Arc<Api>, message: Message, update: bool) {
    if !logging() || (message.bot && !BOTS.load(Ordering::Relaxed)) {
        return;
    }
    // An update that leaves the words be, as one that adds a link's embed, is
    // no edit.
    if update && !(message.edited && !message.content.is_empty()) {
        return;
    }
    let api = api.clone();
    tokio::spawn(async move {
        let line = serde_json::json!({"t": if update { "e" } else { "c" }, "m": &message});
        let channel = message.channel_id.clone();
        let _ = tokio::task::spawn_blocking(move || append(&channel, &[line.to_string()])).await;
        if !update && MEDIA.load(Ordering::Relaxed) {
            media(&api, &message).await;
        }
    });
}

/// Logs deleted messages of `channel`.
pub fn deleted(channel: &str, ids: Vec<String>) {
    if !logging() || ids.is_empty() {
        return;
    }
    let channel = channel.to_owned();
    tokio::spawn(async move {
        let lines: Vec<String> = ids
            .into_iter()
            .map(|id| serde_json::json!({"t": "d", "id": id}).to_string())
            .collect();
        let _ = tokio::task::spawn_blocking(move || append(&channel, &lines)).await;
    });
}

/// The kind of file an attachment is, when the log keeps it.
fn kind(attachment: &Attachment) -> Option<Kind> {
    if attachment.shown != Shown::Message {
        return None;
    }
    let name = attachment.name.to_ascii_lowercase();
    let extension = name.rsplit_once('.').map_or("", |(_, extension)| extension);
    if extension == "gif" {
        Some(Kind::Gif)
    } else if attachment.video_url.is_some()
        || matches!(extension, "mp4" | "webm" | "mov" | "mkv" | "m4v")
    {
        Some(Kind::Video)
    } else if attachment.image {
        Some(Kind::Image)
    } else {
        None
    }
}

/// A file name that is safe on Windows, Linux and macOS.
fn clean(name: &str) -> String {
    let name: String = name
        .chars()
        .map(|c| {
            if c.is_control() || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let name = name.trim_matches(|c: char| c == '.' || c == ' ');
    let name: String = name.chars().take(100).collect();
    if name.is_empty() { "file".into() } else { name }
}

/// Where an attachment of a message is kept.
fn media_file(root: &Path, kind: Kind, message: &Message, index: usize, name: &str) -> PathBuf {
    root.join("media")
        .join(kind.folder())
        .join(&message.channel_id)
        .join(format!("{}-{index}-{}", message.id, clean(name)))
}

async fn media(api: &Api, message: &Message) {
    let (Some(root), Some(_)) = (root(), channel_name(&message.channel_id)) else {
        return;
    };
    for (index, attachment) in message.attachments.iter().enumerate() {
        let Some(kind) = kind(attachment) else {
            continue;
        };
        let limit = kind.limit();
        if let (Some(limit), Some(size)) = (limit, attachment.size)
            && size > limit
        {
            continue;
        }
        let file = media_file(&root, kind, message, index, &attachment.name);
        if file.exists() {
            continue;
        }
        let Ok(_turn) = TURNS.acquire().await else {
            return;
        };
        let (target, partial) = (file.clone(), file.with_extension("partial"));
        let opened = {
            let partial = partial.clone();
            tokio::task::spawn_blocking(move || -> std::io::Result<std::fs::File> {
                if let Some(folder) = partial.parent() {
                    std::fs::create_dir_all(folder)?;
                }
                std::fs::File::create(&partial)
            })
            .await
        };
        let Ok(Ok(mut out)) = opened else {
            continue;
        };
        let result = api
            .download_into(&attachment.url, &mut out, limit.unwrap_or(u64::MAX))
            .await;
        drop(out);
        match result {
            Ok(_) => {
                let _ = std::fs::rename(&partial, &target);
            }
            Err(error) => {
                tracing::debug!(%error, "could not log an attachment");
                let _ = std::fs::remove_file(&partial);
            }
        }
    }
}

/// The messages of `channel` that were deleted, and those edited with what
/// they said before, oldest first, however many. It reads the disk.
pub fn logged(channel: &str) -> Vec<Message> {
    let Some(file) = lines_file(channel) else {
        return Vec::new();
    };
    let open = || std::fs::File::open(&file).map(BufReader::new);
    let Ok(first) = open() else {
        return Vec::new();
    };
    // Which messages were deleted or edited, so the rest aren't read in full.
    let mut wanted = HashSet::new();
    for line in first.lines().map_while(Result::ok) {
        if let Ok(light) = serde_json::from_str::<Light>(&line)
            && light.t != "c"
            && let Some(id) = light.message()
        {
            wanted.insert(id.to_owned());
        }
    }
    if wanted.is_empty() {
        return Vec::new();
    }
    let Ok(second) = open() else {
        return Vec::new();
    };
    let mut messages: BTreeMap<u64, Message> = BTreeMap::new();
    for line in second.lines().map_while(Result::ok) {
        let Ok(light) = serde_json::from_str::<Light>(&line) else {
            continue;
        };
        if !light.message().is_some_and(|id| wanted.contains(id)) {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Entry>(&line) else {
            continue;
        };
        match entry {
            Entry::Created { m } => {
                messages.entry(m.order()).or_insert(m);
            }
            Entry::Edited { m } => {
                let order = m.order();
                let mut history = Vec::new();
                let mut deleted = false;
                if let Some(old) = messages.get(&order) {
                    history = old.history.clone();
                    deleted = old.deleted;
                    if !old.content.is_empty() && old.content != m.content {
                        history.push(old.content.clone());
                        if history.len() > crate::logger::HISTORY {
                            history.remove(0);
                        }
                    }
                }
                messages.insert(
                    order,
                    Message {
                        history,
                        deleted,
                        ..m
                    },
                );
            }
            Entry::Deleted { id } => {
                if let Some(message) = id
                    .parse::<u64>()
                    .ok()
                    .and_then(|order| messages.get_mut(&order))
                {
                    message.deleted = true;
                }
            }
        }
    }
    messages
        .into_values()
        .filter(|message| message.deleted || !message.history.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn said(id: &str, content: &str, edited: bool) -> Message {
        Message {
            id: id.into(),
            channel_id: "7".into(),
            content: content.into(),
            edited,
            ..Message::default()
        }
    }

    #[test]
    fn folds_a_channels_lines_into_what_was_deleted_or_edited() {
        let root = std::env::temp_dir().join(format!("msglog-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        keep_in(root.clone());
        let line = |t: &str, message: &Message| serde_json::json!({"t": t, "m": message}).to_string();
        append(
            "7",
            &[
                line("c", &said("1", "one", false)),
                line("c", &said("2", "two", false)),
                line("c", &said("3", "three", false)),
                line("e", &said("2", "two!", true)),
                line("e", &said("2", "two!!", true)),
                serde_json::json!({"t": "d", "id": "3"}).to_string(),
                serde_json::json!({"t": "d", "id": "99"}).to_string(),
            ],
        );
        let logged = logged("7");
        let shown: Vec<_> = logged
            .iter()
            .map(|m| (m.id.as_str(), m.content.as_str(), m.deleted, m.history.len()))
            .collect();
        assert_eq!(shown, [("2", "two!!", false, 2), ("3", "three", true, 0)]);
        assert_eq!(logged[0].history, ["two", "two!"]);
        assert!(super::logged("8").is_empty());
        assert!(super::logged("../x").is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn names_files_safely_and_sorts_them_by_kind() {
        assert_eq!(clean("a/b:c?.png"), "a_b_c_.png");
        assert_eq!(clean("..."), "file");
        let attachment = |name: &str, image: bool| -> Attachment {
            serde_json::from_value(serde_json::json!({
                "name": name, "url": "https://cdn.discordapp.com/a", "image": image,
                "shown": "Message", "spoiler": false,
            }))
            .unwrap()
        };
        assert_eq!(kind(&attachment("a.GIF", true)), Some(Kind::Gif));
        assert_eq!(kind(&attachment("a.mp4", false)), Some(Kind::Video));
        assert_eq!(kind(&attachment("a.png", true)), Some(Kind::Image));
        assert_eq!(kind(&attachment("a.zip", false)), None);
    }
}
