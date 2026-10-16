//! Whether channels are mixed on the way to an output, so that users see
//! when a stream is played on other channels than it was made for.
//!
//! A stream placed on an output is mixed by the audio system to the
//! output's channels by itself (PipeWire up- and downmixes by position);
//! a wire links the channel pairs Orpheus chose (see
//! [`wires`](crate::wires)).

use orpheus_core::Layout;
use serde::Serialize;
use utoipa::ToSchema;

/// How channels are mixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Mix {
    /// Channels are summed onto fewer, or left out.
    Downmix,
    /// Channels are spread onto more.
    Upmix,
    /// As many channels, but other ones.
    Remix,
}

/// What the audio system does to play `from` on an output with `to`.
pub fn placed(from: &Layout, to: &Layout) -> Option<Mix> {
    let count = |l: &Layout, s| l.symbols().filter(|&t| t == s).count();
    if from.len() > to.len() {
        Some(Mix::Downmix)
    } else if from.len() < to.len() {
        Some(Mix::Upmix)
    } else if from.symbols().any(|s| count(from, s) != count(to, s)) {
        Some(Mix::Remix)
    } else {
        None
    }
}

/// What a wire linking `pairs` (input then output channel) does.
pub struct Wired {
    pub mix: Option<Mix>,
    /// Per pair: whether it plays a channel on another one as part of a
    /// mix, such as the centre on the left.
    pub folded: Vec<bool>,
    /// The input channels left out, for a wire of all channels; none for
    /// one of chosen pairs, whose user left them out.
    pub dropped: Vec<usize>,
}

pub fn wired(pairs: &[(usize, usize)], from: &Layout, to: &Layout, all: bool) -> Wired {
    let count =
        |f: &dyn Fn(&(usize, usize)) -> usize, i: usize| pairs.iter().filter(|p| f(p) == i).count();
    let fan_in = |t: usize| count(&|p| p.1, t);
    let fan_out = |f: usize| count(&|p| p.0, f);
    let symbol = |l: &Layout, i: usize| l.channels().get(i).map(|c| c.symbol);
    let folded = pairs
        .iter()
        .map(|&(f, t)| symbol(from, f) != symbol(to, t) && (fan_in(t) > 1 || fan_out(f) > 1))
        .collect();
    let dropped: Vec<usize> = if all && !pairs.is_empty() {
        (0..from.len()).filter(|&f| fan_out(f) == 0).collect()
    } else {
        Vec::new()
    };
    let mix = if pairs.iter().any(|&(_, t)| fan_in(t) > 1) || !dropped.is_empty() {
        Some(Mix::Downmix)
    } else if pairs.iter().any(|&(f, _)| fan_out(f) > 1) {
        Some(Mix::Upmix)
    } else {
        None
    };
    Wired {
        mix,
        folded,
        dropped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wires::auto_pairs;

    fn layout(s: &str) -> Layout {
        Layout::parse(s).unwrap()
    }

    #[test]
    fn placed_mix() {
        assert_eq!(placed(&layout("ST"), &layout("ST")), None);
        assert_eq!(placed(&layout("R L"), &layout("ST")), None);
        assert_eq!(placed(&layout("51"), &layout("ST")), Some(Mix::Downmix));
        assert_eq!(placed(&layout("M"), &layout("ST")), Some(Mix::Upmix));
        assert_eq!(placed(&layout("DM"), &layout("ST")), Some(Mix::Remix));
    }

    #[test]
    fn wired_mix() {
        let (from, to) = (layout("51"), layout("ST"));
        let w = wired(&auto_pairs(&from, &to), &from, &to, true);
        assert_eq!(w.mix, Some(Mix::Downmix));
        // L R C C Ls Rs: only the front left and right stay where they are.
        assert_eq!(w.folded, [false, false, true, true, true, true]);
        assert_eq!(w.dropped, [3]);

        let (from, to) = (layout("M"), layout("ST"));
        let w = wired(&auto_pairs(&from, &to), &from, &to, true);
        assert_eq!(w.mix, Some(Mix::Upmix));
        assert_eq!(w.folded, [true, true]);

        // Stereo on the front of 5.1, and channels by number: no mix.
        let (from, to) = (layout("ST"), layout("51"));
        let w = wired(&auto_pairs(&from, &to), &from, &to, true);
        assert_eq!(w.mix, None);
        let (from, to) = (layout("DM"), layout("ST"));
        let w = wired(&auto_pairs(&from, &to), &from, &to, true);
        assert_eq!((w.mix, w.folded), (None, vec![false, false]));

        // One chosen channel of four: the rest left out on purpose.
        let (from, to) = (layout("U04"), layout("M"));
        let w = wired(&[(2, 0)], &from, &to, false);
        assert_eq!((w.mix, w.dropped), (None, vec![]));
    }
}
