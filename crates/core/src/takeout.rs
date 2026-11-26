//! Google Takeout's YouTube "subscriptions.csv": Channel Id,Channel Url,Channel Title

use crate::Channel;

pub fn parse_csv(text: &str) -> Vec<Channel> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let mut parts = line.splitn(3, ',');
            let id = parts.next()?.trim();
            let _url = parts.next()?;
            let name = parts.next().unwrap_or("").trim().trim_matches('"').replace("\"\"", "\"");
            id.starts_with("UC").then(|| Channel { id: id.to_string(), name, avatar: None })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn takeout() {
        let csv = "Channel Id,Channel Url,Channel Title\nUCSMOQeBJ2RAnuFungnQOxLg,http://www.youtube.com/channel/UCSMOQeBJ2RAnuFungnQOxLg,Blender\nUCabc,http://x,\"Name, with comma\"\n\n";
        let c = super::parse_csv(csv);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].name, "Blender");
        assert_eq!(c[1].name, "Name, with comma");
    }
}
