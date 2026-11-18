//! Tenor's website as the GIF picker's source, in place of Discord's own
//! provider, which is Klipy now. Tenor's pages hold their GIFs as
//! `<figure class="UniversalGifListItem">` elements: each links its page,
//! `/view/…`, which a message sends and Discord shows as the GIF, and holds
//! the GIF as a small animated WebP, an MP4 and a `.gif`. The pages are read
//! as they are served, as the BringBackTenor plugin for BetterDiscord does,
//! because Tenor's API needs a key. Nothing here is sent to Discord, and the
//! requests carry no login: see [`crate::api::Api::tenor_page`].

use crate::gifs::{Category, Gif, provider_media};

/// Tenor's website.
pub const SITE: &str = "https://tenor.com";

/// The page of a search: `cat dance` is `/search/cat-dance-gifs`.
pub fn search_path(query: &str) -> String {
    let slug = query.split_whitespace().collect::<Vec<_>>().join("-");
    let slug: String = url::form_urlencoded::byte_serialize(slug.as_bytes()).collect();
    format!("/search/{slug}-gifs")
}

/// The text after the first `start` in `text`, up to the next `end`.
fn between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let from = text.find(start)? + start.len();
    let length = text[from..].find(end)?;
    Some(&text[from..from + length])
}

/// `text` as a page wrote it, with the entities an attribute or a name may hold
/// as the characters they stand for.
fn unescape(text: &str) -> String {
    text.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// The GIFs a page of results lists, in its order.
pub fn parse_gifs(html: &str) -> Vec<Gif> {
    let mut gifs: Vec<Gif> = Vec::new();
    for figure in html.split("<figure class=\"UniversalGifListItem").skip(1) {
        let figure = figure.split("</figure>").next().unwrap_or(figure);
        let Some(page) = between(figure, "href=\"/view/", "\"") else {
            continue;
        };
        let url = format!("{SITE}/view/{}", unescape(page));
        // The WebP is the smallest picture that animates; the GIF follows it.
        let gif_src = between(figure, "<img src=\"", "\"").map(unescape);
        let webp = between(figure, "type=\"image/webp\" srcset=\"", " ").map(unescape);
        let Some(src) = webp.clone().or_else(|| gif_src.clone()) else {
            continue;
        };
        if !provider_media(&src) || gifs.iter().any(|known| known.url == url) {
            continue;
        }
        let size = |name: &str| {
            between(figure, name, "\"")
                .and_then(|value| value.parse::<f32>().ok())
                .map_or(0, |value| value as u32)
        };
        gifs.push(Gif {
            id: None,
            url,
            src,
            gif_src: gif_src.filter(|src| provider_media(src)).unwrap_or_default(),
            width: size("data-width=\""),
            height: size("data-height=\""),
        });
    }
    gifs
}

/// The search terms the explore page lists, each with the GIF behind its tile.
pub fn parse_categories(html: &str) -> Vec<Category> {
    html.split("<a class=\"SearchTag\"")
        .skip(1)
        .filter_map(|tag| {
            let tag = tag.split("</a>").next().unwrap_or(tag);
            let src = unescape(between(tag, "background-image:url('", "'")?);
            let name = unescape(between(tag, "<span>", "</span>")?);
            (provider_media(&src) && !name.is_empty()).then_some(Category { name, src })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two results as Tenor's search page writes them, the second a repeat.
    const RESULTS: &str = r#"<div><figure class="UniversalGifListItem clickable" data-index="0" data-width="220" data-height="211" style="top:0px;"><a href="/view/cat-cat-dance-gif-7523542997650649169"><div class="Gif"><picture><source type="video/mp4" srcset="https://media.tenor.com/aGj-frNYMFEAAAP1/cat-cat-dance.mp4 320w"><source media="(max-width:840px)" type="image/webp" srcset="https://media.tenor.com/aGj-frNYMFEAAAA1/cat-cat-dance.webp 200w"><img src="https://media.tenor.com/aGj-frNYMFEAAAAM/cat-cat-dance.gif" width="180" height="172.6" alt="a kitten" loading="lazy"></picture></div></a><figcaption class="tags"></figcaption></figure><figure class="UniversalGifListItem clickable" data-index="1" data-width="100.5" data-height="50"><a href="/view/cat-cat-dance-gif-7523542997650649169"><img src="https://media.tenor.com/x/y.gif"></a></figure><figure class="UniversalGifListItem" data-width="10" data-height="10"><a href="/view/dog-gif-1"><img src="https://media.tenor.com/dogAAAAM/dog.gif"></a></figure>"#;

    #[test]
    fn reads_the_gifs_of_a_results_page() {
        let gifs = parse_gifs(RESULTS);
        assert_eq!(gifs.len(), 2, "a repeat of a GIF's page is left out");
        assert_eq!(
            gifs[0],
            Gif {
                id: None,
                url: "https://tenor.com/view/cat-cat-dance-gif-7523542997650649169".into(),
                src: "https://media.tenor.com/aGj-frNYMFEAAAA1/cat-cat-dance.webp".into(),
                gif_src: "https://media.tenor.com/aGj-frNYMFEAAAAM/cat-cat-dance.gif".into(),
                width: 220,
                height: 211,
            }
        );
        // Without a WebP, its picture is the GIF.
        assert_eq!(gifs[1].src, "https://media.tenor.com/dogAAAAM/dog.gif");
        assert!(parse_gifs("<html>no results</html>").is_empty());
        // A picture from anywhere but a GIF provider isn't one.
        assert!(
            parse_gifs(
                r#"<figure class="UniversalGifListItem"><a href="/view/a-1"><img src="https://example.com/a.gif"></a></figure>"#
            )
            .is_empty()
        );
    }

    #[test]
    fn reads_the_explore_pages_terms() {
        let page = r#"<a class="SearchTag" style="background-image:url('https://media.tenor.com/images/3a97/tenor.gif');" href="/search/movies-gifs"><span>Movies</span></a><a class="SearchTag" style="background-image:url('https://example.com/x.gif');" href="/search/x-gifs"><span>Elsewhere</span></a><a class="SearchTag" style="background-image:url('https://media.tenor.com/images/b2ab/tenor.gif');" href="/search/q-gifs"><span>Q &amp; A</span></a>"#;
        let terms = parse_categories(page);
        assert_eq!(
            terms,
            [
                Category {
                    name: "Movies".into(),
                    src: "https://media.tenor.com/images/3a97/tenor.gif".into()
                },
                Category {
                    name: "Q & A".into(),
                    src: "https://media.tenor.com/images/b2ab/tenor.gif".into()
                },
            ]
        );
    }

    #[test]
    fn a_search_is_a_slug() {
        assert_eq!(search_path("cat dance"), "/search/cat-dance-gifs");
        assert_eq!(search_path("  a  b "), "/search/a-b-gifs");
        assert_eq!(search_path("café/x"), "/search/caf%C3%A9%2Fx-gifs");
    }
}
