//! Capture source model shared by the UI and the pipeline.

use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SourceKind {
    Monitor { index: u32 },
    Window,
}

#[derive(Debug, Clone, Serialize)]
pub struct Source {
    /// Stable within one process run: `m:<hmonitor>` or `w:<hwnd>`.
    pub id: String,
    pub name: String,
    #[serde(flatten)]
    pub kind: SourceKind,
    pub width: u32,
    pub height: u32,
    /// Display refresh rate for monitors; 0 = unknown/not applicable.
    pub refresh_hz: u32,
}

pub fn parse_id(id: &str) -> Option<(char, isize)> {
    let (k, v) = id.split_once(':')?;
    let k = k.chars().next()?;
    if k != 'm' && k != 'w' {
        return None;
    }
    Some((k, v.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_roundtrip() {
        assert_eq!(parse_id("m:65537"), Some(('m', 65537)));
        assert_eq!(parse_id("w:-5"), Some(('w', -5)));
        assert_eq!(parse_id("x:1"), None);
        assert_eq!(parse_id("m:abc"), None);
        assert_eq!(parse_id("garbage"), None);
    }
}
