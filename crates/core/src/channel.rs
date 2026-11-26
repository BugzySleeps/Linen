//! Turning what you paste (a channel link, @handle, or video link) into a channel.

use anyhow::{Context, Result, bail};

use crate::Channel;

/// Find `key` in the page and return the text up to the next quote.
fn after<'a>(page: &'a str, key: &str) -> Option<&'a str> {
    let i = page.find(key)? + key.len();
    let rest = &page[i..];
    Some(&rest[..rest.find('"')?])
}

fn page_url(input: &str) -> String {
    let s = input.trim();
    if s.starts_with("http") {
        s.to_string()
    } else if let Some(h) = s.strip_prefix('@') {
        format!("https://www.youtube.com/@{h}")
    } else if s.starts_with("UC") && s.len() == 24 {
        format!("https://www.youtube.com/channel/{s}")
    } else {
        format!("https://www.youtube.com/@{s}")
    }
}

/// Look a channel up from a link or @handle (also works with a video link).
pub fn resolve(http: &reqwest::blocking::Client, input: &str) -> Result<Channel> {
    if input.trim().is_empty() {
        bail!("Paste a channel link or @handle");
    }
    let url = page_url(input);
    let page = http.get(&url).send()?.error_for_status().context("YouTube didn't find that channel")?.text()?;
    // channel pages carry their RSS link; video pages carry the uploader's id
    let id = after(&page, "feeds/videos.xml?channel_id=")
        .or_else(|| after(&page, "\"channelId\":\""))
        .or_else(|| after(&page, "\"externalId\":\""))
        .filter(|id| id.starts_with("UC"))
        .context("couldn't find a channel there")?
        .to_string();
    let name = after(&page, "<meta property=\"og:title\" content=\"")
        .map(|n| n.replace("&amp;", "&").replace("&#39;", "'").replace("&quot;", "\""))
        .filter(|n| !n.is_empty() && !url.contains("watch?"))
        .unwrap_or_default();
    let avatar = if url.contains("watch?") { None } else { after(&page, "<meta property=\"og:image\" content=\"").map(str::to_string) };
    Ok(Channel { id, name, avatar })
}

/// A channel's picture from its page (for imports, which only have id + name).
pub fn avatar(http: &reqwest::blocking::Client, id: &str) -> Option<String> {
    let page = http.get(format!("https://www.youtube.com/channel/{id}")).send().ok()?.text().ok()?;
    after(&page, "<meta property=\"og:image\" content=\"").map(str::to_string)
}

#[cfg(test)]
mod tests {
    /// Real network: `cargo test -p linen-core -- --ignored`
    #[test]
    #[ignore]
    fn live_resolve_and_feed() {
        let http = crate::http().unwrap();
        let c = super::resolve(&http, "@BlenderOfficial").unwrap();
        assert_eq!(c.id, "UCSMOQeBJ2RAnuFungnQOxLg");
        assert!(!c.name.is_empty() && c.avatar.is_some());
        let v = super::resolve(&http, "https://www.youtube.com/watch?v=aqz-KE-bpKQ").unwrap();
        assert_eq!(v.id, "UCSMOQeBJ2RAnuFungnQOxLg");
        let f = crate::feed::fetch(&http, &c.id).unwrap();
        assert!(!f.videos.is_empty());
        eprintln!("{} videos, newest: {}", f.videos.len(), f.videos[0].title);
    }
}
