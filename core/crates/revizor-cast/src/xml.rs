//! Just enough XML text extraction for UPnP device descriptions and SOAP replies (no general parser needed,
//! and nothing here ever evaluates entities beyond the five predefined ones).

pub fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// Text of the first `<tag>…</tag>` (namespace prefixes ignored), unescaped.
pub fn first_text(xml: &str, tag: &str) -> Option<String> {
    blocks(xml, tag).into_iter().next().map(|b| unescape(b.trim()))
}

/// Inner content of every `<tag …>…</tag>` occurrence (non-nested use only).
pub fn blocks<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some((open_end, close_start, close_end)) = find_block(xml, tag, pos) {
        out.push(&xml[open_end..close_start]);
        pos = close_end;
    }
    out
}

fn find_block(xml: &str, tag: &str, from: usize) -> Option<(usize, usize, usize)> {
    let bytes = xml.as_bytes();
    let mut i = from;
    while let Some(lt) = xml[i..].find('<').map(|p| p + i) {
        let name_start = lt + 1;
        let name_end = xml[name_start..].find(|c: char| c == '>' || c == ' ' || c == '/' || c == '\t' || c == '\r' || c == '\n').map(|p| p + name_start)?;
        let name = &xml[name_start..name_end];
        let local = name.rsplit(':').next().unwrap_or(name);
        if local == tag && !name.starts_with('/') {
            let gt = xml[name_end..].find('>').map(|p| p + name_end)?;
            if bytes[gt - 1] == b'/' {
                i = gt + 1;
                continue; // self-closing
            }
            // matching close tag
            let mut j = gt + 1;
            while let Some(c) = xml[j..].find("</").map(|p| p + j) {
                let ce = xml[c..].find('>').map(|p| p + c)?;
                let cname = &xml[c + 2..ce];
                if cname.rsplit(':').next().unwrap_or(cname) == tag {
                    return Some((gt + 1, c, ce + 1));
                }
                j = ce + 1;
            }
            return None;
        }
        i = name_end;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extraction_ignores_namespaces_and_unescapes() {
        let x = r#"<?xml version="1.0"?><root xmlns="urn:schemas-upnp-org:device-1-0"><device><friendlyName>Living &amp; Room</friendlyName>
        <u:thing xmlns:u="a">x</u:thing><serviceList><service><serviceType>urn:a:AVTransport:1</serviceType><controlURL>/c1</controlURL></service>
        <service><serviceType>urn:a:RenderingControl:1</serviceType><controlURL>/c2</controlURL></service></serviceList></device></root>"#;
        assert_eq!(first_text(x, "friendlyName").unwrap(), "Living & Room");
        assert_eq!(first_text(x, "thing").unwrap(), "x");
        let svcs = blocks(x, "service");
        assert_eq!(svcs.len(), 2);
        assert_eq!(first_text(svcs[1], "controlURL").unwrap(), "/c2");
        assert!(first_text(x, "missing").is_none());
    }

    #[test]
    fn escape_roundtrip() {
        let s = "<a href=\"x\">&'</a>";
        assert_eq!(unescape(&escape(s)), s);
    }
}
