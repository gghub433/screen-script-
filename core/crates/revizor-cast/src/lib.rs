//! Casting to TVs that have nothing of Revizor installed.
//!
//! Most TVs can already *play a network stream*: Chromecast / Android TV / Google TV through the
//! **Google Cast** protocol, and Samsung / LG / Sony / Philips / Hisense / … through **DLNA / UPnP**
//! (`MediaRenderer`). This crate finds them on the local network, serves a live MPEG-TS stream over HTTP
//! (progressive for DLNA, HLS for Cast) and tells the TV to play it.
//!
//! Trade-offs, stated plainly: the TV decides how much it buffers, so the delay is typically 1–6 s (not the
//! ~50 ms of the Revizor protocol), the stream is plain HTTP on the LAN (not end-to-end encrypted; it is
//! limited to the TV's IP address and an unguessable URL), and there is no loss repair or adaptive bitrate
//! because a TV cannot report measurements. Use the Revizor receiver app when delay or privacy matter.

pub mod ts;
pub mod hls;
pub mod server;
pub mod http_client;
pub mod ssdp;
pub mod upnp;
pub mod xml;
pub mod mdns;
pub mod castv2;
pub mod discover;
pub mod session;

pub use discover::{scan_tvs, Method, Tv};
pub use session::{CastConfig, CastEvent, CastSession, CastState, CastStats};

#[cfg(any(test, feature = "testkit"))]
pub mod testkit;
