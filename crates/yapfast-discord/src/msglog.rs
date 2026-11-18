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

use serde::{Deserialize, Serialize};

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

static WHITELIST: AtomicBool = AtomicBool::new(false);
static LISTED: Mutex<Vec<String>> = Mutex::new(Vec::new());
static PLACES: Mutex<Places> = Mutex::new(Places {
    servers: BTreeMap::new(),
    channels: BTreeMap::new(),
});

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
    let rules = std::fs::read(root.join("rules.json"))
        .ok()
        .and_then(|data| serde_json::from_slice::<Rules>(&data).ok())
        .unwrap_or_default();
    WHITELIST.store(rules.whitelist, Ordering::Relaxed);
    *lock(&LISTED) = rules.listed;
    *lock(&PLACES) = std::fs::read(root.join("places.json"))
        .ok()
        .and_then(|data| serde_json::from_slice::<Places>(&data).ok())
        .unwrap_or_default();
    *lock(&ROOT) = Some(root);
}

/// Which server each logged channel is in, and what the servers and channels
/// were called, as `places.json` holds them: a channel's file names neither,
/// and a deleted channel or server is in no list of Discord's any more.
#[derive(Serialize, Deserialize, Default)]
struct Places {
    #[serde(default)]
    servers: BTreeMap<String, Named>,
    #[serde(default)]
    channels: BTreeMap<String, ChannelPlace>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct Named {
    name: String,
    /// The server is deleted, or this account left it.
    #[serde(default)]
    gone: bool,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct ChannelPlace {
    guild: String,
    name: String,
    /// The channel is deleted, or its server is gone.
    #[serde(default)]
    gone: bool,
}

/// A logged channel of a server, as the log's lists name it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    pub guild: String,
    pub guild_name: String,
    pub channel: String,
    pub name: String,
    /// The channel is deleted, or its server is.
    pub gone: bool,
}

fn save_places(places: &Places) {
    let Some(root) = root() else {
        return;
    };
    let written = std::fs::create_dir_all(&root)
        .and_then(|()| std::fs::write(root.join("places.json"), serde_json::to_vec(places)?));
    if let Err(error) = written {
        tracing::warn!(%error, "could not write the message log's places");
    }
}

/// Notes that `channel` is in `guild`, with the names `names` gives, unless
/// the index has it already. Only a channel and a server whose messages are
/// logged are noted, a few of them; `names` isn't called for the rest.
pub fn place(guild: &str, channel: &str, names: impl FnOnce() -> Option<(String, String)>) {
    if !logging()
        || lock(&PLACES)
            .channels
            .get(channel)
            .is_some_and(|known| known.guild == guild && !known.gone)
    {
        return;
    }
    let Some((server, name)) = names() else {
        return;
    };
    let mut places = lock(&PLACES);
    places.servers.insert(
        guild.to_owned(),
        Named {
            name: server,
            gone: false,
        },
    );
    places.channels.insert(
        channel.to_owned(),
        ChannelPlace {
            guild: guild.to_owned(),
            name,
            gone: false,
        },
    );
    save_places(&places);
}

/// A channel of the index was renamed.
pub fn rename_channel(channel: &str, name: &str) {
    let mut places = lock(&PLACES);
    if let Some(known) = places.channels.get_mut(channel)
        && known.name != name
    {
        known.name = name.to_owned();
        save_places(&places);
    }
}

/// A server of the index was renamed.
pub fn rename_server(guild: &str, name: &str) {
    let mut places = lock(&PLACES);
    if let Some(known) = places.servers.get_mut(guild)
        && known.name != name
    {
        known.name = name.to_owned();
        save_places(&places);
    }
}

/// Channels and threads of `guild`, given by id and name, are deleted, or the
/// server `server` names is: what the log holds of each shows as deleted from
/// now. Those the index has of the server go with it.
pub fn gone(guild: &str, server: Option<String>, channels: Vec<(String, String)>) {
    if !logging() {
        return;
    }
    let guild = guild.to_owned();
    // A blocking task: it reads and writes files.
    tokio::task::spawn_blocking(move || mark_gone(&guild, server, channels));
}

fn mark_gone(guild: &str, server: Option<String>, mut channels: Vec<(String, String)>) {
    let mut places = lock(&PLACES);
    if server.is_some() {
        for (id, known) in &places.channels {
            if known.guild == guild && !channels.iter().any(|(channel, _)| channel == id) {
                channels.push((id.clone(), known.name.clone()));
            }
        }
    }
    let mut changed = false;
    for (id, name) in channels {
        // Only a channel whose messages were logged, once.
        let logged = lines_file(&id).is_some_and(|file| file.is_file());
        if !logged || places.channels.get(&id).is_some_and(|known| known.gone) {
            continue;
        }
        append(&id, &[r#"{"t":"x"}"#.to_owned()]);
        let known = places.channels.entry(id).or_insert_with(|| ChannelPlace {
            guild: guild.to_owned(),
            name,
            gone: false,
        });
        known.gone = true;
        changed = true;
    }
    if let Some(name) = server {
        let known = places.servers.entry(guild.to_owned()).or_default();
        if !name.is_empty() {
            known.name = name;
        }
        known.gone = true;
        changed = true;
    }
    if changed {
        save_places(&places);
    }
}

fn placed(places: &Places, id: &str, known: &ChannelPlace) -> Place {
    Place {
        guild: known.guild.clone(),
        guild_name: places
            .servers
            .get(&known.guild)
            .map(|server| server.name.clone())
            .unwrap_or_default(),
        channel: id.to_owned(),
        name: known.name.clone(),
        gone: known.gone || places.servers.get(&known.guild).is_some_and(|server| server.gone),
    }
}

/// The logged channels of server `guild` the index knows of.
pub fn places_of(guild: &str) -> Vec<Place> {
    let places = lock(&PLACES);
    places
        .channels
        .iter()
        .filter(|(_, known)| known.guild == guild)
        .map(|(id, known)| placed(&places, id, known))
        .collect()
}

/// The channels that were deleted, or whose server was, of every server.
pub fn gone_places() -> Vec<Place> {
    let places = lock(&PLACES);
    places
        .channels
        .iter()
        .map(|(id, known)| placed(&places, id, known))
        .filter(|place| place.gone)
        .collect()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Which servers and channels the log keeps, as `rules.json` holds them: all
/// but the ones listed, or only the ones listed.
#[derive(serde::Serialize, Deserialize, Default)]
struct Rules {
    #[serde(default)]
    whitelist: bool,
    /// Ids of servers and channels.
    #[serde(default)]
    listed: Vec<String>,
}

/// Whether the log keeps only the servers and channels listed, rather than all
/// but those.
pub fn whitelist() -> bool {
    WHITELIST.load(Ordering::Relaxed)
}

/// The ids of the servers and channels listed.
pub fn listed() -> Vec<String> {
    lock(&LISTED).clone()
}

fn save_rules() {
    let Some(root) = root() else {
        return;
    };
    let rules = Rules {
        whitelist: whitelist(),
        listed: listed(),
    };
    let written = std::fs::create_dir_all(&root)
        .and_then(|()| std::fs::write(root.join("rules.json"), serde_json::to_vec(&rules)?));
    if let Err(error) = written {
        tracing::warn!(%error, "could not write the message log's rules");
    }
}

/// Switches between keeping all but the listed servers and channels, and only
/// those.
pub fn set_whitelist(on: bool) {
    WHITELIST.store(on, Ordering::Relaxed);
    save_rules();
}

/// Adds the server or channel `id` to the list, or takes it off when it's on;
/// returns whether it's on the list now.
pub fn toggle_listed(id: &str) -> bool {
    let on = {
        let mut listed = lock(&LISTED);
        match listed.iter().position(|known| known == id) {
            Some(at) => {
                listed.remove(at);
                false
            }
            None => {
                listed.push(id.to_owned());
                true
            }
        }
    };
    save_rules();
    on
}

/// Whether the log keeps messages of `channel`, in `guild` if it's in one.
pub fn wants(guild: Option<&str>, channel: &str) -> bool {
    let listed = {
        let listed = lock(&LISTED);
        listed
            .iter()
            .any(|id| id == channel || Some(id.as_str()) == guild)
    };
    listed == whitelist()
}

fn root() -> Option<PathBuf> {
    lock(&ROOT).clone()
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
    /// A message that was deleted, in a purge when `bulk`.
    #[serde(rename = "d")]
    Deleted {
        id: String,
        #[serde(default, rename = "b")]
        bulk: bool,
    },
    /// The channel was deleted, or its server: every message before this line
    /// is gone with it.
    #[serde(rename = "x")]
    Gone {},
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
pub fn message(api: &Arc<Api>, guild: Option<&str>, message: Message, update: bool) {
    if !logging()
        || (message.bot && !BOTS.load(Ordering::Relaxed))
        || !wants(guild, &message.channel_id)
    {
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

/// Logs deleted messages of `channel`, in `guild` if it's in one, `bulk` for
/// a purge.
pub fn deleted(guild: Option<&str>, channel: &str, ids: Vec<String>, bulk: bool) {
    if !logging() || ids.is_empty() || !wants(guild, channel) {
        return;
    }
    let channel = channel.to_owned();
    tokio::spawn(async move {
        let lines: Vec<String> = ids
            .into_iter()
            .map(|id| serde_json::json!({"t": "d", "id": id, "b": bulk}).to_string())
            .collect();
        let _ = tokio::task::spawn_blocking(move || append(&channel, &lines)).await;
    });
}

/// Logs messages that came in a page of history, which the log has no line
/// for when they were posted while Yapfast was closed or before it logged, so
/// a deletion that comes later finds what they said. Those it has a line for
/// are left. No files are saved for them: the log's pictures are those it
/// downloaded as the messages arrived. It reads the channel's file and runs
/// on a thread of its own.
pub fn backfill(guild: Option<&str>, channel: &str, messages: Vec<Message>) {
    log_history(guild, channel, messages, false);
}

/// Logs messages Discord's latest page lacks, which were deleted while
/// Yapfast wasn't following the channel: what they said, if the log hasn't
/// it, and their deletion. Otherwise as [`backfill`].
pub fn lost(guild: Option<&str>, channel: &str, messages: Vec<Message>) {
    log_history(guild, channel, messages, true);
}

fn log_history(guild: Option<&str>, channel: &str, messages: Vec<Message>, deleted: bool) {
    if !logging() || messages.is_empty() || !wants(guild, channel) {
        return;
    }
    let channel = channel.to_owned();
    let started = std::thread::Builder::new()
        .name("msglog-history".into())
        .spawn(move || log_history_now(&channel, messages, deleted));
    if let Err(error) = started {
        tracing::warn!(%error, "could not start the message log's thread");
    }
}

fn log_history_now(channel: &str, messages: Vec<Message>, deleted: bool) {
    let bots = BOTS.load(Ordering::Relaxed);
    let known = logged_ids(channel);
    let mut lines = Vec::new();
    for message in messages {
        if message.bot && !bots {
            continue;
        }
        if !message
            .id
            .parse::<u64>()
            .is_ok_and(|order| known.contains(&order))
        {
            lines.push(serde_json::json!({"t": "c", "m": &message}).to_string());
        }
        if deleted {
            lines.push(serde_json::json!({"t": "d", "id": message.id, "b": false}).to_string());
        }
    }
    if !lines.is_empty() {
        append(channel, &lines);
    }
}

/// The messages of `channel` the log has a line of what they said for, a
/// message that came or was edited: read as the file goes, only their ids
/// held.
fn logged_ids(channel: &str) -> HashSet<u64> {
    let Some(file) = lines_file(channel) else {
        return HashSet::new();
    };
    let Ok(file) = std::fs::File::open(file) else {
        return HashSet::new();
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<Light>(&line).ok())
        .filter(|light| light.t == "c" || light.t == "e")
        .filter_map(|light| light.message()?.parse::<u64>().ok())
        .collect()
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

/// A message the log holds as deleted, or edited with what it said before, or
/// with files saved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Logged {
    pub message: Message,
    /// It went in a purge.
    pub bulk: bool,
    /// The log saved a picture, GIF or video of it.
    pub media: bool,
}

/// The messages of `channel` that were deleted, and those edited with what
/// they said before, oldest first, however many. It reads the disk.
pub fn logged(channel: &str) -> Vec<Message> {
    entries(channel)
        .into_iter()
        .map(|logged| logged.message)
        .collect()
}

/// [`logged`], with how each went.
pub fn entries(channel: &str) -> Vec<Logged> {
    entries_newest(channel, usize::MAX)
}

/// [`entries`], the newest `most` of them: a deleted channel has all its
/// messages to show.
pub fn entries_newest(channel: &str, most: usize) -> Vec<Logged> {
    read(channel, most, false)
}

/// [`entries_newest`], and the messages that have files saved, too, which the
/// log window's Media tab lists.
pub fn entries_with_media(channel: &str, most: usize) -> Vec<Logged> {
    read(channel, most, true)
}

/// The ids of the messages of `channel` the log saved files of: each file is
/// named for its message.
fn saved_media(channel: &str) -> HashSet<String> {
    let (Some(root), Some(channel)) = (root(), channel_name(channel)) else {
        return HashSet::new();
    };
    [Kind::Image, Kind::Gif, Kind::Video]
        .into_iter()
        .filter_map(|kind| {
            std::fs::read_dir(root.join("media").join(kind.folder()).join(channel)).ok()
        })
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|file| {
            let name = file.file_name();
            let id = name.to_string_lossy().split('-').next()?.to_owned();
            (!id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit())).then_some(id)
        })
        .collect()
}

fn read(channel: &str, most: usize, media: bool) -> Vec<Logged> {
    let Some(file) = lines_file(channel) else {
        return Vec::new();
    };
    let open = || std::fs::File::open(&file).map(BufReader::new);
    let Ok(first) = open() else {
        return Vec::new();
    };
    // Which messages were deleted or edited, so the rest aren't read in full;
    // all of them when the channel was deleted.
    let saved = if media {
        saved_media(channel)
    } else {
        HashSet::new()
    };
    let mut wanted = saved.clone();
    let mut all = false;
    for line in first.lines().map_while(Result::ok) {
        if let Ok(light) = serde_json::from_str::<Light>(&line) {
            if light.t == "x" {
                all = true;
            } else if light.t != "c"
                && let Some(id) = light.message()
            {
                wanted.insert(id.to_owned());
            }
        }
    }
    if wanted.is_empty() && !all {
        return Vec::new();
    }
    let Ok(second) = open() else {
        return Vec::new();
    };
    let mut messages: BTreeMap<u64, Message> = BTreeMap::new();
    let mut purged: HashSet<u64> = HashSet::new();
    for line in second.lines().map_while(Result::ok) {
        let Ok(light) = serde_json::from_str::<Light>(&line) else {
            continue;
        };
        if !all && !light.message().is_some_and(|id| wanted.contains(id)) {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Entry>(&line) else {
            continue;
        };
        match entry {
            Entry::Created { m } => {
                messages.entry(m.order()).or_insert(m);
                if messages.len() > most {
                    messages.pop_first();
                }
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
                if messages.len() > most {
                    messages.pop_first();
                }
            }
            Entry::Gone {} => {
                for message in messages.values_mut() {
                    message.deleted = true;
                }
            }
            Entry::Deleted { id, bulk } => {
                let order = id.parse::<u64>().ok();
                if let Some(message) = order.and_then(|order| messages.get_mut(&order)) {
                    message.deleted = true;
                    if let (true, Some(order)) = (bulk, order) {
                        purged.insert(order);
                    }
                }
            }
        }
    }
    messages
        .into_iter()
        .filter(|(_, message)| {
            message.deleted || !message.history.is_empty() || saved.contains(&message.id)
        })
        .map(|(order, message)| Logged {
            bulk: purged.contains(&order),
            media: saved.contains(&message.id),
            message,
        })
        .collect()
}

/// Where the file of the attachment at `index` of `message` is kept, if the
/// log has it.
pub fn local_media(message: &Message, index: usize) -> Option<PathBuf> {
    let attachment = message.attachments.get(index)?;
    let file = media_file(&root()?, kind(attachment)?, message, index, &attachment.name);
    file.is_file().then_some(file)
}

/// [`local_media`]'s file as an address, which the timeline shows a deleted
/// message's attachment from, and opens it by.
pub fn local_url(message: &Message, index: usize) -> Option<String> {
    url::Url::from_file_path(local_media(message, index)?)
        .ok()
        .map(Into::into)
}

/// The file in the log's folder that `address`, a [`local_url`], names. No
/// other file is named: an address from anywhere else, or one that leaves the
/// folder, gives none.
pub fn local_path(address: &str) -> Option<PathBuf> {
    let path = url::Url::parse(address).ok()?.to_file_path().ok()?;
    let inside = path.starts_with(root()?)
        && !path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir));
    inside.then_some(path)
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

    /// A channel's lines, each as its kind and its message's id.
    fn lines_of(channel: &str) -> Vec<String> {
        std::fs::read_to_string(lines_file(channel).unwrap())
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<Light>(line).ok())
            .map(|light| format!("{}{}", light.t, light.message().unwrap_or_default()))
            .collect()
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
                serde_json::json!({"t": "d", "id": "3", "b": true}).to_string(),
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
        let entries = super::entries("7");
        assert_eq!(entries.iter().map(|e| e.bulk).collect::<Vec<_>>(), [false, true]);
        // Only the servers and channels the rules let through.
        set_whitelist(false);
        assert!(wants(Some("1"), "7"));
        assert!(toggle_listed("7"));
        assert!(!wants(Some("1"), "7"));
        assert!(wants(Some("1"), "8"));
        set_whitelist(true);
        assert!(wants(Some("1"), "7"));
        assert!(!wants(Some("1"), "8"));
        assert!(wants(Some("7"), "8"));
        assert!(!toggle_listed("7"));
        assert!(!wants(Some("1"), "7"));
        assert!(super::logged("8").is_empty());
        assert!(super::logged("../x").is_empty());
        // A deleted channel, or server, takes all its messages with it, those
        // that came after it was back excepted.
        set_whitelist(false);
        set(Options {
            messages: true,
            ..Options::default()
        });
        append(
            "9",
            &[
                line("c", &said("1", "one", false)),
                line("c", &said("2", "two", false)),
            ],
        );
        assert!(super::logged("9").is_empty());
        place("5", "9", || Some(("Guild".into(), "old".into())));
        assert_eq!(places_of("5").len(), 1);
        assert!(gone_places().is_empty());
        mark_gone("5", Some("Guild".into()), vec![("9".into(), "old".into())]);
        assert_eq!(super::logged("9").len(), 2);
        assert!(super::logged("9").iter().all(|m| m.deleted));
        assert_eq!(super::entries_newest("9", 1).len(), 1);
        let gone = gone_places();
        assert_eq!(gone.len(), 1);
        assert_eq!(
            (gone[0].guild_name.as_str(), gone[0].name.as_str()),
            ("Guild", "old")
        );
        // Marked once, and a message after it isn't gone.
        mark_gone("5", None, vec![("9".into(), "old".into())]);
        append("9", &[line("c", &said("3", "three", false))]);
        let after: Vec<_> = super::logged("9").iter().map(|m| m.id.clone()).collect();
        assert_eq!(after, ["1", "2"]);
        // A page of history logs what the log lacks, once; one the log has
        // is left, and a message the page lacked is logged deleted.
        let history = |id: &str| said(id, &format!("history {id}"), false);
        log_history_now("12", vec![history("1"), history("2")], false);
        log_history_now("12", vec![history("2"), history("3")], false);
        assert_eq!(lines_of("12"), ["c1", "c2", "c3"]);
        log_history_now("12", vec![history("2"), history("4")], true);
        assert_eq!(lines_of("12")[3..], ["d2", "c4", "d4"]);
        // A message with a file saved is listed with the media, deleted or not.
        std::fs::create_dir_all(root.join("media").join("images").join("9")).unwrap();
        std::fs::write(root.join("media").join("images").join("9").join("3-0-x.png"), b"x")
            .unwrap();
        let with: Vec<_> = entries_with_media("9", 10)
            .iter()
            .map(|e| (e.message.id.clone(), e.media))
            .collect();
        assert_eq!(
            with,
            [("1".into(), false), ("2".into(), false), ("3".into(), true)]
        );
        assert_eq!(super::entries("9").len(), 2);
        // The address of a saved file names that file, and only files here.
        std::fs::create_dir_all(root.join("media")).unwrap();
        let file = root.join("media").join("a b#c.png");
        std::fs::write(&file, b"x").unwrap();
        let address = url::Url::from_file_path(&file).unwrap().to_string();
        assert_eq!(local_path(&address), Some(file));
        let outside = std::env::temp_dir().join("msglog-outside.png");
        let address = url::Url::from_file_path(&outside).unwrap().to_string();
        assert_eq!(local_path(&address), None);
        assert_eq!(local_path("https://cdn.discordapp.com/a.png"), None);
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
