//! Channel layouts: what each channel of a stream carries.
//!
//! Channels are named by the NMOS channel symbols (VSF TR-03 Appendix A),
//! which IS-04 announces for a source, each with a free label: `L`, `R`,
//! `LFE`, or `U03` labelled "Mic 3". SDP files carry the layout as the
//! SMPTE ST 2110-30 channel grouping, `channel-order=SMPTE2110.(51,ST)`,
//! which knows fewer layouts; channels that fit none of its groups go as
//! undefined ones.

use core::fmt;
use core::str::FromStr;

/// What a channel carries, as an NMOS channel symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Symbol {
    /// Left.
    L,
    /// Right.
    R,
    /// Center.
    C,
    /// Low-frequency effects.
    Lfe,
    /// Left surround.
    Ls,
    /// Right surround.
    Rs,
    /// Left side surround.
    Lss,
    /// Right side surround.
    Rss,
    /// Left rear surround.
    Lrs,
    /// Right rear surround.
    Rrs,
    /// Left center.
    Lc,
    /// Right center.
    Rc,
    /// Center surround.
    Cs,
    /// Hearing impaired.
    Hi,
    /// Visually impaired narrative.
    Vin,
    /// Mono, or the first of two.
    M1,
    /// The second of two mono channels.
    M2,
    /// Left total (matrix encoded).
    Lt,
    /// Right total.
    Rt,
    /// Left surround total.
    Lst,
    /// Right surround total.
    Rst,
    /// Surround.
    S,
    /// Numbered source channel, 0 to 128.
    Nsc(u8),
    /// Undefined channel, 1 to 64.
    U(u8),
}

impl Symbol {
    const NAMED: [(Self, &'static str, &'static str); 22] = [
        (Self::L, "L", "Left"),
        (Self::R, "R", "Right"),
        (Self::C, "C", "Center"),
        (Self::Lfe, "LFE", "LFE"),
        (Self::Ls, "Ls", "Left surround"),
        (Self::Rs, "Rs", "Right surround"),
        (Self::Lss, "Lss", "Left side surround"),
        (Self::Rss, "Rss", "Right side surround"),
        (Self::Lrs, "Lrs", "Left rear surround"),
        (Self::Rrs, "Rrs", "Right rear surround"),
        (Self::Lc, "Lc", "Left center"),
        (Self::Rc, "Rc", "Right center"),
        (Self::Cs, "Cs", "Center surround"),
        (Self::Hi, "HI", "Hearing impaired"),
        (Self::Vin, "VIN", "Visually impaired narrative"),
        (Self::M1, "M1", "Mono"),
        (Self::M2, "M2", "Mono 2"),
        (Self::Lt, "Lt", "Left total"),
        (Self::Rt, "Rt", "Right total"),
        (Self::Lst, "Lst", "Left surround total"),
        (Self::Rst, "Rst", "Right surround total"),
        (Self::S, "S", "Surround"),
    ];

    /// The name to show for a channel with no label, if the symbol has
    /// one; numbered and undefined channels are named by their position.
    pub fn name(self) -> Option<&'static str> {
        Self::NAMED
            .iter()
            .find(|(s, ..)| *s == self)
            .map(|(.., name)| *name)
    }

    pub fn is_undefined(self) -> bool {
        matches!(self, Self::U(_))
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Nsc(n) => write!(f, "NSC{n:03}"),
            Self::U(n) => write!(f, "U{n:02}"),
            named => {
                let (_, symbol, _) = Self::NAMED.iter().find(|(s, ..)| s == named).unwrap();
                f.write_str(symbol)
            }
        }
    }
}

impl FromStr for Symbol {
    type Err = UnknownSymbol;

    fn from_str(s: &str) -> Result<Self, UnknownSymbol> {
        if let Some((symbol, ..)) = Self::NAMED.iter().find(|(_, name, _)| *name == s) {
            return Ok(*symbol);
        }
        let number = |digits: &str, len: usize| {
            (digits.len() == len && digits.bytes().all(|b| b.is_ascii_digit()))
                .then(|| digits.parse::<u8>().ok())
                .flatten()
        };
        if let Some(n) = s.strip_prefix("NSC").and_then(|d| number(d, 3))
            && n <= 128
        {
            return Ok(Self::Nsc(n));
        }
        if let Some(n) = s.strip_prefix('U').and_then(|d| number(d, 2))
            && (1..=64).contains(&n)
        {
            return Ok(Self::U(n));
        }
        Err(UnknownSymbol)
    }
}

/// Not an NMOS channel symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownSymbol;

