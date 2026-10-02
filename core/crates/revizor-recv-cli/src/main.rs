//! `revizor-recv` — development receiver.
//!
//! Receives a Revizor stream over the real network stack, authenticates and
//! decrypts it, reassembles frames and writes the raw elementary stream
//! (H.264/H.265 Annex-B) to a file or stdout, so it can be played with e.g.
//! `revizor-recv --out - | ffplay -f h264 -fflags nobuffer -i -`.
//! It does not decode, so it advertises *software* (non-hardware) decode caps.
//! Statistics are printed to stderr once per second and are all measured.

use revizor_crypto::{FileTrustStore, Identity, TrustStore};
use revizor_proto::discovery::DEFAULT_MEDIA_PORT;
use revizor_proto::{AudioCodec, Capabilities, Codec, CodecCap, Platform, TransportKind};
use revizor_session::{MonotonicClock, ReceiverConfig, ReceiverEvent, ReceiverSession};
use revizor_transport::discovery::{broadcast_targets, DiscoveryResponder};
use revizor_transport::{TcpTransport, Transport, UdpTransport};
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Args {
    name: String,
    port: u16,
    dir: PathBuf,
    out: Option<String>,
    pair: bool,
    tcp: bool,
    list: bool,
    remove: Option<String>,
}

fn parse() -> Result<Args, String> {
    let mut a = Args {
        name: hostname(),
        port: DEFAULT_MEDIA_PORT,
        dir: std::env::var_os("REVIZOR_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".revizor")))
            .unwrap_or_else(|| PathBuf::from(".revizor")),
        out: None,
        pair: false,
        tcp: false,
        list: false,
        remove: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut val = |what: &str| it.next().ok_or(format!("{what} needs a value"));
        match k.as_str() {
            "--name" => a.name = val("--name")?,
            "--port" => a.port = val("--port")?.parse().map_err(|_| "bad port")?,
            "--data-dir" => a.dir = PathBuf::from(val("--data-dir")?),
            "--out" => a.out = Some(val("--out")?),
            "--pair" => a.pair = true,
            "--tcp" => a.tcp = true,
            "--list-trusted" => a.list = true,
            "--remove" => a.remove = Some(val("--remove")?),
            "-h" | "--help" => {
                println!(
                    "revizor-recv [--name NAME] [--port N] [--data-dir DIR] [--out FILE|-] [--pair] [--tcp]\n\
                     \x20            [--list-trusted] [--remove DEVICE_ID]\n\n\
                     --pair  open a pairing window and print the PIN\n\
                     --tcp   listen on TCP (e.g. for `adb reverse`) instead of UDP"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(a)
}

fn hostname() -> String {
    std::env::var("HOSTNAME").or_else(|_| std::env::var("COMPUTERNAME")).unwrap_or_else(|_| "Revizor Receiver".into())
}

fn load_identity(dir: &std::path::Path) -> std::io::Result<Identity> {
    let path = dir.join("identity.key");
    if let Ok(s) = std::fs::read_to_string(&path) {
        if let Some(b) = revizor_crypto::identity::unhex(s.trim()).and_then(|v| <[u8; 32]>::try_from(v).ok()) {
            return Ok(Identity::from_secret(b));
        }
    }
    let id = Identity::generate();
    std::fs::write(&path, revizor_crypto::identity::hex(&id.secret_bytes()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(id)
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args = parse()?;
    std::fs::create_dir_all(&args.dir).map_err(|e| e.to_string())?;
    let identity = Arc::new(load_identity(&args.dir).map_err(|e| e.to_string())?);
    let trust = Arc::new(FileTrustStore::open(args.dir.join("trusted.tsv")).map_err(|e| e.to_string())?);

    if args.list {
        for d in trust.list() {
            println!("{}  {}", d.id(), d.name);
        }
        return Ok(());
    }
    if let Some(id) = &args.remove {
        println!("{}", if trust.remove(id) { "removed" } else { "not found" });
        return Ok(());
    }

    let caps = Capabilities {
        device_name: args.name.clone(),
        platform: Platform::Linux,
        codecs: vec![CodecCap { codec: Codec::H264, max_width: 7680, max_height: 4320, max_fps: 240, hardware: false }],
        audio: vec![AudioCodec::AacLc, AudioCodec::Opus],
        transports: TransportKind::Udp.bit() | TransportKind::Tcp.bit(),
        hdr: false,
        max_bitrate_bps: 200_000_000,
    };

    let bind: SocketAddr = format!("0.0.0.0:{}", args.port).parse().unwrap();
    let transport: Arc<dyn Transport> = if args.tcp {
        Arc::new(TcpTransport::listen(bind).map_err(|e| e.to_string())?)
    } else {
        Arc::new(UdpTransport::bind(bind).map_err(|e| e.to_string())?)
    };

    let mut sink: Option<Box<dyn Write + Send>> = match args.out.as_deref() {
        None => None,
        Some("-") => Some(Box::new(std::io::stdout())),
        Some(p) => Some(Box::new(std::fs::File::create(p).map_err(|e| e.to_string())?)),
    };

    let cfg = ReceiverConfig::new(identity.clone(), trust.clone(), caps);
    let session = Arc::new(ReceiverSession::start(cfg, transport, Arc::new(MonotonicClock), |e| match e {
        ReceiverEvent::PairingOpened { pin } => eprintln!("PAIRING PIN: {pin}"),
        ReceiverEvent::Paired { name, device_id } => eprintln!("paired with {name} ({device_id})"),
        ReceiverEvent::PairingLocked => eprintln!("pairing locked after too many wrong PINs"),
        ReceiverEvent::SenderConnected { name, device_id } => eprintln!("sender connected: {name} ({device_id})"),
        ReceiverEvent::Params(p) => eprintln!(
            "stream: {:?} {}x{} @{} fps, {} kbit/s (epoch {})",
            p.video.codec,
            p.video.width,
            p.video.height,
            p.video.fps,
            p.video.bitrate_bps / 1000,
            p.epoch
        ),
        ReceiverEvent::SenderDisconnected => eprintln!("sender disconnected"),
        other => eprintln!("{other:?}"),
    }));

    let ann_session = session.clone();
    let name = args.name.clone();
    let port = args.port;
    let _disc = DiscoveryResponder::start(DiscoveryResponder::default_bind(), broadcast_targets(revizor_proto::discovery::DISCOVERY_PORT), move || {
        ann_session.announcement(&name, port)
    })
    .map_err(|e| format!("discovery: {e}"))?;

    eprintln!("Revizor receiver '{}' (id {}) listening on {} {}", args.name, identity.device_id(), if args.tcp { "tcp" } else { "udp" }, bind);
    if args.pair {
        session.open_pairing();
    }

    let mut last = Instant::now();
    loop {
        if let Some(f) = session.next_frame(Duration::from_millis(50)) {
            if let Some(w) = sink.as_mut() {
                if w.write_all(&f.data).is_err() {
                    break;
                }
            }
            // No decoder here: report decode_us = 0 honestly by not calling on_frame_presented
            // with a made-up value; latency shown is arrival latency only.
        }
        if last.elapsed() >= Duration::from_secs(1) {
            last = Instant::now();
            let s = session.stats();
            if s.params.is_some() {
                let ms = |u: Option<u32>| u.map_or("n/a".to_string(), |v| format!("{:.1} ms", v as f32 / 1000.0));
                eprintln!(
                    "fps {:5.1} | {:6.2} Mbit/s | loss {:4.1}% | jitter {} | rtt {} | arrival latency {} | fec {} retx {} nack {} kf-req {} | dropped {}",
                    s.fps,
                    s.recv_bitrate_bps as f32 / 1e6,
                    s.loss_pct,
                    ms(Some(s.jitter_us)),
                    ms(s.rtt_us),
                    ms(s.network_latency_us),
                    s.packets_recovered_fec,
                    s.packets_recovered_retx,
                    s.nacks_sent,
                    s.keyframe_requests,
                    s.frames_abandoned + s.frames_discarded + s.frames_dropped_app,
                );
            }
        }
    }
    Ok(())
}
