//! What orpheusctl shows, in the language of the environment (see
//! [`orpheus_i18n`]); the help too, which clap can't translate itself.

use std::sync::{Arc, LazyLock};

use clap::{Arg, ArgAction, Command, CommandFactory};
use orpheus_i18n::{Catalog, Texts};

/// The translations; English first, as the fallback.
pub static CATALOG: Catalog = Catalog::new(&[
    ("en", include_str!("../locales/en/orpheusctl.ftl")),
    ("de", include_str!("../locales/de/orpheusctl.ftl")),
]);

/// The texts in the environment's language; English in tests.
pub fn texts() -> &'static Texts {
    static TEXTS: LazyLock<Arc<Texts>> =
        LazyLock::new(|| CATALOG.texts(if cfg!(test) { Some("en") } else { None }));
    &TEXTS
}

/// A text in the user's language: `t!("about")`, `t!("frames", n = 3)`; the
/// values are anything an [`orpheus_i18n::Arg`] can be made from.
macro_rules! t {
    ($id:literal $(, $name:ident = $value:expr)* $(,)?) => {
        $crate::i18n::texts().text(
            &orpheus_i18n::Text::new($id)$(.arg(stringify!($name), $value))*
        )
    };
}
pub(crate) use t;

/// A number formatted as in Rust, `+1.25`, as the user's language writes it.
pub fn num(formatted: String) -> String {
    texts().number(formatted)
}

/// The command line, with its help in the user's language.
pub fn command() -> Command {
    let mut cmd = localize(crate::Cli::command(), None)
        .about(t!("about"))
        .disable_version_flag(true)
        .arg(
            Arg::new("version")
                .short('V')
                .long("version")
                .action(ArgAction::Version)
                .help(t!("help-version"))
                .help_heading(t!("help-options")),
        );
    // clap adds `help` as it builds; only then can it be described.
    cmd.build();
    cmd.mut_subcommand("help", |help| {
        help.about(t!("cmd-help")).mut_args(|a| {
            a.help(t!("arg-help-command"))
                .help_heading(t!("help-arguments"))
        })
    })
}

/// Describes `cmd`, its arguments and its subcommands from the Fluent
/// files: `cmd-<path>` and `arg-<path>-<argument>`, where `path` names the
/// command, `output-create` for `output create`.
fn localize(mut cmd: Command, path: Option<String>) -> Command {
    let texts = texts();
    if let Some(about) = path.as_ref().and_then(|p| texts.get(&format!("cmd-{p}"))) {
        cmd = cmd.about(about);
    }
    // Under a heading of their own, clap sorts arguments by their order
    // and then their names; positional ones need their places.
    let mut place = 0;
    cmd = cmd.mut_args(|a| {
        let id = a.get_id().as_str().to_lowercase();
        let key = match &path {
            Some(p) => format!("arg-{p}-{id}"),
            None => format!("arg-{id}"),
        };
        let (heading, a) = if a.is_positional() {
            place += 1;
            (t!("help-arguments"), a.display_order(place))
        } else {
            (t!("help-options"), a)
        };
        // Defaults and choices are in the help, in the user's language.
        let mut a = a.help_heading(heading).hide_env(true);
        if a.get_action().takes_values() {
            a = a.hide_default_value(true).hide_possible_values(true);
        }
        match texts.get(&key) {
            Some(help) => a.help(help),
            None => a,
        }
    });
    let header = cmd.get_styles().get_header();
    let usage = format!("{header}{}{header:#}", t!("help-usage"));
    cmd = cmd
        .help_template(format!(
            "{{before-help}}{{about-with-newline}}\n{usage} {{usage}}\n\n{{all-args}}{{after-help}}"
        ))
        .subcommand_help_heading(t!("help-commands"))
        .disable_help_flag(true)
        .arg(
            Arg::new("help")
                .short('h')
                .long("help")
                .action(ArgAction::Help)
                .help(t!("help-help"))
                .display_order(usize::MAX)
                .help_heading(t!("help-options")),
        );
    let names: Vec<String> = cmd
        .get_subcommands()
        .map(|s| s.get_name().to_owned())
        .collect();
    for name in names {
        let sub = match &path {
            Some(p) => format!("{p}-{name}"),
            None => name.clone(),
        };
        cmd = cmd.mut_subcommand(&name, |s| localize(s, Some(sub)));
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every command and argument has its help.
    #[test]
    fn help_is_complete() {
        fn check(cmd: &Command, path: Option<String>, missing: &mut Vec<String>) {
            if let Some(p) = &path
                && !CATALOG.has(&format!("cmd-{p}"))
            {
                missing.push(format!("cmd-{p}"));
            }
            for a in cmd.get_arguments() {
                let id = a.get_id().as_str().to_lowercase();
                if matches!(id.as_str(), "help" | "version") {
                    continue;
                }
                let key = match &path {
                    Some(p) => format!("arg-{p}-{id}"),
                    None => format!("arg-{id}"),
                };
                if !CATALOG.has(&key) {
                    missing.push(key);
                }
            }
            for s in cmd.get_subcommands() {
                let name = s.get_name();
                let sub = match &path {
                    Some(p) => format!("{p}-{name}"),
                    None => name.to_owned(),
                };
                check(s, Some(sub), missing);
            }
        }
        let mut missing = vec![];
        check(&crate::Cli::command(), None, &mut missing);
        assert_eq!(missing, Vec::<String>::new());
    }

    #[test]
    fn help_shows() {
        let help = command().render_help().to_string();
        assert!(help.contains("Usage: orpheusctl"), "{help}");
        assert!(
            help.contains("List the other nodes on the network."),
            "{help}"
        );
        assert!(help.contains("Print help"), "{help}");
        let mut cmd = command();
        let connect = cmd.find_subcommand_mut("connect").unwrap();
        let help = connect.render_help().to_string();
        assert!(help.contains("Take over the peer's receiver"), "{help}");
        assert!(help.contains("The node to talk to"), "{help}");
    }

    #[test]
    fn code_uses_known_texts() {
        let unknown =
            CATALOG.unknown_in(&[include_str!("main.rs"), include_str!("i18n.rs")], &["t"]);
        assert_eq!(unknown, Vec::<String>::new());
    }

    #[test]
    fn translations_have_every_text() {
        assert_eq!(CATALOG.mismatches(), Vec::<String>::new());
    }
}
