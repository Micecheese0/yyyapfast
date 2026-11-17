//! The message log window's list: what the `MsgLogData` folder holds of a
//! channel as deleted, edited or purged, each with the pictures it saved.

use super::{commands::NOT_DISCORD, render::found_in, *};
use crate::chat::LogEntry;

/// Carries out a message log command, or gives back any other.
pub(super) fn command(
    command: Command,
    discord: Result<&Discord, &str>,
    events: &UnboundedSender<Event>,
) -> Option<Command> {
    let Command::Discord(DiscordCommand::MessageLog { room }) = command else {
        return Some(command);
    };
    let place =
        discord::parse_room(&room).map(|(guild, channel)| (guild.to_owned(), channel.to_owned()));
    match (discord, place) {
        (Ok(discord), Some((_, channel))) => {
            let (discord, events) = (discord.clone(), events.clone());
            tokio::spawn(async move {
                let entries = tokio::task::spawn_blocking(move || {
                    yapfast_discord::msglog::entries(&channel)
                })
                .await
                .unwrap_or_default();
                let me = discord
                    .snapshots()
                    .borrow()
                    .user_id
                    .clone()
                    .unwrap_or_default();
                let messages: Vec<Message> = entries
                    .iter()
                    .map(|logged| logged.message.clone())
                    .collect();
                // The list shows at once, its pictures as they come.
                let mut found = found_in(&discord, &room, &messages);
                let list = |found: &[ChatMessage]| -> Vec<LogEntry> {
                    // The latest first.
                    entries
                        .iter()
                        .zip(found)
                        .rev()
                        .map(|(logged, message)| LogEntry {
                            message: message.clone(),
                            deleted: logged.message.deleted,
                            edited: !logged.message.history.is_empty(),
                            bulk: logged.bulk,
                            ghost: logged.message.deleted
                                && !me.is_empty()
                                && (logged.message.mentions.contains(&me)
                                    || logged.message.mention_everyone),
                        })
                        .collect()
                };
                let _ = events.send(Event::MessageLog {
                    room: room.clone(),
                    result: Ok(list(&found)),
                });
                saved_pictures(&discord, &messages, &mut found).await;
                let _ = events.send(Event::MessageLog {
                    room,
                    result: Ok(list(&found)),
                });
            });
        }
        (discord, _) => {
            let _ = events.send(Event::MessageLog {
                room,
                result: Err(discord.err().unwrap_or(NOT_DISCORD).into()),
            });
        }
    }
    None
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
