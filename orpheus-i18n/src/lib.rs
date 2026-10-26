//! Texts for people, in their language: what Orpheus shows in the audio
//! system, its warnings, setting errors and `orpheusctl`.
//!
//! Each program has its translations as Fluent files, compiled in, in a
//! [`Catalog`]. The language is the one asked for, else the environment's
//! (`LANGUAGE`, `LC_ALL`, `LC_MESSAGES`, `LANG`); English where neither has
//! a translation.

use std::sync::{Arc, Mutex};

use fluent_bundle::concurrent::FluentBundle;
use fluent_bundle::{FluentArgs, FluentResource, FluentValue};
use fluent_langneg::{NegotiationStrategy, negotiate_languages};
use unic_langid::LanguageIdentifier;

/// The translations of one program, and the texts made from them.
pub struct Catalog {
    /// Language tag and Fluent file; English first, as the fallback.
    locales: &'static [(&'static str, &'static str)],
    /// By the language asked for.
    cache: Mutex<Vec<(Option<String>, Arc<Texts>)>>,
}

/// The texts in one language.
pub struct Texts {
    bundle: FluentBundle<FluentResource>,
    /// Whether numbers take a decimal comma, as in German.
    decimal_comma: bool,
}

/// A text by its ID in the Fluent files and its arguments, to be formatted
/// in any language: warnings show in the node's language and are logged in
/// English.
#[derive(Debug, Clone, PartialEq)]
pub struct Text {
    id: &'static str,
    args: Vec<(&'static str, Arg)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Arg {
    Str(String),
    Int(i64),
    /// A number with this many decimals.
    Decimal(f64, usize),
    /// A number for Fluent to choose a plural by, not shown.
    Count(f64),
    /// Another text, formatted in the same language.
    Text(Text),
}

impl Text {
    pub fn new(id: &'static str) -> Self {
        Self { id, args: vec![] }
    }

    pub fn arg(mut self, name: &'static str, value: impl Into<Arg>) -> Self {
        self.args.push((name, value.into()));
        self
    }
}

impl From<String> for Arg {
    fn from(s: String) -> Self {
        Arg::Str(s)
    }
}

impl From<&str> for Arg {
    fn from(s: &str) -> Self {
        Arg::Str(s.to_owned())
    }
}

impl From<&String> for Arg {
    fn from(s: &String) -> Self {
        Arg::Str(s.clone())
    }
}

impl From<Text> for Arg {
    fn from(t: Text) -> Self {
        Arg::Text(t)
    }
}

macro_rules! int_arg {
    ($($t:ty),*) => {$(
        impl From<$t> for Arg {
            fn from(n: $t) -> Self {
                Arg::Int(n as i64)
            }
        }
    )*};
}
int_arg!(i8, u8, u16, i32, u32, u64, i64, usize);

impl Catalog {
    pub const fn new(locales: &'static [(&'static str, &'static str)]) -> Self {
        Self {
            locales,
            cache: Mutex::new(Vec::new()),
        }
    }

    /// The texts in `chosen`, a language tag such as `de` (`None`: the
    /// environment's).
    pub fn texts(&self, chosen: Option<&str>) -> Arc<Texts> {
        let mut cache = self.cache.lock().unwrap();
        if let Some((_, texts)) = cache.iter().find(|(c, _)| c.as_deref() == chosen) {
            return texts.clone();
        }
        let texts = Arc::new(self.make(&self.negotiate(chosen)));
        cache.push((chosen.map(str::to_owned), texts.clone()));
        texts
    }

    fn languages(&self) -> Vec<LanguageIdentifier> {
        self.locales
            .iter()
            .map(|(l, _)| l.parse().expect("valid language tag"))
            .collect()
    }

    /// The language to use: `chosen` if set, else the environment's; the
    /// best of the translations for it.
    fn negotiate(&self, chosen: Option<&str>) -> LanguageIdentifier {
        let wanted: Vec<LanguageIdentifier> = match chosen {
            Some(l) => vec![l.to_owned()],
            None => sys_locale::get_locales().collect(),
        }
        .iter()
        // `de_DE.UTF-8`: the part before the dot.
        .filter_map(|l| l.split('.').next()?.replace('_', "-").parse().ok())
        .collect();
        let available = self.languages();
        let fallback = &available[0];
        negotiate_languages(
            &wanted,
            &available,
            Some(fallback),
            NegotiationStrategy::Lookup,
        )
        .first()
        .map_or_else(|| fallback.clone(), |&l| l.clone())
    }

    fn make(&self, language: &LanguageIdentifier) -> Texts {
        let mut bundle = FluentBundle::new_concurrent(vec![language.clone()]);
        // No Unicode isolation marks: mixers and terminals show them as
        // boxes.
        bundle.set_use_isolating(false);
        // English underneath, so that a missing translation shows English.
        bundle.add_resource_overriding(resource(self.locales[0].1));
        if let Some((_, ftl)) = self
            .locales
            .iter()
            .skip(1)
            .find(|(l, _)| l.parse::<LanguageIdentifier>().ok().as_ref() == Some(language))
        {
            bundle.add_resource_overriding(resource(ftl));
        }
        Texts {
            bundle,
            decimal_comma: language.language.as_str() == "de",
        }
    }

    /// Whether English has a message `id`.
    pub fn has(&self, id: &str) -> bool {
        ids(self.locales[0].1).iter().any(|i| i == id)
    }

