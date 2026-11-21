//! The open channel and those open lately, and who is typing in them.

use super::*;
use std::{collections::VecDeque, sync::Mutex};

/// Decoded previews shared by all recent channels. Active rows hold their
/// own handles; closing a channel leaves only this bounded cache owning pixels.
/// Each is kept with when it was last asked for.
#[derive(Default)]
pub(super) struct PreviewCache {
    pictures: VecDeque<(String, Arc<Picture>, Instant)>,
    bytes: usize,
    /// A task is waiting to let go of animated pictures nobody asks for
    /// ([`watch_idle`]).
    sweeping: bool,
}

pub(super) const PREVIEW_CACHE_BYTES: usize = 8 * 1024 * 1024;
pub(super) type SharedPreviews = Arc<Mutex<PreviewCache>>;

impl PreviewCache {
    pub(super) fn get(&mut self, url: &str) -> Option<Arc<Picture>> {
        let index = self.pictures.iter().position(|(key, ..)| key == url)?;
        let mut kept = self.pictures.remove(index).unwrap();
        kept.2 = Instant::now();
        let picture = kept.1.clone();
        self.pictures.push_back(kept);
        Some(picture)
    }

    pub(super) fn insert(&mut self, url: String, picture: Arc<Picture>) {
        if let Some(index) = self.pictures.iter().position(|(key, ..)| key == &url) {
            self.bytes -= self.pictures.remove(index).unwrap().1.rgba.len();
        }
        let bytes = picture.rgba.len();
        if bytes > PREVIEW_CACHE_BYTES {
            return;
        }
        while self.bytes + bytes > PREVIEW_CACHE_BYTES {
            self.bytes -= self.pictures.pop_front().unwrap().1.rgba.len();
        }
        self.bytes += bytes;
        self.pictures.push_back((url, picture, Instant::now()));
    }

    /// Lets go of the animated pictures nobody has asked for in `idle` as of
    /// `now`, and says how long until the next is that idle, if any is left.
    /// A still picture stays, as it is small and the cache's limit keeps it in
    /// check.
    fn let_go_idle(&mut self, idle: Duration, now: Instant) -> Option<Duration> {
        let mut bytes = self.bytes;
        let mut next: Option<Duration> = None;
        self.pictures.retain(|(_, picture, asked)| {
            if picture.animation.is_none() {
                return true;
            }
            let age = now.saturating_duration_since(*asked);
            if age >= idle {
                bytes -= picture.rgba.len();
                return false;
            }
            let left = idle - age;
            next = Some(next.map_or(left, |soonest| soonest.min(left)));
            true
        });
        self.bytes = bytes;
        next
    }
}

/// With the setting on ([`crate::chat::set_free_idle_gifs`]), lets go of each
/// animated picture in `previews` that nobody has asked for in
/// [`crate::chat::IDLE_GIF`]. Nothing runs while none is kept: a task starts
/// here, waits until the first is due, lets go, and waits again only for those
/// left. A call finding one already waiting does nothing.
pub(super) fn watch_idle(previews: &SharedPreviews) {
    watch_idle_after(previews, crate::chat::IDLE_GIF);
}

