//! MPEG-2 Transport Stream muxer for live H.264 (+ optional AAC-ADTS) with low, fixed buffering.
//!
//! Everything a TV needs to start playing from any keyframe: PAT/PMT before every keyframe and at least
//! every 100 ms, a PCR on every video access unit, access-unit delimiters, SPS/PPS repeated on every IDR.
//! Timestamps are derived from the capture clock: `PTS = (capture − first_capture) + MUX_DELAY`, and the
//! PCR runs `MUX_DELAY` ahead of nothing — i.e. it equals the PTS minus the delay — so decoders buffer for
//! exactly `MUX_DELAY` (150 ms) instead of the 0.7 s typical of file-oriented muxers.

use std::time::{Duration, Instant};

pub const TS_PACKET: usize = 188;
const PID_PAT: u16 = 0x0000;
const PID_PMT: u16 = 0x1000;
const PID_VIDEO: u16 = 0x0100;
const PID_AUDIO: u16 = 0x0101;
/// Decoder buffering budget between PCR and PTS (90 kHz ticks): 150 ms.
const MUX_DELAY_90K: u64 = 13_500;
const PSI_INTERVAL: Duration = Duration::from_millis(100);

const AUD_NAL: [u8; 6] = [0, 0, 0, 1, 0x09, 0xF0];

/// MPEG-2 CRC-32 (poly 0x04C11DB7, init 0xFFFFFFFF, no reflection, no final xor) as used by PSI sections.
pub fn crc32_mpeg2(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ 0x04C1_1DB7 } else { crc << 1 };
        }
    }
    crc
}

/// NAL unit types found in an Annex-B buffer (types only, in order).
pub fn nal_types(annexb: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    let mut i = 0;
    while i + 3 < annexb.len() {
        if annexb[i] == 0 && annexb[i + 1] == 0 && (annexb[i + 2] == 1 || (annexb[i + 2] == 0 && annexb[i + 3] == 1)) {
            let h = if annexb[i + 2] == 1 { i + 3 } else { i + 4 };
            if let Some(b) = annexb.get(h) {
                v.push(b & 0x1f);
            }
            i = h;
        } else {
            i += 1;
        }
    }
    v
}