    /// For tests: the IDs that a translation lacks or has in excess of
    /// English, by language.
    pub fn mismatches(&self) -> Vec<String> {
        let english = ids(self.locales[0].1);
        let mut found = vec![];
        for (language, ftl) in &self.locales[1..] {
            let these = ids(ftl);
            for id in english.iter().filter(|i| !these.contains(i)) {
                found.push(format!("{language} lacks {id}"));
            }
            for id in these.iter().filter(|i| !english.contains(i)) {
                found.push(format!("{language} has {id}, English doesn't"));
            }
        }
        found
    }

    /// For tests: the IDs of `Text::new("…")` and `$macro!("…"` in
    /// `sources` that English lacks.
    pub fn unknown_in(&self, sources: &[&str], macros: &[&str]) -> Vec<String> {
        let english = ids(self.locales[0].1);
        let mut calls = vec!["Text::new(\"".to_owned()];
        calls.extend(macros.iter().map(|m| format!("{m}!(\"")));
        let mut found = vec![];
        for source in sources {
            for call in &calls {
                for (at, _) in source.match_indices(call.as_str()) {
                    // Not the end of a longer name, such as `format!`.
                    let before = source[..at].chars().next_back();
                    if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                        continue;
                    }
                    let rest = &source[at + call.len()..];
                    let id = &rest[..rest.find('"').unwrap_or(0)];
                    if !english.iter().any(|i| i == id) {
                        found.push(id.to_owned());
                    }
                }
            }
        }
        found
    }
}

fn resource(ftl: &str) -> FluentResource {
    FluentResource::try_new(ftl.to_owned()).expect("valid Fluent file")
}

/// The messages and terms (`-name`) of a Fluent file.
fn ids(ftl: &str) -> Vec<String> {
    resource(ftl)
        .entries()
        .filter_map(|e| match e {
            fluent_syntax::ast::Entry::Message(m) => Some(m.id.name.to_owned()),
            fluent_syntax::ast::Entry::Term(t) => Some(format!("-{}", t.id.name)),
            _ => None,
        })
        .collect()
}

impl Texts {
    /// `text` in this language.
    pub fn text(&self, text: &Text) -> String {
        let mut args = FluentArgs::new();
        for (name, value) in &text.args {
            let value: FluentValue = match value {
                Arg::Str(s) => s.clone().into(),
                Arg::Int(n) => (*n).into(),
                Arg::Decimal(n, places) => self.number(format!("{n:.places$}")).into(),
                Arg::Count(n) => (*n).into(),
                Arg::Text(t) => self.text(t).into(),
            };
            args.set(*name, value);
        }
        self.format(text.id, &args)
    }

    /// The message `id`, without arguments; `None` if there is none.
    pub fn get(&self, id: &str) -> Option<String> {
        self.bundle
            .has_message(id)
            .then(|| self.format(id, &FluentArgs::new()))
    }

    /// A number formatted as in Rust, `+1.25`, as this language writes it.
    pub fn number(&self, formatted: String) -> String {
        if self.decimal_comma {
            formatted.replace('.', ",")
        } else {
            formatted
        }
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

/// Whether `language` is a language tag, such as `de` or `de-AT`.
pub fn valid(language: &str) -> bool {
    !language.is_empty() && language.parse::<LanguageIdentifier>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    static CATALOG: Catalog = Catalog::new(&[
        (
            "en",
            "hello = Hello { $name }\nshort = { $n } ms\nthings = { $count ->\n    [one] { $n } thing\n   *[other] { $n } things\n}\nonly-en = English\n",
        ),
        (
            "de",
            "hello = Hallo { $name }\nshort = { $n } ms\nthings = { $count ->\n    [one] { $n } Ding\n   *[other] { $n } Dinge\n}\n",
        ),
    ]);

    #[test]
    fn picks_the_language() {
        let tag = |l| CATALOG.negotiate(Some(l)).to_string();
        assert_eq!(tag("de"), "de");
        assert_eq!(tag("de-AT"), "de");
        assert_eq!(tag("de_DE.UTF-8"), "de");
        assert_eq!(tag("fr"), "en");
    }

    #[test]
    fn formats_arguments() {
        let t = Text::new("short").arg("n", Arg::Decimal(1.26, 1));
        assert_eq!(CATALOG.texts(Some("en")).text(&t), "1.3 ms");
        assert_eq!(CATALOG.texts(Some("de")).text(&t), "1,3 ms");
        let hello = Text::new("hello").arg("name", "TV");
        assert_eq!(CATALOG.texts(Some("de")).text(&hello), "Hallo TV");
        assert_eq!(CATALOG.texts(Some("fr")).text(&hello), "Hello TV");
    }

    #[test]
    fn chooses_plurals() {
        let things = |n: f64, shown: &str| {
            let t = Text::new("things")
                .arg("n", shown)
                .arg("count", Arg::Count(n));
            CATALOG.texts(Some("de")).text(&t)
        };
        assert_eq!(things(1.0, "1"), "1 Ding");
        assert_eq!(things(2.0, "2"), "2 Dinge");
        assert_eq!(things(1.5, "1,5"), "1,5 Dinge");
    }

    #[test]
    fn falls_back_to_english() {
        assert_eq!(CATALOG.texts(Some("de")).get("only-en").unwrap(), "English");
        assert_eq!(CATALOG.texts(Some("de")).get("missing"), None);
        assert_eq!(CATALOG.mismatches(), ["de lacks only-en"]);
        assert_eq!(
            CATALOG.unknown_in(
                &["t!(\"hello\") format!(\"x\") Text::new(\"nope\")"],
                &["t"]
            ),
            ["nope"]
        );
    }
}