/// [`watch_idle`] for pictures idle for `idle`.
fn watch_idle_after(previews: &SharedPreviews, idle: Duration) {
    if !crate::chat::free_idle_gifs() {
        return;
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    {
        let mut cache = previews
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let animated = cache
            .pictures
            .iter()
            .any(|(_, picture, _)| picture.animation.is_some());
        if cache.sweeping || !animated {
            return;
        }
        cache.sweeping = true;
    }
    // The task holds no cache of its own: one nobody holds any more ends it.
    let weak = Arc::downgrade(previews);
    runtime.spawn(async move {
        let mut wait = idle;
        loop {
            tokio::time::sleep(wait).await;
            let Some(previews) = weak.upgrade() else {
                return;
            };
            let next = {
                let mut cache = previews
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let next = if crate::chat::free_idle_gifs() {
                    cache.let_go_idle(idle, Instant::now())
                } else {
                    None
                };
                if next.is_none() {
                    cache.sweeping = false;
                }
                next
            };
            match next {
                // A little past the due time, so it is due when it wakes.
                Some(left) => wait = left + Duration::from_millis(50),
                None => return,
            }
        }
    });
}

/// The open channel and those open lately, kept so going back to one shows
/// it at once. Closed channels follow live events too, and the latest page
/// loads again when one opens, for what they missed.
#[derive(Default)]
pub struct Channels {
    pub(super) open: Option<OpenChannel>,
    /// A thread shown beside the open channel, kept as live as it.
    pub(super) aside: Option<OpenChannel>,
    /// Newest last.
    pub(super) recent: Vec<OpenChannel>,
    pub(super) previews: SharedPreviews,
    /// Who is typing in each channel, by user id, with their name and until when.
    pub(super) typing: HashMap<String, HashMap<String, (String, Instant)>>,
    /// Where this account last told Discord it's typing, and when.
    pub(super) typed: Option<(String, Instant)>,
    /// Counts member searches, so one typed over before it went is dropped.
    pub(super) searches: Arc<std::sync::atomic::AtomicU64>,
}

/// How long a member search waits for more typing before it goes to Discord.
pub(super) const SEARCH_PAUSE: Duration = Duration::from_millis(300);

impl Channels {
    /// Forgets who stopped typing in `room`, and says who still is.
    pub(super) fn typing_changed(&mut self, room: &str, events: &UnboundedSender<Event>) {
        let now = Instant::now();
        let mut names = Vec::new();
        if let Some(typing) = self.typing.get_mut(room) {
            typing.retain(|_, (_, until)| *until > now);
            names = typing.values().map(|(name, _)| name.clone()).collect();
            names.sort();
            if typing.is_empty() {
                self.typing.remove(room);
            }
        }
        let _ = events.send(Event::Typing {
            room: room.to_owned(),
            names,
        });
    }

    pub(super) fn close(&mut self) {
        let channel = self.open.take();
        self.keep(channel);
    }

    /// Stops showing the thread beside the open channel.
    pub(super) fn close_aside(&mut self) {
        let channel = self.aside.take();
        self.keep(channel);
    }

    /// Keeps a channel that closed among the recent ones.
    fn keep(&mut self, channel: Option<OpenChannel>) {
        if let Some(mut channel) = channel {
            channel.shelve();
            self.recent.push(channel);
            if self.recent.len() > KEPT_CHANNELS {
                let mut gone = self.recent.remove(0);
                save(gone.take_changes());
            }
        }
    }

    /// Applies a live event to the closed channels. Each keeps only its
    /// newest messages, as on closing; a busy one would otherwise grow for
    /// the whole session.
    pub(super) fn follow(&mut self, event: &Live) {
        for kept in &mut self.recent {
            if kept.live(event) {
                kept.trim();
            }
        }
    }

    /// Live events went by without them, so every channel loads its latest
    /// page again when it next opens.
    pub(super) fn missed(&mut self) {
        for channel in self
            .open
            .iter_mut()
            .chain(&mut self.aside)
            .chain(&mut self.recent)
        {
            channel.missed();
        }
    }

    /// Puts the newest messages of the channels that changed on disk.
    pub(super) fn save(&mut self) {
        for channel in self
            .open
            .iter_mut()
            .chain(&mut self.aside)
            .chain(&mut self.recent)
        {
            save(channel.take_changes());
        }
    }

    pub(super) fn reopen(&mut self, room: &str) -> Option<OpenChannel> {
        let index = self
            .recent
            .iter()
            .position(|channel| channel.room == room)?;
        Some(self.recent.remove(index))
    }

    pub(super) fn get_mut(&mut self, room: &str) -> Option<&mut OpenChannel> {
        self.open
            .iter_mut()
            .chain(&mut self.aside)
            .chain(&mut self.recent)
            .find(|channel| channel.room == room)
    }

    /// The open channel, or the thread beside it, if it's `room`.
    pub(super) fn shown_mut(&mut self, room: &str) -> Option<&mut OpenChannel> {
        self.open
            .iter_mut()
            .chain(&mut self.aside)
            .find(|channel| channel.room == room)
    }

    /// Whether `room` is on screen: the open channel or the thread beside it.
    pub(super) fn is_open(&self, room: &str) -> bool {
        self.open
            .iter()
            .chain(&self.aside)
            .any(|channel| channel.room == room)
    }
}

/// Writes a channel's messages to disk, off the async threads.
pub(super) fn save(saved: Option<(String, Vec<Message>)>) {
    if let Some((channel, messages)) = saved {
        tokio::task::spawn_blocking(move || {
            yapfast_discord::cache::save_messages(&channel, messages);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(bytes: usize) -> Arc<Picture> {
        Arc::new(Picture {
            rgba: crate::chat::Pixels::new(0, 0, vec![0; bytes]),
            ..Default::default()
        })
    }

    #[test]
    fn previews_evict_by_bytes_and_recent_use_without_invalidating_visible_handles() {
        let mut cache = PreviewCache::default();
        let first = picture(PREVIEW_CACHE_BYTES / 2);
        let weak = Arc::downgrade(&first);
        cache.insert("first".into(), first.clone());
        cache.insert("second".into(), picture(PREVIEW_CACHE_BYTES / 2));
        assert!(Arc::ptr_eq(&first, &cache.get("first").unwrap()));
        cache.insert("third".into(), picture(PREVIEW_CACHE_BYTES / 2));
        assert!(
            cache.get("second").is_none(),
            "least recently used leaves first"
        );
        cache.insert("fourth".into(), picture(PREVIEW_CACHE_BYTES / 2));
        assert!(cache.get("first").is_none());
        assert!(
            weak.upgrade().is_some(),
            "a visible row still owns its handle"
        );
        drop(first);
        assert!(
            weak.upgrade().is_none(),
            "eviction releases the cache's ownership"
        );
        assert_eq!(cache.bytes, PREVIEW_CACHE_BYTES);
        cache.insert("third".into(), picture(64));
        assert_eq!(cache.bytes, PREVIEW_CACHE_BYTES / 2 + 64);
        cache.insert("oversized".into(), picture(PREVIEW_CACHE_BYTES + 1));
        assert!(cache.get("oversized").is_none());
        assert_eq!(cache.bytes, PREVIEW_CACHE_BYTES / 2 + 64);
    }

    fn moving(bytes: usize) -> Arc<Picture> {
        Arc::new(Picture {
            rgba: crate::chat::Pixels::new(0, 0, vec![0; bytes]),
            animation: Some(crate::chat::Animation::video(&[], None, vec![100])),
            ..Default::default()
        })
    }

    #[test]
    fn an_animation_nobody_asks_for_goes_after_the_idle_time_and_a_still_stays() {
        let idle = Duration::from_secs(30);
        let mut cache = PreviewCache::default();
        cache.insert("still".into(), picture(100));
        cache.insert("moving".into(), moving(1000));
        let start = Instant::now();
        let left = cache
            .let_go_idle(idle, start + Duration::from_secs(10))
            .expect("an animation is waiting");
        assert!(
            left > Duration::from_secs(19) && left <= Duration::from_secs(20),
            "it says when the animation is due: {left:?}"
        );
        assert_eq!(cache.bytes, 1100, "ten seconds in, nothing has gone");
        // Asked for again, it is idle from then: not yet at 31 seconds after
        // the first ask, by the clock the call is given.
        assert!(cache.get("moving").is_some());
        let left = cache.let_go_idle(idle, Instant::now() + Duration::from_secs(25));
        assert!(left.is_some(), "asked for lately, it stays");
        assert_eq!(cache.bytes, 1100);
        let left = cache.let_go_idle(idle, Instant::now() + Duration::from_secs(31));
        assert_eq!(left, None, "no animation is left to wait for");
        assert_eq!(cache.bytes, 100, "the animation's pixels went with it");
        assert!(cache.get("moving").is_none());
        assert!(cache.get("still").is_some(), "a still picture stays");
    }

    #[tokio::test]
    async fn the_watch_lets_go_of_an_idle_animation_only_while_the_setting_is_on() {
        let idle = Duration::from_millis(100);
        let previews = SharedPreviews::default();
        previews
            .lock()
            .unwrap()
            .insert("moving".into(), moving(1000));
        crate::chat::set_free_idle_gifs(false);
        watch_idle_after(&previews, idle);
        assert!(!previews.lock().unwrap().sweeping, "off, nothing waits");
        crate::chat::set_free_idle_gifs(true);
        watch_idle_after(&previews, idle);
        assert!(previews.lock().unwrap().sweeping, "on, a task waits");
        // A second call finds it waiting and starts no other.
        watch_idle_after(&previews, idle);
        tokio::time::sleep(Duration::from_millis(600)).await;
        crate::chat::set_free_idle_gifs(false);
        let cache = previews.lock().unwrap();
        assert_eq!(cache.bytes, 0, "idle for its time, the animation went");
        assert!(!cache.sweeping, "and with none left the task ended");
    }
}
