//! UPnP AVTransport control: tell a DLNA renderer to play a URL.

use crate::http_client;
use crate::xml;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportState {
    Playing,
    Transitioning,
    Paused,
    Stopped,
    NoMedia,
    Other(String),
}

impl TransportState {
    fn parse(s: &str) -> Self {
        match s {
            "PLAYING" => Self::Playing,
            "TRANSITIONING" => Self::Transitioning,
            "PAUSED_PLAYBACK" | "PAUSED_RECORDING" => Self::Paused,
            "STOPPED" => Self::Stopped,
            "NO_MEDIA_PRESENT" => Self::NoMedia,
            o => Self::Other(o.to_string()),
        }
    }
}

#[derive(Debug)]
pub enum UpnpError {
    Io(std::io::Error),
    /// SOAP fault with the UPnP error code/description from the device.
    Fault { code: String, description: String },
    Http(u16),
}

impl std::fmt::Display for UpnpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpnpError::Io(e) => write!(f, "cannot reach the TV: {e}"),
            UpnpError::Fault { code, description } => write!(f, "the TV refused ({code}: {description})"),
            UpnpError::Http(c) => write!(f, "the TV answered HTTP {c}"),
        }
    }
}

pub struct AvTransport {
    pub control_url: String,
    pub service_type: String,
    timeout: Duration,
}

pub fn didl(title: &str, url: &str, protocol_info: &str) -> String {
    format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" xmlns:dlna=\"urn:schemas-dlna-org:metadata-1-0/\">\
<item id=\"1\" parentID=\"0\" restricted=\"1\"><dc:title>{}</dc:title><upnp:class>object.item.videoItem</upnp:class>\
<res protocolInfo=\"{}\">{}</res></item></DIDL-Lite>",
        xml::escape(title),
        xml::escape(protocol_info),
        xml::escape(url)
    )
}

pub fn protocol_info(dlna_features: &str) -> String {
    format!("http-get:*:video/mpeg:{dlna_features}")
}

impl AvTransport {
    pub fn new(control_url: &str, service_type: &str) -> Self {
        Self { control_url: control_url.to_string(), service_type: service_type.to_string(), timeout: Duration::from_secs(5) }
    }

    fn action(&self, name: &str, args: &[(&str, String)]) -> Result<String, UpnpError> {
        let mut body = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{name} xmlns:u=\"{}\"><InstanceID>0</InstanceID>",
            self.service_type
        );
        for (k, v) in args {
            body.push_str(&format!("<{k}>{}</{k}>", xml::escape(v)));
        }
        body.push_str(&format!("</u:{name}></s:Body></s:Envelope>"));
        let headers = [("CONTENT-TYPE", "text/xml; charset=\"utf-8\"".to_string()), ("SOAPACTION", format!("\"{}#{name}\"", self.service_type))];
        let resp = http_client::request("POST", &self.control_url, &headers, body.as_bytes(), self.timeout).map_err(UpnpError::Io)?;
        let text = resp.text();
        if resp.status == 200 {
            return Ok(text);
        }
        if let Some(code) = xml::first_text(&text, "errorCode") {
            return Err(UpnpError::Fault { code, description: xml::first_text(&text, "errorDescription").unwrap_or_default() });
        }
        Err(UpnpError::Http(resp.status))
    }

    pub fn set_uri(&self, url: &str, title: &str, dlna_features: &str) -> Result<(), UpnpError> {
        let meta = didl(title, url, &protocol_info(dlna_features));
        // The metadata is itself XML carried as text, so `action` escapes it once more – exactly what the spec requires.
        self.action("SetAVTransportURI", &[("CurrentURI", url.to_string()), ("CurrentURIMetaData", meta)]).map(|_| ())
    }

    pub fn play(&self) -> Result<(), UpnpError> {
        self.action("Play", &[("Speed", "1".to_string())]).map(|_| ())
    }

    pub fn stop(&self) -> Result<(), UpnpError> {
        self.action("Stop", &[]).map(|_| ())
    }

    pub fn state(&self) -> Result<TransportState, UpnpError> {
        let r = self.action("GetTransportInfo", &[])?;
        Ok(TransportState::parse(&xml::first_text(&r, "CurrentTransportState").unwrap_or_default()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn didl_is_wellformed_and_escaped() {
        let d = didl("A & B", "http://1.2.3.4:5/t/live.ts?a=1&b=2", &protocol_info("DLNA.ORG_OP=00"));
        assert!(d.contains("<dc:title>A &amp; B</dc:title>"));
        assert!(d.contains("a=1&amp;b=2"));
        assert!(d.contains("protocolInfo=\"http-get:*:video/mpeg:DLNA.ORG_OP=00\""));
    }

    #[test]
    fn transport_state_parse() {
        assert_eq!(TransportState::parse("PLAYING"), TransportState::Playing);
        assert_eq!(TransportState::parse("STOPPED"), TransportState::Stopped);
        assert_eq!(TransportState::parse("WEIRD"), TransportState::Other("WEIRD".into()));
    }
}
