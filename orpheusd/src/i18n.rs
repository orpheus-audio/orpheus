//! The texts Orpheus shows in the audio system, in the user's language.
//!
//! Translations are Fluent files under `locales/`, compiled in. The
//! language is `[ui] language` if set, else the environment's
//! (`LC_ALL`, `LC_MESSAGES`, `LANG`); English where neither has a
//! translation.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use fluent_bundle::concurrent::FluentBundle;
use fluent_bundle::{FluentArgs, FluentResource};
use fluent_langneg::{NegotiationStrategy, negotiate_languages};
use unic_langid::{LanguageIdentifier, langid};

/// The translations, by language; English first, as the fallback.
const LOCALES: &[(LanguageIdentifier, &str)] = &[
    (langid!("en"), include_str!("../locales/en/orpheusd.ftl")),
    (langid!("de"), include_str!("../locales/de/orpheusd.ftl")),
];

/// The texts in one language.
pub struct Texts {
    bundle: FluentBundle<FluentResource>,
}

impl Texts {
    fn new(language: &LanguageIdentifier) -> Self {
        let mut bundle = FluentBundle::new_concurrent(vec![language.clone()]);
        // No Unicode isolation marks: mixers show them as boxes.
        bundle.set_use_isolating(false);
        let add = |bundle: &mut FluentBundle<FluentResource>, ftl: &str| {
            let resource = FluentResource::try_new(ftl.to_owned()).expect("valid Fluent file");
            bundle.add_resource_overriding(resource);
        };
        // English underneath, so that a missing translation shows English.
        add(&mut bundle, LOCALES[0].1);
        if let Some((_, ftl)) = LOCALES.iter().skip(1).find(|(l, _)| l == language) {
            add(&mut bundle, ftl);
        }
        Self { bundle }
    }

    /// The name of a sink to another node while that node can't play the
    /// stream.
    pub fn sink_unavailable(&self, name: &str) -> String {
        let mut args = FluentArgs::new();
        args.set("name", name);
        self.format("sink-unavailable", &args)
    }

    fn format(&self, id: &str, args: &FluentArgs) -> String {
        let Some(pattern) = self.bundle.get_message(id).and_then(|m| m.value()) else {
            return id.to_owned();
        };
        let mut errors = vec![];
        let text = self.bundle.format_pattern(pattern, Some(args), &mut errors);
        if !errors.is_empty() {
            tracing::warn!(id, ?errors, "formatting a text");
        }
        text.into_owned()
    }
}

/// The language to use: `chosen` if set, else the environment's; the
/// best of the translations for it.
fn negotiate(chosen: Option<&str>) -> LanguageIdentifier {
    let wanted: Vec<LanguageIdentifier> = chosen
        .map(str::to_owned)
        .or_else(sys_locale::get_locale)
        .into_iter()
        // `de_DE.UTF-8` from the environment: the part before the dot.
        .filter_map(|l| l.split('.').next()?.replace('_', "-").parse().ok())
        .collect();
    let available: Vec<LanguageIdentifier> = LOCALES.iter().map(|(l, _)| l.clone()).collect();
    let fallback = &LOCALES[0].0;
    negotiate_languages(
        &wanted,
        &available,
        Some(fallback),
        NegotiationStrategy::Lookup,
    )
    .first()
    .map_or_else(|| fallback.clone(), |&l| l.clone())
}

/// The texts for `[ui] language` (`None`: from the environment).
pub fn texts(chosen: Option<&str>) -> Arc<Texts> {
    static CACHE: LazyLock<Mutex<HashMap<Option<String>, Arc<Texts>>>> =
        LazyLock::new(Mutex::default);
    let mut cache = CACHE.lock().unwrap();
    cache
        .entry(chosen.map(str::to_owned))
        .or_insert_with(|| Arc::new(Texts::new(&negotiate(chosen))))
        .clone()
}

/// Whether `language` is a language tag, such as `de` or `de-AT`.
pub fn valid(language: &str) -> bool {
    !language.is_empty() && language.parse::<LanguageIdentifier>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_language() {
        assert_eq!(negotiate(Some("de")), langid!("de"));
        assert_eq!(negotiate(Some("de-AT")), langid!("de"));
        assert_eq!(negotiate(Some("de_DE.UTF-8")), langid!("de"));
        assert_eq!(negotiate(Some("fr")), langid!("en"));
    }

    #[test]
    fn formats_texts() {
        assert_eq!(texts(Some("en")).sink_unavailable("TV"), "TV (unavailable)");
        assert_eq!(
            texts(Some("de")).sink_unavailable("TV"),
            "TV (nicht verfügbar)"
        );
        assert_eq!(texts(Some("fr")).sink_unavailable("TV"), "TV (unavailable)");
    }

    #[test]
    fn translations_have_every_text() {
        let ids = |ftl: &str| {
            let mut ids: Vec<String> = FluentResource::try_new(ftl.to_owned())
                .expect("valid Fluent file")
                .entries()
                .filter_map(|e| match e {
                    fluent_syntax::ast::Entry::Message(m) => Some(m.id.name.to_owned()),
                    _ => None,
                })
                .collect();
            ids.sort();
            ids
        };
        for (language, ftl) in LOCALES {
            assert_eq!(ids(ftl), ids(LOCALES[0].1), "{language}");
        }
    }
}
