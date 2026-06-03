//! # go-tool-synthesizer
//!
//! The platform engine's first vertical: a public, generic generator that turns
//! a typed [`GoToolSpec`] into a complete, GSDS-conformant, borealis-profiled Go
//! tool composing the pleme-io fleet primitives (cli-go / shikumi-go / borealis
//! / errors-go / logging-go).
//!
//! Take a pattern → mass-generate standardized tools. The spec is authored
//! declaratively as a `(defgotool …)` Lisp form via `#[derive(TataraDomain)]`;
//! [`lower`] turns it into a set of typed `go_synthesizer::GoFile` values
//! matching the canonical §4 three-line-tool shape (proven by
//! `borealis-cli-example`), rendered to byte-stable Go through
//! `go_synthesizer::print_file`.
//!
//! ```no_run
//! use go_tool_synthesizer::{lower, GoToolSpec};
//! use tatara_lisp::{read, domain::TataraDomain};
//!
//! let forms = read(r#"(defgotool :name "borealis-greet" :kind Cli
//!     :description "greeter" :profile "nord")"#).unwrap();
//! let spec = GoToolSpec::compile_from_sexp(&forms[0]).unwrap();
//! for (path, file) in lower(&spec) {
//!     let src = go_synthesizer::print_file(&file);
//!     // write src to <repo>/<path>
//!     let _ = (path, src);
//! }
//! ```

mod lower;
mod spec;

pub use lower::lower;
pub use spec::{CommandSpec, ConfigField, FieldType, FlagSpec, GoToolSpec, ToolKind};

#[cfg(test)]
mod tests {
    use super::*;
    use go_synthesizer::print_file;
    use std::collections::BTreeMap;
    use tatara_lisp::{domain::TataraDomain, read};