impl fmt::Display for UnknownSymbol {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(
            "not a channel symbol: L, R, C, LFE, Ls, Rs, Lss, Rss, Lrs, Rrs, Lc, Rc, Cs, HI, \
             VIN, M1, M2, Lt, Rt, Lst, Rst, S, NSC000 to NSC128 or U01 to U64",
        )
    }
}

/// The ST 2110-30 groups with the symbols of their channels, except the
/// undefined `Uxx`. `222` (22.2) and `SGRP` (an SDI group) have no NMOS
/// symbols; their channels are undefined.
const GROUPS: [(&str, &[Symbol]); 6] = {
    use Symbol::*;
    [
        ("71", &[L, R, C, Lfe, Lss, Rss, Lrs, Rrs]),
        ("51", &[L, R, C, Lfe, Ls, Rs]),
        ("ST", &[L, R]),
        ("LtRt", &[Lt, Rt]),
        ("DM", &[M1, M2]),
        ("M", &[M1]),
    ]
};

/// Channels of the groups that have no symbols.
const UNDEFINED_GROUPS: [(&str, usize); 2] = [("222", 24), ("SGRP", 4)];

/// The ST 2110-30 grouping of channels with these `symbols`: `51,ST`.
/// Runs of channels that fit no group go as one undefined group, `U02`.
/// Writes nothing for no channels.
pub fn write_channel_order(symbols: &[Symbol], out: &mut impl fmt::Write) -> fmt::Result {
    let mut rest = symbols;
    let mut undefined = 0;
    let mut first = true;
    let mut sep = |out: &mut dyn fmt::Write| {
        let r = if first { Ok(()) } else { out.write_char(',') };
        first = false;
        r
    };
    while !rest.is_empty() {
        let group = GROUPS.iter().find(|(_, g)| rest.starts_with(g));
        if let Some((name, g)) = group {
            if undefined > 0 {
                sep(out)?;
                write!(out, "U{undefined:02}")?;
                undefined = 0;
            }
            sep(out)?;
            out.write_str(name)?;
            rest = &rest[g.len()..];
        } else {
            // A group holds at most 64.
            if undefined == 64 {
                sep(out)?;
                out.write_str("U64")?;
                undefined = 0;
            }
            undefined += 1;
            rest = &rest[1..];
        }
    }
    if undefined > 0 {
        sep(out)?;
        write!(out, "U{undefined:02}")?;
    }
    Ok(())
}

/// Reads an ST 2110-30 grouping, `SMPTE2110.(51,ST)` or just `51,ST`,
/// calling `channel` with the symbol of each channel in turn; the number
/// of channels, or `None` if it isn't one. Undefined channels are
/// numbered from 1 across the layout, `U01`, `U02`, …
pub fn read_channel_order(order: &str, mut channel: impl FnMut(Symbol)) -> Option<usize> {
    let order = order.trim();
    let groups = match order.strip_prefix("SMPTE2110.") {
        Some(g) => g.strip_prefix('(')?.strip_suffix(')')?,
        None => order,
    };
    let mut count = 0;
    let mut undefined = 0u8;
    let mut add_undefined = |n: usize, count: &mut usize, channel: &mut dyn FnMut(Symbol)| {
        for _ in 0..n {
            undefined = undefined.saturating_add(1);
            channel(Symbol::U(undefined.min(64)));
        }
        *count += n;
    };
    for group in groups.split(',').map(str::trim) {
        if let Some((_, symbols)) = GROUPS.iter().find(|(name, _)| *name == group) {
            symbols.iter().for_each(|&s| channel(s));
            count += symbols.len();
        } else if let Some((_, n)) = UNDEFINED_GROUPS.iter().find(|(name, _)| *name == group) {
            add_undefined(*n, &mut count, &mut channel);
        } else {
            let n = group.strip_prefix('U')?;
            if n.len() != 2 || !n.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let n: usize = n.parse().ok()?;
            if !(1..=64).contains(&n) {
                return None;
            }
            add_undefined(n, &mut count, &mut channel);
        }
    }
    Some(count)
}

/// The symbols to assume for `channels` channels whose sender doesn't say:
/// mono, stereo, 5.1 and 7.1 by their count, else undefined channels.
pub fn default_symbols(channels: usize, mut channel: impl FnMut(Symbol)) {
    match GROUPS
        .iter()
        .filter(|(name, _)| matches!(*name, "M" | "ST" | "51" | "71"))
        .find(|(_, g)| g.len() == channels)
    {
        Some((_, g)) => g.iter().for_each(|&s| channel(s)),
        None => (1..=channels).for_each(|i| channel(Symbol::U(i.min(64) as u8))),
    }
}

