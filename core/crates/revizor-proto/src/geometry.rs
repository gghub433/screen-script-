//! Resolution maths shared by all senders.

/// Scales `(src_w, src_h)` so the *shorter* side equals `short` (never upscales
/// beyond the source), preserving aspect ratio, rounded down to `align`.
/// This is what keeps portrait phones and ultrawide monitors undistorted.
pub fn fit_short_side(src_w: u16, src_h: u16, short: u16, align: u16) -> (u16, u16) {
    let align = align.max(1) as u32;
    let (sw, sh) = (src_w.max(1) as u32, src_h.max(1) as u32);
    let src_short = sw.min(sh);
    let target = (short as u32).min(src_short);
    let w = sw * target / src_short;
    let h = sh * target / src_short;
    let a = |v: u32| ((v / align) * align).max(align) as u16;
    (a(w), a(h))
}

/// Letterbox rectangle (x, y, w, h) that shows a `(vw, vh)` video inside a
/// `(sw, sh)` surface without stretching.
pub fn letterbox(vw: u32, vh: u32, sw: u32, sh: u32) -> (u32, u32, u32, u32) {
    if vw == 0 || vh == 0 || sw == 0 || sh == 0 {
        return (0, 0, sw, sh);
    }
    // Compare vw/vh with sw/sh via cross-multiplication (no floats).
    if (vw as u64) * (sh as u64) >= (vh as u64) * (sw as u64) {
        let h = (vh as u64 * sw as u64 / vw as u64) as u32;
        (0, (sh - h) / 2, sw, h)
    } else {
        let w = (vw as u64 * sh as u64 / vh as u64) as u32;
        ((sw - w) / 2, 0, w, sh)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_16_9() {
        assert_eq!(fit_short_side(2560, 1440, 1080, 2), (1920, 1080));
        assert_eq!(fit_short_side(1920, 1080, 720, 2), (1280, 720));
    }
    #[test]
    fn tall_phone() {
        assert_eq!(fit_short_side(1440, 3200, 1080, 2), (1080, 2400));
        assert_eq!(fit_short_side(1080, 2400, 720, 16), (720, 1600));
    }
    #[test]
    fn never_upscales() {
        assert_eq!(fit_short_side(1280, 720, 1440, 2), (1280, 720));
    }
    #[test]
    fn letterbox_pillar_and_letter() {
        assert_eq!(letterbox(1080, 2400, 1920, 1080), (717, 0, 486, 1080));
        assert_eq!(letterbox(1920, 1080, 1080, 1920), (0, 656, 1080, 607));
        assert_eq!(letterbox(1920, 1080, 1920, 1080), (0, 0, 1920, 1080));
    }
}
