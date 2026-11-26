//! Just enough XML for EC2 Query API responses: elements by tag name,
//! nesting-aware (EC2 nests `<item>` in `<item>`), and entity unescaping.

/// The start of the next `<tag>`, `<tag ...>` or `<tag/>` at or after `from`.
fn open_at(xml: &str, tag: &str, from: usize) -> Option<(usize, usize, bool)> {
    let mut i = from;
    while let Some(off) = xml[i..].find('<') {
        let at = i + off;
        let rest = &xml[at + 1..];
        if let Some(after) = rest.strip_prefix(tag) {
            match after.chars().next() {
                Some('>') => return Some((at, at + 1 + tag.len() + 1, false)),
                Some(' ') | Some('\t') | Some('\n') | Some('/') => {
                    let end = at + 1 + rest.find('>')? + 1;
                    let self_closing = xml[..end].ends_with("/>");
                    return Some((at, end, self_closing));
                }
                _ => {}
            }
        }
        i = at + 1;
    }
    None
}

/// The outermost `<tag>` elements of `xml`: (element start, content start,
/// content end, element end).
fn spans(xml: &str, tag: &str) -> Vec<(usize, usize, usize, usize)> {
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut i = 0;
    while let Some((start, content_start, self_closing)) = open_at(xml, tag, i) {
        if self_closing {
            out.push((start, content_start, content_start, content_start));
            i = content_start;
            continue;
        }
        // The matching close, counting nested opens of the same tag.
        let mut depth = 1;
        let mut j = content_start;
        let content_end = loop {
            let Some(next_close) = xml[j..].find(&close).map(|o| j + o) else {
                return out; // unbalanced: stop
            };
            match open_at(xml, tag, j) {
                Some((o, oe, sc)) if o < next_close => {
                    if !sc {
                        depth += 1;
                    }
                    j = oe;
                }
                _ => {
                    depth -= 1;
                    if depth == 0 {
                        break next_close;
                    }
                    j = next_close + close.len();
                }
            }
        };
        let end = content_end + close.len();
        out.push((start, content_start, content_end, end));
        i = end;
    }
    out
}

/// The outermost `<tag>` elements of `xml`, as their inner text.
pub fn elements<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    spans(xml, tag)
        .into_iter()
        .map(|(_, a, b, _)| &xml[a..b])
        .collect()
}

/// The first `<tag>` element's inner text, raw.
pub fn inner<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    elements(xml, tag).into_iter().next()
}

/// The first `<tag>` element's text, unescaped and trimmed.
pub fn text(xml: &str, tag: &str) -> Option<String> {
    inner(xml, tag).map(|s| unescape(s.trim()))
}

/// `xml` without its outermost `<tag>` elements.
pub fn without(xml: &str, tag: &str) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut at = 0;
    for (start, _, _, end) in spans(xml, tag) {
        out.push_str(&xml[at..start]);
        at = end;
    }
    out.push_str(&xml[at..]);
    out
}

pub fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    const VOLUMES: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<DescribeVolumesResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
  <requestId>r-1</requestId>
  <volumeSet>
    <item>
      <volumeId>vol-1</volumeId><size>10</size><availabilityZone>us-west-1a</availabilityZone>
      <status>in-use</status>
      <attachmentSet>
        <item><volumeId>vol-1</volumeId><instanceId>i-a</instanceId><device>/dev/sdf</device><status>attached</status></item>
      </attachmentSet>
      <tagSet><item><key>Name</key><value>a &amp; b</value></item></tagSet>
    </item>
    <item><volumeId>vol-2</volumeId><size>1</size><status>available</status><attachmentSet/></item>
  </volumeSet>
</DescribeVolumesResponse>"#;

    #[test]
    fn nested_items_and_text() {
        let set = inner(VOLUMES, "volumeSet").unwrap();
        let items = elements(set, "item");
        assert_eq!(items.len(), 2, "outermost items only");
        let base = without(&without(items[0], "attachmentSet"), "tagSet");
        assert_eq!(text(&base, "volumeId").as_deref(), Some("vol-1"));
        assert_eq!(text(&base, "status").as_deref(), Some("in-use"));
        let att = elements(inner(items[0], "attachmentSet").unwrap(), "item");
        assert_eq!(text(att[0], "instanceId").as_deref(), Some("i-a"));
        assert_eq!(text(items[0], "value").as_deref(), Some("a & b"));
        assert_eq!(inner(items[1], "attachmentSet"), Some(""));
        assert_eq!(text(items[1], "volumeId").as_deref(), Some("vol-2"));
        let err = "<Response><Errors><Error><Code>InvalidVolume.NotFound</Code><Message>The volume 'vol-9' does not exist.</Message></Error></Errors></Response>";
        assert_eq!(text(err, "Code").as_deref(), Some("InvalidVolume.NotFound"));
    }
}
