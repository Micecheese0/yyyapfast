//! The message log window's list: what the `MsgLogData` folder holds of a
//! channel, or of every channel of a server, as deleted, edited or purged,
//! each with the pictures it saved.

use super::{commands::NOT_DISCORD, render::found_in, *};
use crate::chat::LogEntry;

/// One channel's part of a list.
struct Part {
    /// What heads the channel's messages, for a list of more than one
    /// channel's; empty for one channel's.
    name: String,
    entries: Vec<yapfast_discord::msglog::Logged>,
    messages: Vec<Message>,
    found: Vec<ChatMessage>,
}

/// A channel's heading: `#name`, with its server's before it when that's in
/// the list.
fn heading(server: &str, name: &str, id: &str) -> String {
    let name = if name.is_empty() { id } else { name };
    if server.is_empty() {
        format!("#{name}")
    } else {
        format!("{server} #{name}")
    }
}

/// Carries out a message log command, or gives back any other.
pub(super) fn command(
    command: Command,
    discord: Result<&Discord, &str>,
    events: &UnboundedSender<Event>,
) -> Option<Command> {
    // One channel's log, by its room id; a server's, by its id, which lists
    // its channels' by name, those deleted since too; or what was deleted, of
    // every server.
    let (key, places, most) = match command {
        Command::Discord(DiscordCommand::MessageLog { room, most }) => {
            let place = discord::parse_room(&room)
                .map(|(guild, channel)| (guild.to_owned(), channel.to_owned()));
            let places = place.map(|(guild, channel)| vec![(guild, channel, String::new())]);
            (room, places, most)
        }
        Command::Discord(DiscordCommand::ServerLog { server, most }) => {
            let places = match (&discord, discord::parse_server(&server)) {
                (Ok(discord), Some(guild)) => {
                    let snapshot = discord.snapshots().borrow().clone();
                    let mut places: Vec<(String, String, String)> = snapshot
                        .servers
                        .iter()
                        .find(|known| known.id == guild)
                        .map(|known| {
                            known
                                .server
                                .text_channels
                                .iter()
                                .chain(&known.server.voice_channels)
                                .chain(&known.server.threads)
                                .map(|channel| {
                                    (
                                        guild.to_owned(),
                                        channel.id.clone(),
                                        heading("", &channel.name, &channel.id),
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    // Channels the server lost, which no list of its has.
                    for place in yapfast_discord::msglog::places_of(guild) {
                        if !places.iter().any(|(_, id, _)| *id == place.channel) {
                            let label = heading("", &place.name, &place.channel);
                            places.push((place.guild, place.channel, label));
                        }
                    }
                    Some(places)
                }
                _ => None,
            };
            (server, places, most)
        }
        Command::Discord(DiscordCommand::DeletedLog { most }) => {
            let places = discord.is_ok().then(|| {
                yapfast_discord::msglog::gone_places()
                    .into_iter()
                    .map(|place| {
                        let label = heading(&place.guild_name, &place.name, &place.channel);
                        (place.guild, place.channel, label)
                    })
                    .collect()
            });
            (crate::chat::DELETED_LOG.to_owned(), places, most)
        }
        other => return Some(other),
    };
    match (discord, places) {
        (Ok(discord), Some(places)) => {
            let (discord, events) = (discord.clone(), events.clone());
            tokio::spawn(async move {
                let me = discord
                    .snapshots()
                    .borrow()
                    .user_id
                    .clone()
                    .unwrap_or_default();
                let mut parts = Vec::new();
                let mut more = false;
                for (guild, channel, name) in places {
                    let id = channel.clone();
                    // One more than `most`, to tell whether there are more.
                    let mut entries = tokio::task::spawn_blocking(move || {
                        yapfast_discord::msglog::entries_with_media(&id, most.saturating_add(1))
                    })
                    .await
                    .unwrap_or_default();
                    if entries.len() > most {
                        entries.remove(0);
                        more = true;
                    }
                    if entries.is_empty() {
                        continue;
                    }
                    let messages: Vec<Message> = entries
                        .iter()
                        .map(|logged| logged.message.clone())
                        .collect();
                    let found = found_in(&discord, &discord::room_id(&guild, &channel), &messages);
                    parts.push(Part {
                        name,
                        entries,
                        messages,
                        found,
                    });
                }
                // The list shows at once, its pictures as they come. The
                // window orders it, so it goes channel by channel.
                let _ = events.send(Event::MessageLog {
                    room: key.clone(),
                    result: Ok(list(&parts, &me)),
                    more,
                });
                for part in &mut parts {
                    saved_pictures(&discord, &part.messages, &mut part.found).await;
                }
                let _ = events.send(Event::MessageLog {
                    room: key,
                    result: Ok(list(&parts, &me)),
                    more,
                });
            });
        }
        (discord, _) => {
            let _ = events.send(Event::MessageLog {
                room: key,
                result: Err(discord.err().unwrap_or(NOT_DISCORD).into()),
                more: false,
            });
        }
    }
    None
}

/// The entries of every part, each channel's latest first.
fn list(parts: &[Part], me: &str) -> Vec<LogEntry> {
    parts
        .iter()
        .flat_map(|part| {
            part.entries
                .iter()
                .zip(&part.found)
                .rev()
                .map(|(logged, message)| LogEntry {
                    message: message.clone(),
                    channel: part.name.clone(),
                    deleted: logged.message.deleted,
                    edited: !logged.message.history.is_empty(),
                    bulk: logged.bulk,
                    media: logged.media,
                    ghost: logged.message.deleted
                        && !me.is_empty()
                        && (logged.message.mentions.iter().any(|id| id == me)
                            || logged.message.mention_everyone),
                })
        })
        .collect()
}

/// Gives `found` the first pictures of each message: from the file the log
/// saved of it when it has one, else from Discord, which may no longer have
/// it. One that can't be had is marked so.
async fn saved_pictures(discord: &Discord, messages: &[Message], found: &mut [ChatMessage]) {
    for (at, message) in messages.iter().enumerate() {
        let shown: Vec<usize> = message
            .attachments
            .iter()
            .enumerate()
            .filter(|(_, attachment)| attachment.shown == Shown::Message)
            .map(|(index, _)| index)
            .collect();
        for (slot, original) in shown
            .into_iter()
            .enumerate()
            .take(crate::chat::FOUND_PICTURES)
        {
            let Some(attachment) = found[at].attachments.get_mut(slot) else {
                continue;
            };
            if !(attachment.image || attachment.video) {
                continue;
            }
            if let Some(file) = yapfast_discord::msglog::local_media(message, original) {
                let picture = tokio::task::spawn_blocking(move || {
                    crate::chat::decode_picture_at(
                        &file,
                        crate::chat::PREVIEW_WIDTH,
                        crate::chat::PREVIEW_HEIGHT,
                    )
                })
                .await
                .ok()
                .flatten();
                if let Some(picture) = picture {
                    attachment.preview = Some(Arc::new(picture));
                    attachment.preview_failed = false;
                    continue;
                }
            }
            if let Some(url) = message.attachments[original].preview_url.clone() {
                let picture = super::render::preview(discord, &url).await;
                attachment.preview_failed = picture.is_none();
                attachment.preview = picture;
            }
        }
    }
}
