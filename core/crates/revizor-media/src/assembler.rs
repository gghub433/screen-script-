//! Receiver-side frame reassembly.
//!
//! Responsibilities:
//! * collect packets into frames (tolerating reordering and duplicates),
//! * rebuild lost packets from XOR parity (interleaved groups),
//! * ask the sender to retransmit what FEC cannot rebuild (NACK),
//! * deliver frames strictly in order with a bounded wait,
//! * when a frame is truly lost, stop feeding the decoder broken references and
//!   request a keyframe instead (a single lost packet never hangs the stream).

use crate::{newer_u16, newer_u32};
use revizor_proto::wire::Reader;
use revizor_proto::{FecHeader, KeyframeReason, MediaHeader, FLAG_KEYFRAME, FLAG_RETRANSMIT};
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct AssemblerConfig {
    /// A frame still incomplete this long after its first packet is abandoned.
    pub frame_deadline_us: u64,
    /// Keyframes are larger, give them more time before giving up.
    pub keyframe_deadline_us: u64,
    /// Wait this long after first packet before NACKing (lets reordering settle and FEC arrive).
    pub nack_delay_us: u64,
    /// Minimum spacing between NACKs for one frame; the session raises it to ~1.5×RTT.
    pub nack_interval_us: u64,
    pub max_nacks: u8,
    /// Re-request a keyframe at most this often while waiting for one.
    pub keyframe_retry_us: u64,
    /// At stream start the sender emits a keyframe on its own; do not ask for another before this.
    pub initial_keyframe_grace_us: u64,
    /// Hard bound on frames buffered at once.
    pub max_partial_frames: usize,
}

