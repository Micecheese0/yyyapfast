//! The message log window's list: what the `MsgLogData` folder holds of a
//! channel, or of every channel of a server, as deleted, edited or purged,
//! each with the pictures it saved.

use super::{commands::NOT_DISCORD, render::found_in, *};
use crate::chat::LogEntry;

/// One channel's part of a list.
struct Part {
    /// The channel's name, for a server's list; empty for one channel's.
    name: String,
    entries: Vec<yapfast_discord::msglog::Logged>,
    messages: Vec<Message>,
    found: Vec<ChatMessage>,
}

/// Carries out a message log command, or gives back any other.
pub(super) fn command(
    command: Command,
    discord: Result<&Discord, &str>,
    events: &UnboundedSender<Event>,
) -> Option<Command> {
    // One channel's log, by its room id; or a server's, by its id, which
    // lists its channels' with their names.
    let (key, places) = match command {
        Command::Discord(DiscordCommand::MessageLog { room }) => {
            let place = discord::parse_room(&room)
                .map(|(guild, channel)| (guild.to_owned(), channel.to_owned()));
            let places = place.map(|(guild, channel)| vec![(guild, channel, String::new())]);
            (room, places)
        }
        Command::Discord(DiscordCommand::ServerLog { server }) => {
            let places = match (&discord, discord::parse_server(&server)) {
                (Ok(discord), Some(guild)) => {
                    let snapshot = discord.snapshots().borrow().clone();
                    snapshot
                        .servers
                        .iter()
                        .find(|known| known.id == guild)
                        .map(|known| {
                            known
                                .server
                                .text_channels
                                .iter()
                                .chain(&known.server.voice_channels)
                                .map(|channel| {
                                    (guild.to_owned(), channel.id.clone(), channel.name.clone())
                                })
                                .collect()
                        })
                }
                _ => None,
            };
            (server, places)
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
                for (guild, channel, name) in places {
                    let id = channel.clone();
                    let entries = tokio::task::spawn_blocking(move || {
                        yapfast_discord::msglog::entries(&id)
                    })
                    .await
                    .unwrap_or_default();
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
                });
                for part in &mut parts {
                    saved_pictures(&discord, &part.messages, &mut part.found).await;
                }
                let _ = events.send(Event::MessageLog {
                    room: key,
                    result: Ok(list(&parts, &me)),
                });
            });
        }
        (discord, _) => {
            let _ = events.send(Event::MessageLog {
                room: key,
                result: Err(discord.err().unwrap_or(NOT_DISCORD).into()),
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
