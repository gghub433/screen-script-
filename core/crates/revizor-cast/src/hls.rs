//! Live HLS segmenter over the TS muxer output: segments are cut only at keyframes, a bounded window of
//! recent segments is kept (so memory is constant), and the playlist is generated on demand.

use std::collections::VecDeque;
use std::sync::Arc;

#[derive(Clone)]
pub struct Segment {
    pub seq: u64,
    pub duration_us: u64,
    pub data: Arc<Vec<u8>>,
}

pub struct Segmenter {
    target_us: u64,
    window: usize,
    min_ready: usize,
    next_seq: u64,
    cur: Vec<u8>,
    cur_start_pts: Option<u64>,
    segs: VecDeque<Segment>,
    pub segments_made: u64,
}

/// A segment that never ends (encoder that never emits a keyframe) must not eat all memory.
const MAX_SEGMENT_BYTES: usize = 24 << 20;

impl Segmenter {
    pub fn new(target_us: u64, window: usize, min_ready: usize) -> Self {
        Self { target_us, window: window.max(3), min_ready: min_ready.max(1), next_seq: 0, cur: Vec::new(), cur_start_pts: None, segs: VecDeque::new(), segments_made: 0 }
    }

    /// `ts` = the TS packets of one video access unit (PSI + PES). Cut *before* a keyframe once the segment is long enough.
    pub fn push_video(&mut self, pts_us: u64, keyframe: bool, ts: &[u8]) {
        if ts.is_empty() {
            return;
        }
        match self.cur_start_pts {
            None => {
                if !keyframe {
                    return;
                }
                self.cur_start_pts = Some(pts_us);
            }
            Some(start) => {
                let enough = pts_us.saturating_sub(start) + self.target_us / 10 >= self.target_us;
                if keyframe && enough {
                    let dur = pts_us.saturating_sub(start).max(1);
                    let data = Arc::new(std::mem::take(&mut self.cur));
                    self.segs.push_back(Segment { seq: self.next_seq, duration_us: dur, data });
                    self.next_seq += 1;
                    self.segments_made += 1;
                    while self.segs.len() > self.window {
                        self.segs.pop_front();
                    }
                    self.cur_start_pts = Some(pts_us);
                } else if self.cur.len() > MAX_SEGMENT_BYTES {
                    log::warn!("HLS segment exceeded {MAX_SEGMENT_BYTES} bytes without a keyframe; restarting it");
                    self.cur.clear();
                    self.cur_start_pts = None;
                    return;
                }
            }
        }
        self.cur.extend_from_slice(ts);
    }

    /// Audio / keep-alive packets belong to whatever segment is currently being built.
    pub fn push_other(&mut self, ts: &[u8]) {
        if self.cur_start_pts.is_some() && self.cur.len() <= MAX_SEGMENT_BYTES {
            self.cur.extend_from_slice(ts);
        }
    }

    pub fn ready(&self) -> bool {
        self.segs.len() >= self.min_ready
    }

    /// `prefix` is prepended to segment URIs (empty = relative).
    pub fn playlist(&self) -> Option<String> {
        if !self.ready() {
            return None;
        }
        let max = self.segs.iter().map(|s| s.duration_us).max().unwrap_or(self.target_us);
        let target = ((max + 999_999) / 1_000_000).max(1);
        let mut s = String::from("#EXTM3U\n#EXT-X-VERSION:3\n");
        s.push_str(&format!("#EXT-X-TARGETDURATION:{target}\n#EXT-X-MEDIA-SEQUENCE:{}\n", self.segs.front().unwrap().seq));
        for seg in &self.segs {
            s.push_str(&format!("#EXTINF:{:.3},\nseg-{}.ts\n", seg.duration_us as f64 / 1e6, seg.seq));
        }
        Some(s)
    }

    pub fn segment(&self, seq: u64) -> Option<Arc<Vec<u8>>> {
        self.segs.iter().find(|s| s.seq == seq).map(|s| s.data.clone())
    }

    pub fn buffered_bytes(&self) -> usize {
        self.cur.len() + self.segs.iter().map(|s| s.data.len()).sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(s: &mut Segmenter, frames: u64, fps: u64, gop: u64) {
        for i in 0..frames {
            let pts = i * 1_000_000 / fps;
            s.push_video(pts, i % gop == 0, &vec![(i % 251) as u8; 188]);
        }
    }

    #[test]
    fn cuts_only_at_keyframes_and_reports_real_durations() {
        let mut s = Segmenter::new(1_000_000, 5, 2);
        feed(&mut s, 150, 30, 30); // 5 s, IDR every second
        assert_eq!(s.segments_made, 4); // the fifth is still open
        let p = s.playlist().unwrap();
        assert!(p.starts_with("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:0\n"), "{p}");
        assert_eq!(p.matches("#EXTINF:1.000,").count(), 4);
        assert!(!p.contains("ENDLIST"));
        // each segment starts with a keyframe packet and holds exactly 30 frames
        assert_eq!(s.segment(0).unwrap().len(), 30 * 188);
    }

    #[test]
    fn long_gop_gives_long_segments_and_target_duration_follows() {
        let mut s = Segmenter::new(1_000_000, 5, 1);
        feed(&mut s, 200, 30, 90); // IDR every 3 s
        let p = s.playlist().unwrap();
        assert!(p.contains("#EXT-X-TARGETDURATION:3"), "{p}");
    }

    #[test]
    fn window_is_bounded_and_sequence_advances() {
        let mut s = Segmenter::new(1_000_000, 4, 2);
        feed(&mut s, 30 * 20, 30, 30);
        let p = s.playlist().unwrap();
        assert_eq!(p.matches("#EXTINF").count(), 4);
        assert!(p.contains("#EXT-X-MEDIA-SEQUENCE:15"), "{p}");
        assert!(s.segment(0).is_none() && s.segment(18).is_some());
        assert!(s.buffered_bytes() < 6 * 30 * 188 + 1000);
    }

    #[test]
    fn not_ready_until_enough_segments_and_never_starts_mid_gop() {
        let mut s = Segmenter::new(1_000_000, 5, 2);
        s.push_video(0, false, &[1; 188]);
        assert_eq!(s.buffered_bytes(), 0);
        feed(&mut s, 45, 30, 30);
        assert!(s.playlist().is_none());
        feed(&mut s, 30, 30, 30);
        // (feed restarts pts at 0 which counts as a new keyframe stream; only readiness matters here)
        assert!(s.segments_made >= 1);
    }

    #[test]
    fn runaway_segment_is_capped() {
        let mut s = Segmenter::new(1_000_000, 5, 1);
        s.push_video(0, true, &[0; 188]);
        let big = vec![0u8; 1 << 20];
        for i in 1..40u64 {
            s.push_video(i, false, &big);
        }
        assert!(s.buffered_bytes() <= MAX_SEGMENT_BYTES + (1 << 20));
    }
}
