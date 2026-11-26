//! A channel's latest ~15 videos from https://www.youtube.com/feeds/videos.xml.

use anyhow::{Context, Result};

use crate::Video;

pub struct Feed {
    pub channel_name: String,
    pub videos: Vec<Video>,
}

fn unix(rfc3339: &str) -> i64 {
    // "2026-11-24T17:49:58+00:00" -> seconds; good enough without a date crate
    let p = |a: usize, b: usize| rfc3339.get(a..b).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    let (y, m, d, hh, mm, ss) = (p(0, 4), p(5, 7), p(8, 10), p(11, 13), p(14, 16), p(17, 19));
    // days from civil (Howard Hinnant's algorithm)
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let mut t = days * 86400 + hh * 3600 + mm * 60 + ss;
    // offset like +02:00 / -05:00
    if let Some(off) = rfc3339.get(19..).filter(|o| o.len() >= 6) {
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let oh = off.get(1..3).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
        let om = off.get(4..6).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
        t -= sign * (oh * 3600 + om * 60);
    }
    t
}

fn child<'a, 'i>(n: roxmltree::Node<'a, 'i>, name: &str) -> Option<roxmltree::Node<'a, 'i>> {
    n.children().find(|c| c.tag_name().name() == name)
}

fn text(n: Option<roxmltree::Node>) -> String {
    n.and_then(|n| n.text()).unwrap_or("").to_string()
}

/// Parse a feed. Shorts are left out (Linen is calm by default).
pub fn parse(xml: &str) -> Result<Feed> {
    let doc = roxmltree::Document::parse(xml).context("feed isn't valid XML")?;
    let root = doc.root_element();
    let channel_name = text(child(root, "title"));
    let mut videos = vec![];
    for e in root.children().filter(|c| c.tag_name().name() == "entry") {
        let link = child(e, "link").and_then(|l| l.attribute("href")).unwrap_or("");
        if link.contains("/shorts/") {
            continue;
        }
        let id = text(child(e, "videoId"));
        if id.is_empty() {
            continue;
        }
        let group = child(e, "group");
        let thumb = group
            .and_then(|g| child(g, "thumbnail"))
            .and_then(|t| t.attribute("url"))
            .map(str::to_string)
            .unwrap_or_else(|| format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg"));
        let views = group
            .and_then(|g| child(g, "community"))
            .and_then(|c| child(c, "statistics"))
            .and_then(|s| s.attribute("views"))
            .and_then(|v| v.parse().ok());
        videos.push(Video {
            channel_id: text(child(e, "channelId")),
            channel_name: channel_name.clone(),
            title: text(child(e, "title")),
            published: unix(&text(child(e, "published"))),
            thumbnail: thumb,
            views,
            description: group.map(|g| text(child(g, "description"))).unwrap_or_default(),
            seen: false,
            position: 0.0,
            duration: None,
            id,
        });
    }
    Ok(Feed { channel_name, videos })
}

pub fn fetch(http: &reqwest::blocking::Client, channel_id: &str) -> Result<Feed> {
    let url = format!("https://www.youtube.com/feeds/videos.xml?channel_id={channel_id}");
    let body = http.get(url).send()?.error_for_status()?.text()?;
    parse(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_skips_shorts() {
        let xml = r#"<?xml version="1.0"?>
<feed xmlns:yt="http://www.youtube.com/xml/schemas/2015" xmlns:media="http://search.yahoo.com/mrss/" xmlns="http://www.w3.org/2005/Atom">
 <title>Blender</title>
 <entry>
  <yt:videoId>abc</yt:videoId><yt:channelId>UCx</yt:channelId><title>Long one</title>
  <link rel="alternate" href="https://www.youtube.com/watch?v=abc"/>
  <published>2026-11-24T17:49:58+00:00</published>
  <media:group><media:thumbnail url="https://i1.ytimg.com/vi/abc/hqdefault.jpg"/>
   <media:description>hi</media:description>
   <media:community><media:statistics views="1234"/></media:community></media:group>
 </entry>
 <entry>
  <yt:videoId>sh</yt:videoId><yt:channelId>UCx</yt:channelId><title>A short</title>
  <link rel="alternate" href="https://www.youtube.com/shorts/sh"/>
  <published>2026-11-24T18:00:00+00:00</published>
 </entry>
</feed>"#;
        let f = parse(xml).unwrap();
        assert_eq!(f.channel_name, "Blender");
        assert_eq!(f.videos.len(), 1);
        let v = &f.videos[0];
        assert_eq!((v.id.as_str(), v.views, v.description.as_str()), ("abc", Some(1234), "hi"));
        assert_eq!(v.published, 1795542598);
    }

    #[test]
    fn time_offsets() {
        assert_eq!(unix("1970-01-01T00:00:00+00:00"), 0);
        assert_eq!(unix("1970-01-01T02:00:00+02:00"), 0);
    }
}