impl Default for AssemblerConfig {
    fn default() -> Self {
        Self {
            frame_deadline_us: 60_000,
            keyframe_deadline_us: 150_000,
            nack_delay_us: 2_000,
            nack_interval_us: 12_000,
            max_nacks: 4,
            keyframe_retry_us: 250_000,
            initial_keyframe_grace_us: 400_000,
            max_partial_frames: 128,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReceivedFrame {
    pub epoch: u16,
    pub frame_id: u32,
    pub pts_us: u64,
    pub keyframe: bool,
    pub data: Vec<u8>,
    /// Local time the first packet of the frame arrived.
    pub first_packet_us: u64,
    /// Local time the frame became complete (all packets present).
    pub complete_us: u64,
}

#[derive(Debug, Default, Clone)]
pub struct AssemblerOutput {
    pub frames: Vec<ReceivedFrame>,
    pub nacks: Vec<NackRequest>,
    pub keyframe_request: Option<KeyframeReason>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NackRequest {
    pub epoch: u16,
    pub frame_id: u32,
    pub missing: Vec<u16>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct AssemblerCounters {
    pub frames_delivered: u64,
    /// Frames given up on (timeout / buffer overflow).
    pub frames_abandoned: u64,
    /// Complete delta frames thrown away because the reference chain was broken.
    pub frames_discarded: u64,
    pub packets_recovered_fec: u64,
    pub packets_recovered_retx: u64,
    pub duplicates: u64,
    pub nacks_sent: u64,
    pub keyframe_requests: u64,
}

struct Partial {
    epoch: u16,
    pkt_count: u16,
    slots: Vec<Option<Vec<u8>>>,
    parity: Vec<Option<(u16, Vec<u8>)>>,
    groups: u16,
    received: u16,
    max_idx: u16,
    pts_us: u64,
    flags: u8,
    have_meta: bool,
    first_seen_us: u64,
    last_nack_us: u64,
    nacks: u8,
}

impl Partial {
    fn new(epoch: u16, pkt_count: u16, now: u64) -> Self {
        Self {
            epoch,
            pkt_count,
            slots: vec![None; pkt_count as usize],
            parity: vec![],
            groups: 0,
            received: 0,
            max_idx: 0,
            pts_us: 0,
            flags: 0,
            have_meta: false,
            first_seen_us: now,
            last_nack_us: 0,
            nacks: 0,
        }
    }
    fn complete(&self) -> bool {
        self.received == self.pkt_count
    }
    fn is_key(&self) -> bool {
        self.have_meta && self.flags & FLAG_KEYFRAME != 0
    }
    fn missing(&self) -> Vec<u16> {
        self.slots.iter().enumerate().filter(|(_, s)| s.is_none()).map(|(i, _)| i as u16).collect()
    }

    /// Attempts XOR recovery for every group with parity and exactly one hole.
    /// Returns how many packets were rebuilt.
    fn try_fec(&mut self) -> u64 {
        let mut rebuilt = 0;
        for g in 0..self.parity.len() {
            let Some((xor_len, par)) = &self.parity[g] else { continue };
            let groups = self.groups as usize;
            let members: Vec<usize> = (g..self.pkt_count as usize).step_by(groups).collect();
            let missing: Vec<usize> = members.iter().copied().filter(|&i| self.slots[i].is_none()).collect();
            if missing.len() != 1 {
                continue;
            }
            let hole = missing[0];
            let mut buf = par.clone();
            let mut len = *xor_len;
            for &i in &members {
                if i == hole {
                    continue;
                }
                let d = self.slots[i].as_ref().unwrap();
                len ^= d.len() as u16;
                for (b, x) in buf.iter_mut().zip(d) {
                    *b ^= *x;
                }
            }
            let len = len as usize;
            if len > buf.len() {
                continue; // corrupt parity metadata; leave to NACK
            }
            buf.truncate(len);
            self.slots[hole] = Some(buf);
            self.received += 1;
            self.max_idx = self.max_idx.max(hole as u16);
            rebuilt += 1;
        }
        rebuilt
    }
}

pub struct Assembler {
    cfg: AssemblerConfig,
    epoch: Option<u16>,
    partials: BTreeMap<u32, Partial>,
    next_id: Option<u32>,
    highest_seen: Option<u32>,
    waiting_key: bool,
    key_req_due_us: Option<u64>,
    last_key_req_us: Option<u64>,
    first_activity_us: Option<u64>,
    counters: AssemblerCounters,
}

impl Assembler {
    pub fn new(cfg: AssemblerConfig) -> Self {
        Self {
            cfg,
            epoch: None,
            partials: BTreeMap::new(),
            next_id: None,
            highest_seen: None,
            waiting_key: true,
            key_req_due_us: None,
            last_key_req_us: None,
            first_activity_us: None,
            counters: AssemblerCounters::default(),
        }
    }

    pub fn counters(&self) -> AssemblerCounters {
        self.counters
    }
    pub fn set_nack_interval_us(&mut self, us: u64) {
        self.cfg.nack_interval_us = us.max(2_000);
    }
    pub fn buffered_frames(&self) -> usize {
        self.partials.len()
    }

    fn touch(&mut self, now: u64) {
        if self.first_activity_us.is_none() {
            self.first_activity_us = Some(now);
            self.key_req_due_us = Some(now + self.cfg.initial_keyframe_grace_us);
        }
    }

    fn reset_for_epoch(&mut self, epoch: u16) {
        self.counters.frames_abandoned += self.partials.len() as u64;
        self.partials.clear();
        self.epoch = Some(epoch);
        self.next_id = None;
        self.highest_seen = None;
        self.waiting_key = true;
        // The sender emits a keyframe with every config change; wait briefly for it.
        self.key_req_due_us = self.first_activity_us.map(|_| 0); // set properly in on_* via now
    }

    fn accept_epoch(&mut self, now: u64, epoch: u16) -> bool {
        match self.epoch {
            None => {
                self.epoch = Some(epoch);
                true
            }
            Some(e) if e == epoch => true,
            Some(e) if newer_u16(epoch, e) => {
                self.reset_for_epoch(epoch);
                self.key_req_due_us = Some(now + self.cfg.initial_keyframe_grace_us);
                true
            }
            Some(_) => false, // stale epoch
        }
    }

    /// Feed a decrypted video data packet.
    pub fn on_media(&mut self, now: u64, hdr: &MediaHeader, payload: &[u8]) {
        self.touch(now);
        if !self.accept_epoch(now, hdr.epoch) {
            return;
        }
        if let Some(n) = self.next_id {
            if !newer_u32(hdr.frame_id, n.wrapping_sub(1)) {
                return; // already delivered or abandoned
            }
        }
        if self.highest_seen.map_or(true, |h| newer_u32(hdr.frame_id, h)) {
            self.highest_seen = Some(hdr.frame_id);
        }
        if !self.partials.contains_key(&hdr.frame_id) {
            if self.partials.len() >= self.cfg.max_partial_frames {
                self.drop_oldest_partial();
            }
            self.partials.insert(hdr.frame_id, Partial::new(hdr.epoch, hdr.pkt_count, now));
        }
        let p = self.partials.get_mut(&hdr.frame_id).unwrap();
        if p.pkt_count != hdr.pkt_count {
            return; // inconsistent; ignore
        }
        if !p.have_meta {
            p.pts_us = hdr.pts_us;
            p.flags = hdr.flags & !FLAG_RETRANSMIT;
            p.have_meta = true;
        }
        let slot = &mut p.slots[hdr.pkt_idx as usize];
        if slot.is_some() {
            self.counters.duplicates += 1;
            return;
        }
        *slot = Some(payload.to_vec());
        p.received += 1;
        p.max_idx = p.max_idx.max(hdr.pkt_idx);
        if hdr.flags & FLAG_RETRANSMIT != 0 {
            self.counters.packets_recovered_retx += 1;
        }
        self.counters.packets_recovered_fec += p.try_fec();
    }

    /// Feed a decrypted FEC parity packet (`payload` includes the FEC header).
    pub fn on_fec(&mut self, now: u64, payload: &[u8]) {
        self.touch(now);
        let mut r = Reader::new(payload);
        let Ok(h) = FecHeader::decode(&mut r) else { return };
        if !self.accept_epoch(now, h.epoch) {
            return;
        }
        if let Some(n) = self.next_id {
            if !newer_u32(h.frame_id, n.wrapping_sub(1)) {
                return;
            }
        }
        if !self.partials.contains_key(&h.frame_id) {
            if self.partials.len() >= self.cfg.max_partial_frames {
                self.drop_oldest_partial();
            }
            self.partials.insert(h.frame_id, Partial::new(h.epoch, h.pkt_count, now));
        }
        let p = self.partials.get_mut(&h.frame_id).unwrap();
        if p.pkt_count != h.pkt_count {
            return;
        }
        if p.parity.is_empty() {
            p.groups = h.groups;
            p.parity = vec![None; h.groups as usize];
        }
        if p.groups != h.groups {
            return;
        }
        p.parity[h.group as usize] = Some((h.xor_len, r.rest().to_vec()));
        self.counters.packets_recovered_fec += p.try_fec();
    }

    fn drop_oldest_partial(&mut self) {
        if let Some((&id, _)) = self.partials.iter().next() {
            self.partials.remove(&id);
            self.counters.frames_abandoned += 1;
            self.waiting_key = true;
            self.key_req_due_us = Some(0);
        }
    }

    /// Advance time: deliver frames, abandon late ones, produce NACKs / keyframe requests.
    pub fn poll(&mut self, now: u64) -> AssemblerOutput {
        let mut out = AssemblerOutput::default();
        if self.first_activity_us.is_none() {
            return out;
        }

        loop {
            if self.waiting_key {
                if !self.resync_on_keyframe(now, &mut out) {
                    break;
                }
                continue;
            }
            let Some(next) = self.next_id else {
                self.waiting_key = true;
                continue;
            };
            // Complete head → deliver.
            if self.partials.get(&next).is_some_and(|p| p.complete()) {
                let p = self.partials.remove(&next).unwrap();
                out.frames.push(Self::finish(next, p, now));
                self.counters.frames_delivered += 1;
                self.next_id = Some(next.wrapping_add(1));
                continue;
            }
            // Head incomplete or entirely missing: has it run out of time?
            let head_age = match self.partials.get(&next) {
                Some(p) => Some(now.saturating_sub(p.first_seen_us)),
                // Never seen: age it from when the oldest later frame appeared.
                None => self.partials.values().map(|p| p.first_seen_us).min().map(|t| now.saturating_sub(t)),
            };
            let Some(age) = head_age else { break }; // nothing buffered at all
            let deadline = match self.partials.get(&next) {
                Some(p) if p.is_key() => self.cfg.keyframe_deadline_us,
                _ => self.cfg.frame_deadline_us,
            };
            if age > deadline {
                self.partials.remove(&next);
                self.counters.frames_abandoned += 1;
                self.next_id = Some(next.wrapping_add(1));
                self.waiting_key = true;
                self.key_req_due_us = Some(now);
                log::debug!("frame {next} abandoned after {age} us");
                continue;
            }
            break;
        }

        self.collect_nacks(now, &mut out);

        if self.waiting_key {
            if let Some(due) = self.key_req_due_us {
                let retry_ok = self.last_key_req_us.map_or(true, |t| now.saturating_sub(t) >= self.cfg.keyframe_retry_us);
                if now >= due && retry_ok {
                    self.last_key_req_us = Some(now);
                    self.counters.keyframe_requests += 1;
                    out.keyframe_request = Some(if self.counters.frames_delivered == 0 {
                        KeyframeReason::StreamStart
                    } else {
                        KeyframeReason::PacketLoss
                    });
                }
            }
        }
        out
    }

    /// While the reference chain is broken: discard deltas, deliver a complete
    /// keyframe if one is buffered. Returns true if state changed (call again).
    fn resync_on_keyframe(&mut self, now: u64, out: &mut AssemblerOutput) -> bool {
        // Newest complete keyframe wins; everything older is useless.
        let key_id = self.partials.iter().rev().find(|(_, p)| p.complete() && p.is_key()).map(|(&id, _)| id);
        if let Some(id) = key_id {
            let older: Vec<u32> = self.partials.range(..id).map(|(&k, _)| k).collect();
            for k in older {
                self.partials.remove(&k);
                self.counters.frames_discarded += 1;
            }
            let p = self.partials.remove(&id).unwrap();
            out.frames.push(Self::finish(id, p, now));
            self.counters.frames_delivered += 1;
            self.next_id = Some(id.wrapping_add(1));
            self.waiting_key = false;
            self.last_key_req_us = None;
            return true;
        }
        // Throw away complete deltas and hopelessly late partials.
        let mut changed = false;
        let doomed: Vec<u32> = self
            .partials
            .iter()
            .filter(|(_, p)| {
                let late = now.saturating_sub(p.first_seen_us)
                    > if p.is_key() { self.cfg.keyframe_deadline_us } else { self.cfg.frame_deadline_us };
                (p.complete() && !p.is_key()) || (late && !p.is_key())
            })
            .map(|(&k, _)| k)
            .collect();
        for k in doomed {
            let p = self.partials.remove(&k).unwrap();
            if p.complete() {
                self.counters.frames_discarded += 1;
            } else {
                self.counters.frames_abandoned += 1;
            }
            changed = true;
        }
        // An incomplete keyframe that ran out of time must also go, or it would stay forever.
        let stale_keys: Vec<u32> = self
            .partials
            .iter()
            .filter(|(_, p)| p.is_key() && !p.complete() && now.saturating_sub(p.first_seen_us) > self.cfg.keyframe_deadline_us)
            .map(|(&k, _)| k)
            .collect();
        for k in stale_keys {
            self.partials.remove(&k);
            self.counters.frames_abandoned += 1;
            changed = true;
        }
        changed
    }

    fn collect_nacks(&mut self, now: u64, out: &mut AssemblerOutput) {
        let highest = self.highest_seen;
        let (delay, interval, max) = (self.cfg.nack_delay_us, self.cfg.nack_interval_us, self.cfg.max_nacks);
        for (&id, p) in self.partials.iter_mut() {
            if p.complete() || p.nacks >= max {
                continue;
            }
            let age = now.saturating_sub(p.first_seen_us);
            if age < delay || (p.nacks > 0 && now.saturating_sub(p.last_nack_us) < interval) {
                continue;
            }
            let later_frame_seen = highest.is_some_and(|h| newer_u32(h, id));
            let missing: Vec<u16> = p
                .missing()
                .into_iter()
                .filter(|&i| i < p.max_idx || later_frame_seen || age >= delay * 3)
                .take(255)
                .collect();
            if missing.is_empty() {
                continue;
            }
            p.nacks += 1;
            p.last_nack_us = now;
            self.counters.nacks_sent += 1;
            out.nacks.push(NackRequest { epoch: p.epoch, frame_id: id, missing });
        }
    }

    fn finish(id: u32, p: Partial, now: u64) -> ReceivedFrame {
        let total: usize = p.slots.iter().map(|s| s.as_ref().map_or(0, |v| v.len())).sum();
        let mut data = Vec::with_capacity(total);
        for s in p.slots.iter().flatten() {
            data.extend_from_slice(s);
        }
        ReceivedFrame {
            epoch: p.epoch,
            frame_id: id,
            pts_us: p.pts_us,
            keyframe: p.flags & FLAG_KEYFRAME != 0,
            data,
            first_packet_us: p.first_seen_us,
            complete_us: now,
        }
    }

    /// Called by the session after it asked the sender for a keyframe on its own
    /// (e.g. decoder error) so the retry timer starts from now.
    pub fn note_keyframe_requested(&mut self, now: u64) {
        self.waiting_key = true;
        self.last_key_req_us = Some(now);
        self.counters.keyframe_requests += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packetizer::{EncodedFrame, PacketizedFrame};

    const P: usize = 100;

    fn frame(id: u32, key: bool, len: usize) -> EncodedFrame {
        EncodedFrame {
            epoch: 1,
            frame_id: id,
            pts_us: id as u64 * 16_667,
            flags: if key { FLAG_KEYFRAME } else { 0 },
            data: (0..len).map(|i| (i as u32 ^ id.wrapping_mul(31)) as u8).collect(),
        }
    }

    fn feed_data(a: &mut Assembler, now: u64, pf: &PacketizedFrame, idx: u16, retx: bool) {
        let (h, body) = pf.data_packet(idx, retx).unwrap();
        let hdr = MediaHeader::decode(&mut Reader::new(&h)).unwrap();
        a.on_media(now, &hdr, body);
    }

    fn feed_fec(a: &mut Assembler, now: u64, pf: &PacketizedFrame) {
        for (h, body) in pf.fec_packets() {
            let mut v = h;
            v.extend_from_slice(body);
            a.on_fec(now, &v);
        }
    }

    fn feed_all_but(a: &mut Assembler, now: u64, pf: &PacketizedFrame, skip: &[u16]) {
        for i in 0..pf.pkt_count {
            if !skip.contains(&i) {
                feed_data(a, now, pf, i, false);
            }
        }
        feed_fec(a, now, pf);
    }

    fn asm() -> Assembler {
        Assembler::new(AssemblerConfig { initial_keyframe_grace_us: 0, ..Default::default() })
    }

    #[test]
    fn in_order_delivery_and_content_intact() {
        let mut a = asm();
        for id in 0..5 {
            let pf = PacketizedFrame::with_payload(frame(id, id == 0, 1000 + id as usize), 0, P);
            feed_all_but(&mut a, id as u64 * 16_000, &pf, &[]);
            let out = a.poll(id as u64 * 16_000);
            assert_eq!(out.frames.len(), 1);
            assert_eq!(out.frames[0].data, pf.frame.data);
            assert_eq!(out.frames[0].keyframe, id == 0);
        }
        assert_eq!(a.counters().frames_delivered, 5);
    }

    #[test]
    fn reordered_packets_ok() {
        let mut a = asm();
        let pf = PacketizedFrame::with_payload(frame(0, true, 1000), 0, P);
        for i in (0..pf.pkt_count).rev() {
            feed_data(&mut a, 0, &pf, i, false);
        }
        let out = a.poll(0);
        assert_eq!(out.frames[0].data, pf.frame.data);
    }

    #[test]
    fn duplicates_ignored() {
        let mut a = asm();
        let pf = PacketizedFrame::with_payload(frame(0, true, 300), 0, P);
        feed_data(&mut a, 0, &pf, 0, false);
        feed_data(&mut a, 0, &pf, 0, false);
        assert_eq!(a.counters().duplicates, 1);
    }

    #[test]
    fn single_loss_recovered_by_fec_without_nack() {
        let mut a = asm();
        let pf = PacketizedFrame::with_payload(frame(0, true, 2000), 5, P); // 20 pkts, 4 groups
        feed_all_but(&mut a, 0, &pf, &[7]);
        let out = a.poll(10_000);
        assert_eq!(out.frames.len(), 1);
        assert_eq!(out.frames[0].data, pf.frame.data);
        assert!(out.nacks.is_empty());
        assert_eq!(a.counters().packets_recovered_fec, 1);
    }

    #[test]
    fn burst_loss_recovered_thanks_to_interleaving() {
        let mut a = asm();
        let pf = PacketizedFrame::with_payload(frame(0, true, 2000), 5, P); // 4 groups
        // 4 consecutive packets lost: one per group
        feed_all_but(&mut a, 0, &pf, &[8, 9, 10, 11]);
        let out = a.poll(10_000);
        assert_eq!(out.frames[0].data, pf.frame.data);
        assert_eq!(a.counters().packets_recovered_fec, 4);
    }

    #[test]
    fn last_short_packet_recovered_with_exact_length() {
        let mut a = asm();
        let pf = PacketizedFrame::with_payload(frame(0, true, 1950), 5, P); // last packet 50 bytes
        let last = pf.pkt_count - 1;
        feed_all_but(&mut a, 0, &pf, &[last]);
        assert_eq!(a.poll(5_000).frames[0].data, pf.frame.data);
    }

    #[test]
    fn two_losses_in_one_group_use_nack_retransmit() {
        let mut a = asm();
        let pf = PacketizedFrame::with_payload(frame(0, true, 2000), 5, P);
        // packets 3 and 7 are both in group 3 (3 % 4 == 7 % 4)
        feed_all_but(&mut a, 0, &pf, &[3, 7]);
        let out = a.poll(1_000);
        assert!(out.frames.is_empty() && out.nacks.is_empty()); // inside nack_delay
        let out = a.poll(3_000);
        assert_eq!(out.nacks.len(), 1);
        assert_eq!(out.nacks[0].missing, vec![3, 7]);
        // retransmission arrives
        feed_data(&mut a, 4_000, &pf, 3, true);
        // (the first retransmit leaves one hole in the group, so FEC rebuilds the other)
        let out = a.poll(4_000);
        assert_eq!(out.frames[0].data, pf.frame.data);
        assert_eq!(a.counters().packets_recovered_retx, 1);
        assert_eq!(a.counters().packets_recovered_fec, 1);
    }

    #[test]
    fn nack_is_rate_limited_and_bounded() {
        let mut a = asm();
        let pf = PacketizedFrame::with_payload(frame(0, true, 1000), 0, P);
        feed_all_but(&mut a, 0, &pf, &[2]);
        let mut total = 0;
        for t in (3_000..140_000).step_by(1_000) {
            total += a.poll(t).nacks.len();
        }
        assert_eq!(total, 4); // max_nacks
    }

    #[test]
    fn lost_delta_frame_triggers_keyframe_request_and_discards_until_key() {
        let mut a = asm();
        let k = PacketizedFrame::with_payload(frame(0, true, 500), 0, P);
        feed_all_but(&mut a, 0, &k, &[]);
        assert_eq!(a.poll(0).frames.len(), 1);

        // frame 1 loses a packet permanently; frames 2,3 arrive complete
        let f1 = PacketizedFrame::with_payload(frame(1, false, 500), 0, P);
        feed_all_but(&mut a, 16_000, &f1, &[1]);
        for id in 2..4 {
            let f = PacketizedFrame::with_payload(frame(id, false, 300), 0, P);
            feed_all_but(&mut a, 16_000 + id as u64 * 1000, &f, &[]);
        }
        // before the deadline nothing is delivered (waiting for retransmit)
        assert!(a.poll(20_000).frames.is_empty());
        // past the deadline: frame 1 abandoned → key request, deltas dropped
        let out = a.poll(16_000 + 61_000);
        assert!(out.frames.is_empty());
        assert_eq!(out.keyframe_request, Some(KeyframeReason::PacketLoss));
        assert_eq!(a.counters().frames_abandoned, 1);
        assert_eq!(a.counters().frames_discarded, 2);
        assert_eq!(a.buffered_frames(), 0);

        // keyframe request is repeated, not spammed
        assert!(a.poll(16_000 + 100_000).keyframe_request.is_none());
        assert!(a.poll(16_000 + 61_000 + 250_000).keyframe_request.is_some());

        // new keyframe resumes
        let k2 = PacketizedFrame::with_payload(frame(10, true, 500), 0, P);
        feed_all_but(&mut a, 400_000, &k2, &[]);
        let out = a.poll(400_000);
        assert_eq!(out.frames.len(), 1);
        assert!(out.frames[0].keyframe);
        // subsequent delta decodes normally
        let d = PacketizedFrame::with_payload(frame(11, false, 200), 0, P);
        feed_all_but(&mut a, 416_000, &d, &[]);
        assert_eq!(a.poll(416_000).frames.len(), 1);
    }

    #[test]
    fn entirely_lost_frame_detected_by_gap() {
        let mut a = asm();
        let k = PacketizedFrame::with_payload(frame(0, true, 300), 0, P);
        feed_all_but(&mut a, 0, &k, &[]);
        a.poll(0);
        // frame 1 never arrives at all; frame 2 does
        let f2 = PacketizedFrame::with_payload(frame(2, false, 300), 0, P);
        feed_all_but(&mut a, 16_000, &f2, &[]);
        assert!(a.poll(20_000).frames.is_empty());
        let out = a.poll(16_000 + 61_000);
        assert_eq!(out.keyframe_request, Some(KeyframeReason::PacketLoss));
    }

    #[test]
    fn epoch_change_resyncs_on_new_keyframe() {
        let mut a = asm();
        let k = PacketizedFrame::with_payload(frame(0, true, 300), 0, P);
        feed_all_but(&mut a, 0, &k, &[]);
        assert_eq!(a.poll(0).frames.len(), 1);
        let mut e2 = frame(1, true, 400);
        e2.epoch = 2;
        let pf = PacketizedFrame::with_payload(e2, 0, P);
        feed_all_but(&mut a, 1000, &pf, &[]);
        let out = a.poll(1000);
        assert_eq!(out.frames.len(), 1);
        assert_eq!(out.frames[0].epoch, 2);
        // late packet of the old epoch is ignored
        feed_data(&mut a, 2000, &k, 0, false);
        assert_eq!(a.buffered_frames(), 0);
    }

    #[test]
    fn initial_grace_defers_first_keyframe_request() {
        let mut a = Assembler::new(AssemblerConfig::default());
        let d = PacketizedFrame::with_payload(frame(5, false, 300), 0, P); // joined mid-GOP
        feed_all_but(&mut a, 0, &d, &[]);
        assert!(a.poll(100_000).keyframe_request.is_none());
        assert_eq!(a.poll(400_000).keyframe_request, Some(KeyframeReason::StreamStart));
        assert_eq!(a.counters().frames_delivered, 0);
    }

    #[test]
    fn memory_is_bounded() {
        let mut a = Assembler::new(AssemblerConfig { max_partial_frames: 8, ..Default::default() });
        for id in 0..100u32 {
            let pf = PacketizedFrame::with_payload(frame(id, false, 300), 0, P);
            feed_data(&mut a, id as u64, &pf, 0, false); // never completes
            assert!(a.buffered_frames() <= 8);
        }
    }
}