    /// The generic proof spec: a Nord-profiled CLI with one `greet` command, a
    /// `--name` flag, and a required `greeting` config field.
    fn proof_spec() -> GoToolSpec {
        let forms = read(
            r#"(defgotool
                  :name "borealis-greet"
                  :kind Cli
                  :description "A generic borealis-profiled greeter — the platform engine proof."
                  :profile "nord"
                  :primitives ("cli-go" "shikumi-go" "borealis" "errors-go" "logging-go")
                  :config-fields (
                    (:name "greeting" :ty Str :yaml "greeting" :validate "required")
                    (:name "locale"   :ty Str :yaml "locale"))
                  :commands (
                    (:name "greet"
                     :summary "print a themed greeting"
                     :long "Greet a name using the configured greeting."
                     :flags ((:name "name" :ty Str :default "world" :usage "who to greet"
                              :require-non-empty #t)))))"#,
        )
        .unwrap();
        GoToolSpec::compile_from_sexp(&forms[0]).unwrap()
    }

    #[test]
    fn defgotool_compiles_from_lisp() {
        let s = proof_spec();
        assert_eq!(s.name, "borealis-greet");
        assert_eq!(s.kind, ToolKind::Cli);
        assert_eq!(s.profile, "nord");
        assert_eq!(s.primitives.len(), 5);
        assert_eq!(s.config_fields.len(), 2);
        assert_eq!(s.config_fields[0].name, "greeting");
        assert_eq!(s.config_fields[0].ty, FieldType::Str);
        assert_eq!(s.config_fields[0].validate.as_deref(), Some("required"));
        assert_eq!(s.commands.len(), 1);
        assert_eq!(s.commands[0].name, "greet");
        assert_eq!(s.commands[0].flags.len(), 1);
        assert_eq!(s.commands[0].flags[0].name, "name");
        assert!(s.commands[0].flags[0].require_non_empty);
    }

    #[test]
    fn defaults_apply() {
        let forms =
            read(r#"(defgotool :name "minimal" :description "d")"#).unwrap();
        let s = GoToolSpec::compile_from_sexp(&forms[0]).unwrap();
        assert_eq!(s.kind, ToolKind::Cli); // default
        // The derive falls back to String::default() (empty) for a missing
        // field with a named serde default; the accessor applies the real
        // default. (See crate GAPS — derive named-default limitation.)
        assert_eq!(s.resolved_profile(), "tundra"); // default
        assert_eq!(s.resolved_go_version(), "1.22"); // default
        assert_eq!(s.theme_constructor(), "Tundra");
        assert_eq!(s.resolved_module_path(), "github.com/pleme-io/minimal");
        assert_eq!(s.env_prefix(), "MINIMAL_");
    }

    #[test]
    fn env_prefix_derivation() {
        let s = proof_spec();
        assert_eq!(s.env_prefix(), "BOREALIS_GREET_");
        assert_eq!(s.theme_constructor(), "Nord");
        assert_eq!(s.resolved_module_path(), "github.com/pleme-io/borealis-greet");
    }

    /// Render the whole tool to a path→source map for assertion convenience.
    fn rendered(spec: &GoToolSpec) -> BTreeMap<String, String> {
        lower(spec)
            .into_iter()
            .map(|(p, f)| (p.to_string_lossy().into_owned(), print_file(&f)))
            .collect()
    }

    #[test]
    fn lower_emits_all_files() {
        let r = rendered(&proof_spec());
        assert!(r.contains_key("main.go"));
        assert!(r.contains_key("internal/app/config.go"));
        assert!(r.contains_key("internal/app/errors.go"));
        assert!(r.contains_key("internal/app/app.go"));
        assert!(r.contains_key("internal/app/app_test.go"));
        // The green smoke test exercises real wiring, not a vacuous assertion.
        let test = &r["internal/app/app_test.go"];
        assert!(test.contains("func TestNewBuildsApp(t *testing.T)"));
        assert!(test.contains("New(DefaultConfig(), nil, borealis.Nord())"));
    }

    #[test]
    fn main_go_matches_section_35_shape() {
        let r = rendered(&proof_spec());
        let main = &r["main.go"];
        // §3.5: errs.Exit is the single exit funnel wrapping run(ctx).
        assert!(main.contains("errs.Exit(run(context.Background()))"));
        // The run body loads config via the canonical loader.
        assert!(main.contains("cfg, err := app.LoadConfig(ctx)"));
        // logging.FromConfig consumes the sub-struct.
        assert!(main.contains("log, err := logging.FromConfig(cfg.Logging)"));
        // theme — Nord for the public profile.
        assert!(main.contains("theme := borealis.Nord()"));
        // grammar.
        assert!(main.contains("root := app.New(cfg, log, theme)"));
        // §3.5: borealis.Execute is the OUTERMOST entrypoint, mapped through exit.
        assert!(main.contains("return exit.Map(borealis.Execute(ctx, root))"));
        // Imports present.
        assert!(main.contains("\"github.com/pleme-io/borealis\""));
        assert!(main.contains("\"github.com/pleme-io/cli-go/exit\""));
        assert!(main.contains("errs \"github.com/pleme-io/errors-go\""));
        assert!(main.contains("logging \"github.com/pleme-io/logging-go\""));
        assert!(main.contains("\"github.com/pleme-io/borealis-greet/internal/app\""));
    }

    #[test]
    fn config_go_uses_canonical_loader_with_no_shikumi_load_in_fromconfig() {
        let r = rendered(&proof_spec());
        let cfg = &r["internal/app/config.go"];
        // The canonical loader chain — shikumi.For[Config]…Load(ctx).
        assert!(cfg.contains("shikumi.For[Config](Name)"));
        assert!(cfg.contains(".EnvPrefix(EnvPrefix)"));
        assert!(cfg.contains(".Defaults(DefaultConfig())"));
        assert!(cfg.contains(".Validate(validate.New())"));
        assert!(cfg.contains(".Load(ctx)"));
        // FromConfig must NOT call shikumi.Load — logging.FromConfig is invoked
        // in main.go, never re-loading. (No shikumi.Load anywhere.)
        assert!(!cfg.contains("shikumi.Load"));
        // The typed config fields with yaml + validate tags.
        assert!(cfg.contains("Greeting string `yaml:\"greeting\" json:\"greeting\" validate:\"required\"`"));
        // The embedded logging sub-struct.
        assert!(cfg.contains("Logging logging.Config"));
        // EnvPrefix derived from the name.
        assert!(cfg.contains("EnvPrefix = \"BOREALIS_GREET_\""));
    }

    #[test]
    fn errors_go_has_typed_config_error() {
        let r = rendered(&proof_spec());
        let errs = &r["internal/app/errors.go"];
        assert!(errs.contains("func ErrConfig(cause error) error"));
        assert!(errs.contains("errs.Build()"));
        assert!(errs.contains(".Code(\"E_CONFIG\")"));
        assert!(errs.contains(".ExitCode(errs.ExitConfig)"));
        assert!(errs.contains(".Wrap(cause, \"load configuration\")"));
    }

    #[test]
    fn app_go_builds_cli_tree_with_typed_flag() {
        let r = rendered(&proof_spec());
        let app = &r["internal/app/app.go"];
        // App construction.
        assert!(app.contains("cli.NewApp(Name"));
        assert!(app.contains("cli.WithVersion(Version)"));
        // The command is wired into root.Add.
        assert!(app.contains("greetCmd(cfg, log, theme)"));
        // The typed Flag[string] with Env + Validate.
        assert!(app.contains("cli.NewFlag[string](\"name\", \"world\""));
        assert!(app.contains(".Env(EnvPrefix + \"NAME\")"));
        assert!(app.contains(".Validate("));
        // Bound in a Flags closure.
        assert!(app.contains("Flags: func(fs *flag.FlagSet)"));
        assert!(app.contains("name.Bind(fs)"));
        // The Run closure is a typed func literal.
        assert!(app.contains("Run: func(ctx context.Context, _ []string, _ *flag.FlagSet) error {"));
        // Output rendered through the one borealis.Render verb.
        assert!(app.contains("borealis.Render(theme"));
        assert!(app.contains("comp.Item"));
        assert!(app.contains("borealis.Success"));
        // The greeting composition.
        assert!(app.contains("cfg.Greeting + \", \" + nameVal + \"!\""));
    }
}