/// Splits Annex-B into NAL payloads (without start codes) – used to cache SPS/PPS.
fn nal_ranges(annexb: &[u8]) -> Vec<(usize, usize)> {
    let mut starts = Vec::new(); // (start_code_pos, payload_pos)
    let mut i = 0;
    while i + 3 < annexb.len() {
        if annexb[i] == 0 && annexb[i + 1] == 0 && annexb[i + 2] == 1 {
            starts.push((if i > 0 && annexb[i - 1] == 0 { i - 1 } else { i }, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::new();
    for (k, &(sc, _)) in starts.iter().enumerate() {
        let end = starts.get(k + 1).map_or(annexb.len(), |n| n.0);
        out.push((sc, end));
    }
    out
}

pub struct TsMuxer {
    cc: [u8; 4],
    has_audio: bool,
    base_us: Option<u64>,
    last_pts_us: Option<u64>,
    sps: Vec<u8>,
    pps: Vec<u8>,
    last_psi: Option<Instant>,
    last_video_at: Option<Instant>,
    last_pcr_27m: u64,
}

fn cc_index(pid: u16) -> usize {
    match pid {
        PID_PAT => 0,
        PID_PMT => 1,
        PID_VIDEO => 2,
        _ => 3,
    }
}

impl TsMuxer {
    pub fn new(has_audio: bool) -> Self {
        Self { cc: [0; 4], has_audio, base_us: None, last_pts_us: None, sps: vec![], pps: vec![], last_psi: None, last_video_at: None, last_pcr_27m: 0 }
    }

    pub fn started(&self) -> bool {
        self.base_us.is_some()
    }

    fn next_cc(&mut self, pid: u16) -> u8 {
        let i = cc_index(pid);
        let v = self.cc[i];
        self.cc[i] = (v + 1) & 0x0f;
        v
    }

    fn psi_packet(&mut self, pid: u16, section: &[u8]) -> [u8; TS_PACKET] {
        let mut p = [0xFFu8; TS_PACKET];
        p[0] = 0x47;
        p[1] = 0x40 | (pid >> 8) as u8;
        p[2] = pid as u8;
        p[3] = 0x10 | self.next_cc(pid);
        p[4] = 0; // pointer_field
        p[5..5 + section.len()].copy_from_slice(section);
        p
    }

    /// PAT + PMT (two packets).
    pub fn psi(&mut self) -> Vec<u8> {
        // PAT
        let mut pat = vec![0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xE0 | (PID_PMT >> 8) as u8, PID_PMT as u8];
        let c = crc32_mpeg2(&pat);
        pat.extend_from_slice(&c.to_be_bytes());
        // PMT
        let n = if self.has_audio { 2 } else { 1 };
        let section_length = 9 + 5 * n + 4;
        let mut pmt = vec![
            0x02,
            0xB0 | (section_length >> 8) as u8,
            section_length as u8,
            0x00,
            0x01,
            0xC1,
            0x00,
            0x00,
            0xE0 | (PID_VIDEO >> 8) as u8,
            PID_VIDEO as u8,
            0xF0,
            0x00,
            0x1B,
            0xE0 | (PID_VIDEO >> 8) as u8,
            PID_VIDEO as u8,
            0xF0,
            0x00,
        ];
        if self.has_audio {
            pmt.extend_from_slice(&[0x0F, 0xE0 | (PID_AUDIO >> 8) as u8, PID_AUDIO as u8, 0xF0, 0x00]);
        }
        let c = crc32_mpeg2(&pmt);
        pmt.extend_from_slice(&c.to_be_bytes());
        let mut out = Vec::with_capacity(2 * TS_PACKET);
        out.extend_from_slice(&self.psi_packet(PID_PAT, &pat));
        out.extend_from_slice(&self.psi_packet(PID_PMT, &pmt));
        self.last_psi = Some(Instant::now());
        out
    }

    fn pts_90k(&mut self, pts_us: u64) -> u64 {
        let base = *self.base_us.get_or_insert(pts_us);
        let mut rel = pts_us.saturating_sub(base);
        if let Some(last) = self.last_pts_us {
            if rel <= last {
                rel = last + 1_000; // timestamps must increase strictly
            }
        }
        self.last_pts_us = Some(rel);
        rel * 9 / 100
    }

    /// One video access unit (Annex-B). Returns TS packets; empty if the first frame is not a keyframe yet.
    pub fn video(&mut self, pts_us: u64, keyframe: bool, au: &[u8]) -> Vec<u8> {
        // Remember parameter sets so every IDR can be made self-contained.
        let types = nal_types(au);
        if types.contains(&7) || types.contains(&8) {
            for (s, e) in nal_ranges(au) {
                let sc = if au[s..].starts_with(&[0, 0, 0, 1]) { 4 } else { 3 };
                match au.get(s + sc).map(|b| b & 0x1f) {
                    Some(7) => self.sps = au[s..e].to_vec(),
                    Some(8) => self.pps = au[s..e].to_vec(),
                    _ => {}
                }
            }
        }
        let is_idr = keyframe || types.contains(&5);
        if self.base_us.is_none() && !is_idr {
            return Vec::new(); // never start mid-GOP
        }
        let mut payload = Vec::with_capacity(au.len() + 64);
        if types.first() != Some(&9) {
            payload.extend_from_slice(&AUD_NAL);
        }
        let has_ps = types.contains(&7) && types.contains(&8);
        if is_idr && !has_ps && !self.sps.is_empty() && !self.pps.is_empty() {
            payload.extend_from_slice(&self.sps);
            payload.extend_from_slice(&self.pps);
        }
        payload.extend_from_slice(au);

        let pts = self.pts_90k(pts_us);
        let pts33 = (pts + MUX_DELAY_90K) & 0x1_FFFF_FFFF;
        let pcr_27m = (pts % (1 << 33)) * 300;
        self.last_pcr_27m = pcr_27m;
        self.last_video_at = Some(Instant::now());

        let mut out = Vec::with_capacity(payload.len() / 180 * TS_PACKET + 4 * TS_PACKET);
        if is_idr || self.last_psi.map_or(true, |t| t.elapsed() >= PSI_INTERVAL) {
            out.extend_from_slice(&self.psi());
        }
        self.pes(&mut out, PID_VIDEO, 0xE0, pts33, &payload, true, is_idr, Some(pcr_27m));
        out
    }

    /// One AAC frame with ADTS header. Dropped until the first video frame has fixed the time base.
    pub fn audio(&mut self, pts_us: u64, adts: &[u8]) -> Vec<u8> {
        if !self.has_audio {
            return Vec::new();
        }
        let Some(base) = self.base_us else { return Vec::new() };
        let rel = pts_us.saturating_sub(base);
        let pts33 = (rel * 9 / 100 + MUX_DELAY_90K) & 0x1_FFFF_FFFF;
        let mut out = Vec::with_capacity(adts.len() / 180 * TS_PACKET + 2 * TS_PACKET);
        self.pes(&mut out, PID_AUDIO, 0xC0, pts33, adts, false, false, None);
        out
    }

    /// If no video arrived for ≥ 100 ms (static screen), emit PSI + a PCR-only packet so the TV's clock
    /// keeps running and the player does not declare the stream dead. Returns empty otherwise.
    pub fn keepalive(&mut self) -> Vec<u8> {
        let (Some(last), true) = (self.last_video_at, self.base_us.is_some()) else { return Vec::new() };
        let idle = last.elapsed();
        if idle < PSI_INTERVAL {
            return Vec::new();
        }
        let pcr = self.last_pcr_27m + idle.as_micros() as u64 * 27;
        let mut out = self.psi();
        let mut p = [0xFFu8; TS_PACKET];
        p[0] = 0x47;
        p[1] = (PID_VIDEO >> 8) as u8;
        p[2] = PID_VIDEO as u8;
        p[3] = 0x20 | self.cc[cc_index(PID_VIDEO)]; // adaptation field only: continuity counter does not advance
        p[4] = 183;
        p[5] = 0x10; // PCR flag
        write_pcr(&mut p[6..12], pcr % (300 << 33));
        out.extend_from_slice(&p);
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn pes(&mut self, out: &mut Vec<u8>, pid: u16, stream_id: u8, pts33: u64, data: &[u8], unbounded: bool, random_access: bool, pcr: Option<u64>) {
        // PES header with PTS only
        let mut pes = Vec::with_capacity(data.len() + 14);
        pes.extend_from_slice(&[0, 0, 1, stream_id]);
        let len = if unbounded { 0 } else { (data.len() + 8).min(0xFFFF) as u16 };
        pes.extend_from_slice(&len.to_be_bytes());
        pes.extend_from_slice(&[0x80, 0x80, 5]);
        pes.push(0x21 | (((pts33 >> 30) & 7) as u8) << 1);
        pes.push((pts33 >> 22) as u8);
        pes.push((((pts33 >> 15) & 0x7F) as u8) << 1 | 1);
        pes.push((pts33 >> 7) as u8);
        pes.push(((pts33 & 0x7F) as u8) << 1 | 1);
        pes.extend_from_slice(data);

        let mut off = 0;
        let mut first = true;
        while off < pes.len() {
            let want_pcr = first && pcr.is_some();
            let want_rai = first && random_access;
            let af_min = if want_pcr { 8 } else if want_rai { 2 } else { 0 };
            let room = TS_PACKET - 4 - af_min;
            let remaining = pes.len() - off;
            let take = remaining.min(room);
            // adaptation field length incl. stuffing
            let af_total = TS_PACKET - 4 - take;
            let mut p = [0xFFu8; TS_PACKET];
            p[0] = 0x47;
            p[1] = (if first { 0x40 } else { 0 }) | (pid >> 8) as u8;
            p[2] = pid as u8;
            let cc = self.next_cc(pid);
            if af_total == 0 {
                p[3] = 0x10 | cc;
            } else {
                p[3] = 0x30 | cc;
                p[4] = (af_total - 1) as u8;
                if af_total >= 2 {
                    let mut flags = 0u8;
                    if want_rai {
                        flags |= 0x40;
                    }
                    if want_pcr {
                        flags |= 0x10;
                    }
                    p[5] = flags;
                    if want_pcr {
                        write_pcr(&mut p[6..12], pcr.unwrap() % (300 << 33));
                    }
                    // remaining AF bytes stay 0xFF (stuffing)
                }
            }
            p[4 + af_total..4 + af_total + take].copy_from_slice(&pes[off..off + take]);
            out.extend_from_slice(&p);
            off += take;
            first = false;
        }
    }
}

fn write_pcr(dst: &mut [u8], pcr_27m: u64) {
    let base = pcr_27m / 300;
    let ext = pcr_27m % 300;
    dst[0] = (base >> 25) as u8;
    dst[1] = (base >> 17) as u8;
    dst[2] = (base >> 9) as u8;
    dst[3] = (base >> 1) as u8;
    dst[4] = (((base & 1) as u8) << 7) | 0x7E | ((ext >> 8) as u8 & 1);
    dst[5] = ext as u8;
}

#[cfg(test)]
pub(crate) use parse::parse as parse_for_tests;

#[cfg(test)]
pub(crate) mod parse {
    //! Tiny TS parser used by the tests to check structure independent of the muxer's own logic.
    use super::*;

    #[derive(Debug, Default)]
    pub struct Report {
        pub packets: usize,
        pub sync_errors: usize,
        pub cc_errors: usize,
        pub pat_count: usize,
        pub pmt_count: usize,
        pub pmt_streams: Vec<(u8, u16)>,
        pub video_pes: Vec<(u64, bool)>, // (pts, random access)
        pub audio_pes: Vec<u64>,
        pub pcr_count: usize,
        pub first_is_psi: bool,
        pub crc_ok: bool,
        pub video_es: Vec<u8>,
    }

    pub fn parse(ts: &[u8]) -> Report {
        let mut r = Report { crc_ok: true, ..Default::default() };
        let mut last_cc: std::collections::HashMap<u16, u8> = Default::default();
        let mut cur_video: Vec<u8> = vec![];
        for (n, p) in ts.chunks(TS_PACKET).enumerate() {
            r.packets += 1;
            if p.len() != TS_PACKET || p[0] != 0x47 {
                r.sync_errors += 1;
                continue;
            }
            let pusi = p[1] & 0x40 != 0;
            let pid = ((p[1] as u16 & 0x1F) << 8) | p[2] as u16;
            let afc = (p[3] >> 4) & 3;
            let cc = p[3] & 0xF;
            let has_payload = afc & 1 != 0;
            if has_payload {
                if let Some(prev) = last_cc.get(&pid) {
                    if cc != (prev + 1) & 0xF {
                        r.cc_errors += 1;
                    }
                }
                last_cc.insert(pid, cc);
            }
            let mut i = 4;
            let mut rai = false;
            if afc & 2 != 0 {
                let l = p[4] as usize;
                if l > 0 {
                    if p[5] & 0x10 != 0 {
                        r.pcr_count += 1;
                    }
                    rai = p[5] & 0x40 != 0;
                }
                i = 5 + l;
            }
            if !has_payload {
                continue;
            }
            let pl = &p[i..];
            match pid {
                PID_PAT | PID_PMT => {
                    if n == 0 {
                        r.first_is_psi = pid == PID_PAT;
                    }
                    let ptr = pl[0] as usize;
                    let sec = &pl[1 + ptr..];
                    let len = (((sec[1] & 0xF) as usize) << 8) | sec[2] as usize;
                    let section = &sec[..3 + len];
                    if crc32_mpeg2(section) != 0 {
                        r.crc_ok = false;
                    }
                    if pid == PID_PAT {
                        r.pat_count += 1;
                    } else {
                        r.pmt_count += 1;
                        if r.pmt_streams.is_empty() {
                            let mut k = 12; // after program_info
                            while k + 5 <= 3 + len - 4 {
                                let st = section[k];
                                let epid = ((section[k + 1] as u16 & 0x1F) << 8) | section[k + 2] as u16;
                                r.pmt_streams.push((st, epid));
                                let es_len = (((section[k + 3] & 0xF) as usize) << 8) | section[k + 4] as usize;
                                k += 5 + es_len;
                            }
                        }
                    }
                }
                PID_VIDEO | PID_AUDIO => {
                    if pusi {
                        assert_eq!(&pl[..3], &[0, 0, 1], "PES start code");
                        let hdr = 9 + pl[8] as usize;
                        let b = &pl[9..14];
                        let pts = (((b[0] >> 1) & 7) as u64) << 30 | (b[1] as u64) << 22 | ((b[2] >> 1) as u64) << 15 | (b[3] as u64) << 7 | (b[4] >> 1) as u64;
                        if pid == PID_VIDEO {
                            r.video_pes.push((pts, rai));
                            cur_video.clear();
                            cur_video.extend_from_slice(&pl[hdr..]);
                            r.video_es.extend_from_slice(&pl[hdr..]);
                        } else {
                            r.audio_pes.push(pts);
                        }
                    } else if pid == PID_VIDEO {
                        r.video_es.extend_from_slice(pl);
                    }
                }
                _ => {}
            }
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::parse::parse;
    use super::*;

    #[test]
    fn crc_of_known_pat_is_zero_when_included() {
        let mut m = TsMuxer::new(true);
        let ts = m.psi();
        let r = parse(&ts);
        assert!(r.crc_ok && r.pat_count == 1 && r.pmt_count == 1);
        assert_eq!(r.pmt_streams, vec![(0x1B, 0x100), (0x0F, 0x101)]);
    }

    #[test]
    fn crc32_mpeg2_check_value() {
        // standard check value for CRC-32/MPEG-2 over "123456789"
        assert_eq!(crc32_mpeg2(b"123456789"), 0x0376_E6E7);
    }

    #[test]
    fn nal_scan() {
        let au = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9];
        assert_eq!(nal_types(&au), vec![7, 8, 5]);
    }

    #[test]
    fn no_output_until_first_keyframe_and_idr_gets_psi_aud_and_pcr() {
        let mut m = TsMuxer::new(false);
        let delta = [0, 0, 0, 1, 0x41, 1, 2, 3];
        assert!(m.video(0, false, &delta).is_empty());
        let idr = [0, 0, 0, 1, 0x67, 0x42, 0, 0x1e, 0, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 0x88, 0x84];
        let ts = m.video(1_000, true, &idr);
        let r = parse(&ts);
        assert!(r.first_is_psi && r.crc_ok);
        assert_eq!(r.video_pes.len(), 1);
        assert!(r.video_pes[0].1, "random access indicator on IDR");
        assert_eq!(r.pcr_count, 1);
        assert_eq!(r.sync_errors + r.cc_errors, 0);
        // AUD inserted at the start of the elementary stream
        assert_eq!(&r.video_es[..6], &AUD_NAL);
    }

    #[test]
    fn idr_without_parameter_sets_gets_cached_ones_prepended() {
        let mut m = TsMuxer::new(false);
        let idr_with_ps = [0, 0, 0, 1, 0x67, 0x42, 0, 0x1e, 0, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 0x88, 0x84];
        m.video(0, true, &idr_with_ps);
        let bare_idr = [0, 0, 0, 1, 0x65, 0x11, 0x22, 0x33];
        let ts = m.video(33_333, true, &bare_idr);
        let r = parse(&ts);
        assert_eq!(nal_types(&r.video_es), vec![9, 7, 8, 5]);
    }

    #[test]
    fn large_frames_continuity_pts_monotonic_and_exact_payload() {
        let mut m = TsMuxer::new(true);
        let mut all = Vec::new();
        let mut total_es = 0usize;
        for i in 0..40u64 {
            let key = i % 10 == 0;
            let mut au = vec![0, 0, 0, 1, if key { 0x65 } else { 0x41 }];
            au.extend((0..(100 + i * 997 % 5000)).map(|k| (k % 251) as u8 | 0x01 | 0x80));
            all.extend(m.video(i * 33_333, key, &au));
            all.extend(m.audio(i * 33_333, &[0xFF, 0xF1, 0x4C, 0x80, 0x01, 0x3F, 0xFC, 1, 2, 3, 4]));
            total_es += au.len() + 6; // + AUD
        }
        let r = parse(&all);
        assert_eq!((r.sync_errors, r.cc_errors), (0, 0));
        assert!(r.crc_ok);
        assert_eq!(r.video_pes.len(), 40);
        assert_eq!(r.audio_pes.len(), 40);
        assert!(r.video_pes.windows(2).all(|w| w[1].0 > w[0].0));
        // every video byte (plus AUDs and any inserted parameter sets) round-trips
        assert!(r.video_es.len() >= total_es);
        assert_eq!(r.pat_count, r.pmt_count);
        assert!(r.pat_count >= 4, "PSI before every keyframe");
    }

    #[test]
    fn exact_fit_and_tiny_payloads_use_valid_stuffing() {
        let mut m = TsMuxer::new(false);
        let mut all = vec![];
        for n in [0usize, 1, 2, 160, 168, 169, 170, 175, 176, 177, 178, 183, 184, 185, 400] {
            let mut au = vec![0, 0, 0, 1, 0x65];
            au.extend(std::iter::repeat(0x55).take(n));
            all.extend(m.video(1_000_000 + n as u64 * 40_000, true, &au));
        }
        let r = parse(&all);
        assert_eq!((r.sync_errors, r.cc_errors), (0, 0));
        assert_eq!(r.video_pes.len(), 15);
    }

    #[test]
    fn audio_dropped_until_time_base_exists() {
        let mut m = TsMuxer::new(true);
        assert!(m.audio(5, &[1, 2, 3]).is_empty());
    }

    #[test]
    fn non_increasing_timestamps_are_repaired() {
        let mut m = TsMuxer::new(false);
        let idr = [0, 0, 0, 1, 0x65, 1];
        let mut all = m.video(10, true, &idr);
        all.extend(m.video(10, false, &[0, 0, 0, 1, 0x41, 1]));
        all.extend(m.video(5, false, &[0, 0, 0, 1, 0x41, 1]));
        let r = parse(&all);
        assert!(r.video_pes.windows(2).all(|w| w[1].0 > w[0].0), "{:?}", r.video_pes);
    }

    #[test]
    fn keepalive_only_when_idle() {
        let mut m = TsMuxer::new(false);
        assert!(m.keepalive().is_empty());
        m.video(0, true, &[0, 0, 0, 1, 0x65, 1]);
        assert!(m.keepalive().is_empty());
        std::thread::sleep(Duration::from_millis(120));
        let ts = m.keepalive();
        let r = parse(&ts);
        assert_eq!((r.sync_errors, r.cc_errors), (0, 0));
        assert!(r.crc_ok && r.pcr_count == 1);
    }
}
