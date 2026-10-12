//! Kinds of audio outputs.

/// What kind of device an output is. The device that owns the output
/// announces it; other nodes show the output with a matching icon and may
/// override the type locally.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum OutputType {
    /// Loudspeaker, smart speaker.
    Speaker,
    Headphones,
    /// Headphones with a microphone.
    Headset,
    /// TV, projector, monitor with speakers.
    Tv,
    /// Amplifier, AV receiver, stereo, DAC.
    Hifi,
    /// PC or laptop.
    Computer,
    /// Portable player.
    Portable,
    /// Phone or tablet.
    Phone,
    /// Anything else, or not known.
    #[default]
    Generic,
}

impl OutputType {
    pub const ALL: [Self; 9] = [
        Self::Speaker,
        Self::Headphones,
        Self::Headset,
        Self::Tv,
        Self::Hifi,
        Self::Computer,
        Self::Portable,
        Self::Phone,
        Self::Generic,
    ];

    /// The name used in config files and announcements.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Speaker => "speaker",
            Self::Headphones => "headphones",
            Self::Headset => "headset",
            Self::Tv => "tv",
            Self::Hifi => "hifi",
            Self::Computer => "computer",
            Self::Portable => "portable",
            Self::Phone => "phone",
            Self::Generic => "generic",
        }
    }
}

impl core::fmt::Display for OutputType {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}