#[cfg(feature = "alloc")]
pub use heap::{Channel, Layout};

#[cfg(feature = "alloc")]
mod heap {
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::fmt;

    use super::*;

    /// A channel: what it carries and, if given, a label to show.
    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    pub struct Channel {
        pub symbol: Symbol,
        #[cfg_attr(
            feature = "serde",
            serde(default, skip_serializing_if = "Option::is_none")
        )]
        pub label: Option<String>,
    }

    impl Channel {
        pub const fn new(symbol: Symbol) -> Self {
            Self {
                symbol,
                label: None,
            }
        }
    }

    /// The channels of a stream, in order.
    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(transparent))]
    pub struct Layout(Vec<Channel>);

    impl Layout {
        pub fn new(channels: Vec<Channel>) -> Self {
            Self(channels)
        }

        /// What to assume for `channels` channels whose sender doesn't say;
        /// see [`default_symbols`].
        pub fn default_for(channels: usize) -> Self {
            let mut v = Vec::with_capacity(channels);
            default_symbols(channels, |s| v.push(Channel::new(s)));
            Self(v)
        }

        /// The layout of an ST 2110-30 grouping, `SMPTE2110.(51,ST)`.
        pub fn from_channel_order(order: &str) -> Option<Self> {
            let mut v = Vec::new();
            read_channel_order(order, |s| v.push(Channel::new(s)))?;
            (!v.is_empty()).then_some(Self(v))
        }

        /// A layout as the user writes it: ST 2110-30 groups, `51` or
        /// `ST,M`, or symbols, `L R C` or `L,R,C`.
        pub fn parse(s: &str) -> Result<Self, ParseLayoutError> {
            let words: Vec<&str> = s
                .split(|c: char| c == ',' || c.is_whitespace())
                .filter(|w| !w.is_empty())
                .collect();
            if words.is_empty() {
                return Err(ParseLayoutError);
            }
            let symbols: Option<Vec<Symbol>> = words.iter().map(|w| w.parse().ok()).collect();
            if let Some(symbols) = symbols {
                // `U02` is a symbol and a group; as the only word, the group.
                let group = matches!(symbols[..], [Symbol::U(_)]);
                if !group {
                    return Ok(Self(symbols.into_iter().map(Channel::new).collect()));
                }
            }
            Self::from_channel_order(&words.join(",")).ok_or(ParseLayoutError)
        }

        pub fn len(&self) -> usize {
            self.0.len()
        }

        pub fn is_empty(&self) -> bool {
            self.0.is_empty()
        }

        pub fn channels(&self) -> &[Channel] {
            &self.0
        }

        pub fn symbols(&self) -> impl Iterator<Item = Symbol> + '_ {
            self.0.iter().map(|c| c.symbol)
        }

        /// The label to show for channel `index`: its own, else the name of
        /// its symbol, else "Channel 3".
        pub fn label(&self, index: usize) -> String {
            use alloc::borrow::ToOwned;
            use alloc::format;
            let channel = self.0.get(index);
            match channel.and_then(|c| c.label.clone()) {
                Some(label) => label,
                None => match channel.and_then(|c| c.symbol.name()) {
                    Some(name) => name.to_owned(),
                    None => format!("Channel {}", index + 1),
                },
            }
        }

        /// The ST 2110-30 grouping, `51,ST`, without the `SMPTE2110.()`.
        pub fn channel_order(&self) -> String {
            let symbols: Vec<Symbol> = self.symbols().collect();
            let mut s = String::new();
            write_channel_order(&symbols, &mut s).expect("writing to a String");
            s
        }
    }

    /// The layout in short: `51`, `ST,M`, or the symbols, `L R C`.
    impl fmt::Display for Layout {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            let order = self.channel_order();
            // The grouping where it says all, else the symbols.
            if Self::from_channel_order(&order).is_some_and(|l| l.same_symbols(self)) {
                return f.write_str(&order);
            }
            for (i, s) in self.symbols().enumerate() {
                if i > 0 {
                    f.write_str(" ")?;
                }
                write!(f, "{s}")?;
            }
            Ok(())
        }
    }

    impl Layout {
        fn same_symbols(&self, other: &Self) -> bool {
            self.symbols().eq(other.symbols())
        }
    }

    /// Neither ST 2110-30 groups nor channel symbols.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ParseLayoutError;

    impl fmt::Display for ParseLayoutError {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str(
                "a layout is ST 2110-30 groups (M, DM, ST, LtRt, 51, 71, 222, SGRP, U01 to U64), \
                 as in 51 or ST,M, or channel symbols, as in L R C",
            )
        }
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for Symbol {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Symbol {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = Symbol;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an NMOS channel symbol")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Symbol, E> {
                v.parse().map_err(E::custom)
            }
        }
        d.deserialize_str(Visitor)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::string::String;
    use std::vec::Vec;

    use super::*;

    fn order(order: &str) -> Option<Vec<String>> {
        let mut v = Vec::new();
        read_channel_order(order, |s| v.push(std::format!("{s}")))?;
        Some(v)
    }

    #[test]
    fn symbols_read_and_write() {
        for s in ["L", "LFE", "HI", "M2", "NSC000", "NSC128", "U01", "U64"] {
            assert_eq!(std::format!("{}", s.parse::<Symbol>().unwrap()), s);
        }
        for s in ["", "l", "M3", "NSC129", "NSC12", "U00", "U65", "U1", "U+1"] {
            assert_eq!(s.parse::<Symbol>(), Err(UnknownSymbol), "{s}");
        }
    }

    #[test]
    fn reads_channel_orders() {
        assert_eq!(order("SMPTE2110.(ST)").unwrap(), ["L", "R"]);
        assert_eq!(
            order("SMPTE2110.(51,M,U02)").unwrap(),
            ["L", "R", "C", "LFE", "Ls", "Rs", "M1", "U01", "U02"]
        );
        assert_eq!(order("DM").unwrap(), ["M1", "M2"]);
        assert_eq!(order("SGRP,U01").unwrap().len(), 5);
        assert_eq!(order("SMPTE2110.(222)").unwrap().len(), 24);
        for bad in [
            "SMPTE2110.ST",
            "SMPTE2110.(XY)",
            "U00",
            "U65",
            "U2",
            "ST,,M",
        ] {
            assert_eq!(order(bad), None, "{bad}");
        }
    }

    #[test]
    fn writes_channel_orders() {
        use Symbol::*;
        let write = |symbols: &[Symbol]| {
            let mut s = String::new();
            write_channel_order(symbols, &mut s).unwrap();
            s
        };
        assert_eq!(write(&[L, R]), "ST");
        assert_eq!(write(&[M1]), "M");
        assert_eq!(write(&[L, R, C, Lfe, Ls, Rs, L, R]), "51,ST");
        assert_eq!(write(&[L, R, C, Lfe, Lss, Rss, Lrs, Rrs]), "71");
        // What no group says goes as undefined channels.
        assert_eq!(write(&[C, L, R, Hi, Vin]), "U01,ST,U02");
        assert_eq!(write(&[R, L]), "U02");
        assert_eq!(write(&[U(1); 70]), "U64,U06");
        assert_eq!(write(&[]), "");
    }

    #[test]
    fn defaults_by_count() {
        let symbols = |n| {
            let mut v = Vec::new();
            default_symbols(n, |s| v.push(s));
            v
        };
        assert_eq!(symbols(1), [Symbol::M1]);
        assert_eq!(symbols(2), [Symbol::L, Symbol::R]);
        assert_eq!(symbols(6)[3], Symbol::Lfe);
        assert_eq!(symbols(4), [1, 2, 3, 4].map(Symbol::U));
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn layouts_parse_and_show() {
        let shown = |s: &str| std::format!("{}", Layout::parse(s).unwrap());
        assert_eq!(shown("51"), "51");
        assert_eq!(shown("L R"), "ST");
        assert_eq!(shown("ST, M"), "ST,M");
        assert_eq!(shown("U02"), "U02");
        assert_eq!(shown("R,L"), "R L");
        assert_eq!(Layout::parse("U02").unwrap().len(), 2);
        assert_eq!(Layout::parse("U05 U06").unwrap().len(), 2);
        assert!(Layout::parse("").is_err());
        assert!(Layout::parse("L X").is_err());
        let mut channels = Layout::parse("L R U01").unwrap().channels().to_vec();
        channels[1].label = Some("Rechts".into());
        let layout = Layout::new(channels);
        assert_eq!(
            (layout.label(0), layout.label(1), layout.label(2)),
            ("Left".into(), "Rechts".into(), "Channel 3".into())
        );
    }

    #[cfg(feature = "serde")]
    #[test]
    fn layouts_as_json() {
        let json = r#"[{"symbol":"L"},{"symbol":"U01","label":"Mic"}]"#;
        let layout: Layout = serde_json::from_str(json).unwrap();
        assert_eq!(layout.label(1), "Mic");
        assert_eq!(serde_json::to_string(&layout).unwrap(), json);
        assert!(serde_json::from_str::<Layout>(r#"[{"symbol":"X"}]"#).is_err());
    }
}
