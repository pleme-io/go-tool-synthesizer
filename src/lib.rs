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

    // ── Milestone 2: per-kind §4-conformance ────────────────────────────────

    /// Compile a `(defgotool …)` form (the kind-proof helper).
    fn spec_from(src: &str) -> GoToolSpec {
        let forms = read(src).unwrap();
        GoToolSpec::compile_from_sexp(&forms[0]).unwrap()
    }

    fn service_spec() -> GoToolSpec {
        spec_from(
            r#"(defgotool :name "borealis-svc" :kind Service
                :description "svc proof" :profile "nord"
                :primitives ("borealis" "cli-go" "errors-go" "logging-go" "shikumi-go" "lifecycle-go" "server-go")
                :config-fields ((:name "greeting" :ty Str :yaml "greeting" :validate "required")))"#,
        )
    }

    fn daemon_spec() -> GoToolSpec {
        spec_from(
            r#"(defgotool :name "borealis-daemon" :kind Daemon
                :description "daemon proof" :profile "nord"
                :primitives ("borealis" "cli-go" "errors-go" "logging-go" "shikumi-go" "refresh-loop-go" "shigoto-go"))"#,
        )
    }

    fn action_spec() -> GoToolSpec {
        spec_from(
            r#"(defgotool :name "borealis-action" :kind Action
                :description "action proof" :profile "nord"
                :primitives ("borealis" "cli-go" "errors-go" "logging-go" "shikumi-go" "pleme-actions-shared-go")
                :config-fields ((:name "name" :ty Str :yaml "name" :validate "required")
                                 (:name "verbose" :ty Bool :yaml "verbose")))"#,
        )
    }

    #[test]
    fn service_execute_outermost_and_lifecycle_nested_in_serve() {
        let r = rendered(&service_spec());
        let main = &r["main.go"];
        // §3.5: errs.Exit is the single exit funnel.
        assert!(main.contains("errs.Exit(run())"));
        // borealis.Execute is the OUTERMOST entrypoint.
        assert!(main.contains("return borealis.Execute(ctx, root)"));
        assert!(main.contains("root.Add(app.ServeCommand())"));
        // The serve composition root nests lifecycle.New(...).Go("work", ...).Run(ctx).
        let serve = &r["internal/app/serve.go"];
        assert!(serve.contains("func ServeCommand() cli.Command"));
        assert!(serve.contains("lifecycle.New(cfg.Lifecycle, lifecycle.WithLogger(log))"));
        assert!(serve.contains("app.Go(\"work\","));
        assert!(serve.contains("app.Run(ctx)"));
        // The config load happens INSIDE serve's Run (not in main, not in FromConfig).
        assert!(serve.contains("cfg, err := LoadConfig(ctx)"));
        // server-go is declared → the server leaf is wired + registered as an actor.
        assert!(serve.contains("server.New(cfg.Server"));
        assert!(serve.contains("srv.Register(app)"));
    }

    #[test]
    fn service_config_embeds_primitive_substructs() {
        let r = rendered(&service_spec());
        let cfg = &r["internal/app/config.go"];
        assert!(cfg.contains("Lifecycle lifecycle.Config"));
        assert!(cfg.contains("Server server.Config"));
        // FromConfig must NOT call shikumi.Load (Law 3) — the loader lives only in
        // LoadConfig, called once from the serve command.
        assert!(!cfg.contains("shikumi.Load"));
        assert!(cfg.contains("shikumi.For[Config](Name)"));
    }

    #[test]
    fn daemon_drives_refresh_loop_in_run() {
        let r = rendered(&daemon_spec());
        let main = &r["main.go"];
        assert!(main.contains("errs.Exit(run())"));
        assert!(main.contains("return borealis.Execute(ctx, root)"));
        assert!(main.contains("root.Add(app.RunCommand())"));
        let daemon = &r["internal/app/daemon.go"];
        assert!(daemon.contains("func RunCommand() cli.Command"));
        // The keep-fresh loop is built from its sub-struct + driven by Run.
        assert!(daemon.contains("refreshloop.FromConfig(cfg.Refresh)"));
        assert!(daemon.contains("loop.Register(refreshloop.Spec{"));
        assert!(daemon.contains("loop.Run(ctx, cfg.Refresh.TickInterval())"));
        // The config sub-struct + the seeded Tool default.
        let cfg = &r["internal/app/config.go"];
        assert!(cfg.contains("Refresh refreshloop.Config"));
        assert!(cfg.contains("Refresh: refreshloop.Config{"));
    }

    #[test]
    fn daemon_oneshot_ticks_once() {
        let s = spec_from(
            r#"(defgotool :name "borealis-once" :kind Daemon :oneshot #t
                :description "one-shot" :profile "nord"
                :primitives ("borealis" "cli-go" "errors-go" "logging-go" "shikumi-go" "refresh-loop-go" "shigoto-go"))"#,
        );
        assert!(s.oneshot);
        let r = rendered(&s);
        let daemon = &r["internal/app/daemon.go"];
        // One-shot: a single Tick, not Run.
        assert!(daemon.contains("loop.Tick(ctx)"));
        assert!(!daemon.contains("loop.Run(ctx"));
    }

    #[test]
    fn action_parses_inputs_and_renders_yaml() {
        let r = rendered(&action_spec());
        let main = &r["main.go"];
        assert!(main.contains("errs.Exit(run())"));
        assert!(main.contains("root.Add(app.ActionCommand())"));
        let action = &r["internal/app/action.go"];
        // The typed Inputs struct with input tags (required carried through).
        assert!(action.contains("Name string `input:\"name,required\"`"));
        assert!(action.contains("Verbose bool `input:\"verbose\"`"));
        // The runtime entrypoint parses inputs; the gen sub renders action.yml.
        assert!(action.contains("actions.ParseInputs[Inputs](&in)"));
        assert!(action.contains("ActionMeta().RenderActionYAML()"));
        // The typed metadata composes pleme-actions-shared-go.
        assert!(action.contains("actions.NewAction(\"borealis-action\""));
        assert!(action.contains("actions.RunComposite"));
        assert!(action.contains("actions.WithActionInput("));
    }

    #[test]
    fn library_defers_emits_nothing() {
        let s = spec_from(
            r#"(defgotool :name "borealis-lib" :kind Library :description "lib")"#,
        );
        // Library defers to pleme-doc-gen's scaffold — lower emits no Go files.
        assert!(lower(&s).is_empty());
    }

    #[test]
    fn binary_uses_the_proven_cli_shape() {
        let s = spec_from(
            r#"(defgotool :name "borealis-bin" :kind Binary :description "bin" :profile "nord"
                :primitives ("borealis" "cli-go" "errors-go" "logging-go" "shikumi-go")
                :commands ((:name "do" :summary "do it")))"#,
        );
        let r = rendered(&s);
        // Binary ≈ Cli: main runs the grammar through exit.Map(borealis.Execute(...)).
        let main = &r["main.go"];
        assert!(main.contains("errs.Exit(run(context.Background()))"));
        assert!(main.contains("return exit.Map(borealis.Execute(ctx, root))"));
        assert!(r.contains_key("internal/app/app.go"));
    }

    #[test]
    fn api_op_emits_abstract_client_seam_and_dispatch() {
        let s = spec_from(
            r#"(defgotool :name "borealis-api" :kind Cli :description "api" :profile "nord"
                :primitives ("borealis" "cli-go" "errors-go" "logging-go" "shikumi-go")
                :api-ops ("ListSecrets")
                :commands ((:name "list" :summary "list" :api-op "ListSecrets")))"#,
        );
        let r = rendered(&s);
        // The client seam file exists with the abstract interface + the seam sentinel.
        let client = &r["internal/app/client.go"];
        assert!(client.contains("type Client interface {"));
        assert!(client.contains("ListSecrets(ctx context.Context, req ListSecretsRequest) (ListSecretsResponse, error)"));
        assert!(client.contains("func NewClient(cfg Config) (Client, error)"));
        assert!(client.contains("return nil, errClientNotImplemented"));
        // No vendor SDK leaks into the public client seam (worlds-separate).
        assert!(!client.to_lowercase().contains("akeyless"));
        // The command dispatches through the abstract client.
        let app = &r["internal/app/app.go"];
        assert!(app.contains("client, err := NewClient(cfg)"));
        assert!(app.contains("client.ListSecrets(ctx, ListSecretsRequest{})"));
    }

    #[test]
    fn no_kind_names_akeyless() {
        // Worlds-separate: nothing the engine emits, for any kind, names akeyless.
        for spec in [service_spec(), daemon_spec(), action_spec(), proof_spec()] {
            for (_, file) in lower(&spec) {
                let src = print_file(&file).to_lowercase();
                assert!(!src.contains("akeyless"), "generated source named akeyless");
            }
        }
    }

    // ── Milestone 3: hardening — real loop, header, controller depth ──────────

    /// A no-keep-fresh-primitive daemon: the fallback drives a REAL ctx-aware
    /// ticker loop (`for { select { … } }`), not a ctx.Done() block placeholder.
    fn ticker_daemon_spec() -> GoToolSpec {
        spec_from(
            r#"(defgotool :name "borealis-ticker" :kind Daemon
                :description "ticker daemon proof" :profile "nord"
                :primitives ("borealis" "cli-go" "errors-go" "logging-go" "shikumi-go"))"#,
        )
    }

    /// A controller service: lifecycle-go + controller-go among primitives.
    fn controller_service_spec() -> GoToolSpec {
        spec_from(
            r#"(defgotool :name "borealis-controller" :kind Service
                :description "controller service proof" :profile "nord"
                :primitives ("borealis" "cli-go" "errors-go" "logging-go" "shikumi-go" "lifecycle-go" "controller-go"))"#,
        )
    }

    #[test]
    fn daemon_fallback_drives_real_ticker_loop() {
        let r = rendered(&ticker_daemon_spec());
        let daemon = &r["internal/app/daemon.go"];
        // The real loop shape: a bare for {} wrapping a select with both arms.
        assert!(daemon.contains("ticker := time.NewTicker(time.Second)"));
        assert!(daemon.contains("for {"), "expected a bare infinite for loop:\n{daemon}");
        assert!(daemon.contains("select {"), "expected a select:\n{daemon}");
        assert!(daemon.contains("case <-ctx.Done():"));
        // Cancellation arm: stop the ticker, then return ctx.Err() (clean stop).
        assert!(daemon.contains("ticker.Stop()"));
        assert!(daemon.contains("return ctx.Err()"));
        // Tick arm: the honest periodic-work seam (a structured log line).
        assert!(daemon.contains("case <-ticker.C:"));
        assert!(daemon.contains("log.InfoContext(ctx, \"borealis-ticker tick\")"));
        // The old placeholder (a bare ctx.Done() block then return nil) is gone:
        // the loop's only return is ctx.Err() on the cancellation arm.
        assert!(!daemon.contains("return nil"), "no-keep-fresh loop must not return nil placeholder:\n{daemon}");
    }

    #[test]
    fn every_file_carries_synthesizer_header_not_iac_forge() {
        // The generated-file header on EVERY emitted file is the synthesizer's own
        // provenance, parameterised by the (defgotool …) name — never iac-forge.
        for spec in [proof_spec(), service_spec(), daemon_spec(), action_spec()] {
            for (path, file) in lower(&spec) {
                let src = print_file(&file);
                let want = format!(
                    "// Code generated by go-tool-synthesizer (defgotool: {}). DO NOT EDIT.\n",
                    spec.name
                );
                assert!(
                    src.starts_with(&want),
                    "{}: header was:\n{}",
                    path.display(),
                    src.lines().next().unwrap_or("")
                );
                assert!(!src.contains("iac-forge"), "{} leaked the iac-forge stamp", path.display());
            }
        }
    }

    #[test]
    fn client_seam_interface_generated_adapter_is_separate_handwritten() {
        let s = spec_from(
            r#"(defgotool :name "borealis-api" :kind Cli :description "api" :profile "nord"
                :primitives ("borealis" "cli-go" "errors-go" "logging-go" "shikumi-go")
                :api-ops ("ListSecrets")
                :commands ((:name "list" :summary "list" :api-op "ListSecrets")))"#,
        );
        let r = rendered(&s);
        let client = &r["internal/app/client.go"];
        // The INTERFACE is generated and carries the DO NOT EDIT header.
        assert!(client.starts_with(
            "// Code generated by go-tool-synthesizer (defgotool: borealis-api). DO NOT EDIT.\n"
        ));
        assert!(client.contains("type Client interface {"));
        // The doc-comment states the concrete adapter is a SEPARATE hand-written
        // file — the adapter is NOT stamped DO NOT EDIT (regenerating never touches it).
        assert!(client.contains("SEPARATE file"));
        assert!(client.contains("hand"));
        assert!(client.contains("client_adapter.go"));
    }

    #[test]
    fn controller_service_wires_chassis_with_real_api() {
        let r = rendered(&controller_service_spec());
        let serve = &r["internal/app/serve.go"];
        // controller.New(cfg.Controller, ReconcileFunc, For(gvk), WithLogger(log)) —
        // the canonical New(cfg, Reconciler, opts...) shape from controller-go.
        assert!(serve.contains("controller.New(cfg.Controller, controller.ReconcileFunc("));
        // A watched kind is supplied (else controller.New returns ErrNoKind).
        assert!(serve.contains("controller.For(controller.GVKConfig{"));
        assert!(serve.contains("Version: \"v1\","));
        assert!(serve.contains("Kind: \"ConfigMap\","));
        // The shared logger is threaded in as an Option.
        assert!(serve.contains("controller.WithLogger(log)"));
        // The reconciler returns the typed controller.Done result.
        assert!(serve.contains("return controller.Done, nil"));
        // Run as the ctx-aware App.Go("reconcile", ctrl.Run) unit.
        assert!(serve.contains("app.Go(\"reconcile\", ctrl.Run)"));
        // The Controller sub-struct is embedded in the typed Config.
        let cfg = &r["internal/app/config.go"];
        assert!(cfg.contains("Controller controller.Config"));
    }
}
