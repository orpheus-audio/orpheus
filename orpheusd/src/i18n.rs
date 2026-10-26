//! The texts orpheusd shows to people: names in the audio system,
//! setting errors, in `[ui] language` or the environment's
//! (see [`orpheus_i18n`]). Logs stay in English.

use std::sync::Arc;

use orpheus_i18n::Catalog;
pub use orpheus_i18n::{Arg, Text, Texts};

/// The translations; English first, as the fallback.
static CATALOG: Catalog = Catalog::new(&[
    ("en", include_str!("../locales/en/orpheusd.ftl")),
    ("de", include_str!("../locales/de/orpheusd.ftl")),
]);

/// `[ui] language` when it takes the environment's.
pub const AUTO: &str = "auto";

/// The texts for `[ui] language`: a language tag, or [`AUTO`].
pub fn texts(language: &str) -> Arc<Texts> {
    // Tests don't depend on the environment.
    let auto = if cfg!(test) { Some("en") } else { None };
    CATALOG.texts(if language == AUTO {
        auto
    } else {
        Some(language)
    })
}

/// Whether `language` is a value `[ui] language` takes.
pub fn valid(language: &str) -> bool {
    language == AUTO || orpheus_i18n::valid(language)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_texts() {
        let t = Text::new("sink-unavailable").arg("name", "TV");
        assert_eq!(texts("en").text(&t), "TV (unavailable)");
        assert_eq!(texts("de").text(&t), "TV (nicht verfügbar)");
    }

    #[test]
    fn formats_arguments() {
        let t = Text::new("setting-block")
            .arg("min", 16)
            .arg("max", 4800)
            .arg("min_ms", Arg::Decimal(16.0 / 48.0, 2))
            .arg("max_ms", Arg::Decimal(100.0, 0))
            .arg("frames", 8);
        assert_eq!(
            texts("en").text(&t),
            "the block must be 16 to 4800 frames (at 48 kHz 0.33 to 100 ms), not 8"
        );
        assert!(texts("de").text(&t).contains("0,33"));
    }

    /// Every text the code asks for is in the English file.
    #[test]
    fn code_uses_known_texts() {
        let sources = [
            include_str!("health.rs"),
            include_str!("settings.rs"),
            include_str!("config.rs"),
            include_str!("connection.rs"),
            include_str!("main.rs"),
        ];
        assert_eq!(CATALOG.unknown_in(&sources, &[]), Vec::<String>::new());
    }

    #[test]
    fn translations_have_every_text() {
        assert_eq!(CATALOG.mismatches(), Vec::<String>::new());
    }
}
