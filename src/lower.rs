//! The core lowering engine: `GoToolSpec` → a set of typed [`GoFile`] values.
//!
//! [`lower`] turns a typed [`GoToolSpec`] into the canonical §4 three-line-tool
//! shape proven by `borealis-cli-example`:
//!
//!   - `main.go`                  — the `errs.Exit(run(ctx))` funnel + the
//!                                  load→derive→theme→grammar→execute body, with
//!                                  `exit.Map(borealis.Execute(ctx, root))` as
//!                                  the OUTERMOST entrypoint.
//!   - `internal/app/config.go`   — the typed shikumi `Config` + the canonical
//!                                  `shikumi.For[Config]…Load(ctx)` loader.
//!   - `internal/app/app.go`      — the cli-go App tree (commands → typed
//!                                  Flag[T] declarations) + borealis rendering.
//!   - `internal/app/errors.go`   — the typed errors-go vocabulary (ErrConfig
//!                                  → EX_CONFIG).
//!
//! Every node is built structurally and rendered through
//! `go_synthesizer::print_file`. There is NO `format!()` of raw Go syntax.

use std::path::PathBuf;

use go_synthesizer::{
    GoBlock, GoDecl, GoExpr, GoField, GoFile, GoFuncDecl, GoImport, GoParam, GoSelectCase, GoStmt,
    GoStructTag, GoType, GoTypeBody, GoTypeDecl, GoVarDecl, JsonTag, YamlTag,
};

use crate::spec::{CommandSpec, ConfigField, FieldType, FlagSpec, GoToolSpec, ToolKind};

// ── Primitive predicates ─────────────────────────────────────────────────────

/// True when the spec composes the named fleet primitive (e.g. `"server-go"`).
fn has_primitive(spec: &GoToolSpec, name: &str) -> bool {
    spec.primitives.iter().any(|p| p == name)
}

/// The set of api_op operationIds the tool declares (the spec-level list plus
/// any referenced by a command's `api_op`), de-duplicated and ordered. Drives
/// the abstract `app.Client` interface seam.
fn declared_api_ops(spec: &GoToolSpec) -> Vec<String> {
    let mut ops: Vec<String> = vec![];
    let mut push = |op: &str| {
        if !op.is_empty() && !ops.iter().any(|o| o == op) {
            ops.push(op.to_string());
        }
    };
    for op in &spec.api_ops {
        push(op);
    }
    fn walk<'a>(cmds: &'a [CommandSpec], out: &mut dyn FnMut(&'a str)) {
        for c in cmds {
            if let Some(op) = &c.api_op {
                out(op);
            }
            walk(&c.sub, out);
        }
    }
    walk(&spec.commands, &mut |op| push(op));
    ops
}

/// True when the tool declares any api_op — i.e. it needs the abstract
/// `app.Client` interface + the `NewClient` adapter seam.
fn uses_client_seam(spec: &GoToolSpec) -> bool {
    !declared_api_ops(spec).is_empty()
}

/// One kind-specific primitive sub-struct embedded into the tool's `Config`
/// (e.g. `Lifecycle lifecycle.Config`). Drives both the `Config` field and the
/// matching import.
struct SubStruct {
    /// The Go field name on `Config` (e.g. `"Lifecycle"`).
    field: &'static str,
    /// The yaml/json tag (e.g. `"lifecycle"`).
    yaml: &'static str,
    /// The package alias used in the qualified type + import (e.g. `"lifecycle"`).
    pkg_alias: &'static str,
    /// The import path (e.g. `"github.com/pleme-io/lifecycle-go"`).
    import_path: &'static str,
    /// The field doc comment.
    doc: &'static str,
}

/// The runtime-primitive sub-structs the tool's `Config` embeds, selected by the
/// spec's [`ToolKind`] and declared primitives. CLI/Binary/Library embed none
/// beyond logging; Service embeds Lifecycle (+ Server / Controller when those
/// primitives are declared); Daemon embeds Refresh when refresh-loop-go is
/// declared. Order is stable.
fn primitive_sub_structs(spec: &GoToolSpec) -> Vec<SubStruct> {
    let mut subs = vec![];
    match spec.kind {
        ToolKind::Service => {
            subs.push(SubStruct {
                field: "Lifecycle",
                yaml: "lifecycle",
                pkg_alias: "lifecycle",
                import_path: "github.com/pleme-io/lifecycle-go",
                doc: "Lifecycle is the lifecycle-go knob surface (signals/drain/shutdown grace) — the\nowner of the run loop, the graceful drain, and the three health planes.",
            });
            if has_primitive(spec, "server-go") {
                subs.push(SubStruct {
                    field: "Server",
                    yaml: "server",
                    pkg_alias: "server",
                    import_path: "github.com/pleme-io/server-go",
                    doc: "Server is the server-go knob surface (addr/timeouts/throttle), consumed via\nserver.New(cfg.Server, …). Mounts the health planes + the sample route.",
                });
            }
            if has_primitive(spec, "controller-go") {
                subs.push(SubStruct {
                    field: "Controller",
                    yaml: "controller",
                    pkg_alias: "controller",
                    import_path: "github.com/pleme-io/controller-go",
                    doc: "Controller is the controller-go knob surface (kind/leader-election/concurrency),\nconsumed via controller.New(cfg.Controller, reconciler).",
                });
            }
        }
        ToolKind::Daemon => {
            if has_primitive(spec, "refresh-loop-go") {
                subs.push(SubStruct {
                    field: "Refresh",
                    yaml: "refresh",
                    pkg_alias: "refreshloop",
                    import_path: "github.com/pleme-io/refresh-loop-go",
                    doc: "Refresh is the refresh-loop-go knob surface (tool tag, tick cadence, per-kind\nconcurrency caps), consumed via refreshloop.FromConfig(cfg.Refresh).",
                });
            }
        }
        ToolKind::Cli | ToolKind::Binary | ToolKind::Action | ToolKind::Library => {}
    }
    subs
}

// ── Name helpers ────────────────────────────────────────────────────────────

/// `borealis-greet` / `greeting_prefix` → `BorealisGreet` / `GreetingPrefix`.
fn pascal_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut upper = true;
    for c in s.chars() {
        if c == '-' || c == '_' || c == '.' || c == ' ' {
            upper = true;
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// `greeting_prefix` / `greeting-prefix` → `greetingPrefix`.
fn camel_case(s: &str) -> String {
    let p = pascal_case(s);
    let mut chars = p.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The local Go identifier for a flag variable inside a command body.
fn flag_var(name: &str) -> String {
    let c = camel_case(name);
    // Avoid colliding with Go builtins / keywords commonly hit by flag names.
    match c.as_str() {
        "type" | "func" | "range" | "map" | "var" | "case" => format!("{c}Flag"),
        _ => c,
    }
}

// ── Type helpers ────────────────────────────────────────────────────────────

/// The Go type for a config field. `Secret` → `shikumi.Secret[string]`.
fn config_field_type(ty: &FieldType) -> GoType {
    match ty {
        FieldType::Str => GoType::named("string"),
        FieldType::Int => GoType::named("int"),
        FieldType::Bool => GoType::named("bool"),
        // shikumi.Secret[string] — Go generic instantiation. The AST has no
        // dedicated generic-instantiation node, but a Named carrying the
        // bracketed form renders byte-correct ("shikumi.Secret[string]").
        FieldType::Secret => GoType::named("shikumi.Secret[string]"),
    }
}

/// The Go scalar type for a flag's `NewFlag[T]`.
fn flag_go_type(ty: &FieldType) -> &'static str {
    match ty {
        FieldType::Str | FieldType::Secret => "string",
        FieldType::Int => "int",
        FieldType::Bool => "bool",
    }
}

/// A default-value Go expression for a flag, typed by its scalar kind.
fn flag_default_expr(flag: &FlagSpec) -> GoExpr {
    match (&flag.ty, flag.default.as_deref()) {
        (FieldType::Int, Some(d)) => d
            .parse::<i64>()
            .map(|n| GoExpr::Lit(go_synthesizer::GoLit::Int(n)))
            .unwrap_or_else(|_| GoExpr::Lit(go_synthesizer::GoLit::Int(0))),
        (FieldType::Int, None) => GoExpr::Lit(go_synthesizer::GoLit::Int(0)),
        (FieldType::Bool, Some(d)) => {
            GoExpr::Lit(go_synthesizer::GoLit::Bool(d == "true"))
        }
        (FieldType::Bool, None) => GoExpr::Lit(go_synthesizer::GoLit::Bool(false)),
        (_, Some(d)) => GoExpr::str(d),
        (_, None) => GoExpr::str(""),
    }
}

// ── Tag helpers ─────────────────────────────────────────────────────────────

/// Build the yaml + json + (optional) validate tags for a config field.
fn config_field_tags(field: &ConfigField) -> Vec<GoStructTag> {
    let tag_name = field
        .yaml
        .clone()
        .unwrap_or_else(|| camel_case(&field.name));
    let mut tags = vec![
        GoStructTag::Yaml(YamlTag { name: tag_name.clone(), omitempty: false, inline: false }),
        GoStructTag::Json(JsonTag { name: tag_name, omitempty: false, inline: false }),
    ];
    if let Some(v) = &field.validate {
        tags.push(GoStructTag::Custom { key: "validate".into(), value: v.clone() });
    }
    tags
}

// ── Public entrypoint ───────────────────────────────────────────────────────

/// Lower a [`GoToolSpec`] to the set of `(path, GoFile)` pairs that make up the
/// generated tool's Go source. Paths are relative to the tool's repo root.
///
/// The emitted shape is selected by [`GoToolSpec::kind`] (BOREALIS §4):
///
///   - [`ToolKind::Cli`] / [`ToolKind::Binary`] — the proven three-line-tool
///     shape: `main.go` runs the cli-go grammar through `borealis.Execute`; the
///     commands call their work directly.
///   - [`ToolKind::Service`] — a `serve` subcommand whose Run nests
///     `lifecycle.New(cfg.Lifecycle, …).Go("work", run).Run(ctx)`; the
///     `server-go` / `controller-go` leaf is wired in only when that primitive
///     is declared.
///   - [`ToolKind::Daemon`] — a `run` subcommand whose Run drives a
///     `refresh-loop-go` keep-fresh loop (or a lifecycle ticker fallback),
///     one-shot or recurring per [`GoToolSpec`]'s daemon mode.
///   - [`ToolKind::Action`] — a GitHub-action entrypoint: an `action`
///     subcommand that `ParseInputs` into config + runs work, plus a
///     `gen-action-yml` capability composing `pleme-actions-shared-go`.
///   - [`ToolKind::Library`] — no `main`; the scaffolder (pleme-doc-gen) owns
///     the library shape, so `lower` emits nothing and defers (see GAPS).
///
/// Every node is built structurally and rendered through
/// `go_synthesizer::print_file`. There is NO `format!()` of raw Go syntax.
#[must_use]
pub fn lower(spec: &GoToolSpec) -> Vec<(PathBuf, GoFile)> {
    let mut files = match spec.kind {
        ToolKind::Library => lower_library(spec),
        ToolKind::Service => lower_service(spec),
        ToolKind::Daemon => lower_daemon(spec),
        ToolKind::Action => lower_action(spec),
        // Cli and Binary share the proven leaf shape.
        ToolKind::Cli | ToolKind::Binary => lower_cli(spec),
    };
    // Stamp the generated-file header on EVERY emitted file in one place (one
    // shape, no per-emitter drift): `go-tool-synthesizer (defgotool: <name>)`.
    // This replaces the historical iac-forge default so a synthesized tool no
    // longer carries the wrong provenance stamp.
    let header = generated_by_header(spec);
    for (_, f) in &mut files {
        f.generated_by = Some(header.clone());
    }
    files
}

/// The generated-file header tool name stamped on every emitted file:
/// `go-tool-synthesizer (defgotool: <spec.name>)`. The printer renders it as
/// `// Code generated by go-tool-synthesizer (defgotool: <name>). DO NOT EDIT.`,
/// so a reader (and the fleet's drift checker) sees the correct provenance — this
/// engine, the specific (defgotool …) form — instead of the inherited iac-forge
/// stamp.
fn generated_by_header(spec: &GoToolSpec) -> String {
    format!("go-tool-synthesizer (defgotool: {})", spec.name)
}

/// The shared base files every executable kind emits: the typed config, the
/// errors vocabulary, and (when the spec declares api_ops) the abstract
/// `app.Client` seam. The CLI grammar (`app.go` + its smoke test) is emitted for
/// the leaf-CLI shape; the composition kinds (Service/Daemon/Action) carry their
/// command in their own composition-root file and only need the `Version` const,
/// so they get a lean `app.go` + a matching smoke test.
fn base_app_files(spec: &GoToolSpec) -> Vec<(PathBuf, GoFile)> {
    let is_cli = matches!(spec.kind, ToolKind::Cli | ToolKind::Binary);
    let mut files = vec![
        (PathBuf::from("internal/app/config.go"), build_config(spec)),
        (PathBuf::from("internal/app/errors.go"), build_errors(spec)),
    ];
    if is_cli {
        files.push((PathBuf::from("internal/app/app.go"), build_app(spec)));
        files.push((PathBuf::from("internal/app/app_test.go"), build_app_test(spec)));
    } else {
        files.push((PathBuf::from("internal/app/app.go"), build_kind_app(spec)));
        files.push((PathBuf::from("internal/app/app_test.go"), build_kind_app_test(spec)));
    }
    if uses_client_seam(spec) {
        files.push((PathBuf::from("internal/app/client.go"), build_client(spec)));
    }
    files
}

/// Build a lean `internal/app/app.go` for a composition kind (Service / Daemon
/// / Action): just the `Version` const (the kind command lives in its own
/// composition-root file). `Name`/`EnvPrefix` come from config.go.
fn build_kind_app(_spec: &GoToolSpec) -> GoFile {
    let mut f = GoFile::new("app");
    f.doc = Some(
        "Package app holds the tool's typed config, errors vocabulary, and the kind-specific\n\
         composition root (the serve/run/action command). Version is the build-stamped\n\
         identity surfaced in help and logs."
            .into(),
    );
    f.decls.push(GoDecl::Var(GoVarDecl {
        name: "Version".into(),
        ty: None,
        value: Some(GoExpr::str("0.1.0")),
        doc: Some(
            "Version is the tool version, surfaced by the --version flag and the borealis help\n\
             footer. Overridable at build time via ldflags."
                .into(),
        ),
        block_id: None,
    }));
    f
}

/// Build the green smoke test for a composition kind: it exercises the real
/// config wiring (DefaultConfig + LoadConfig-free DefaultConfig) and asserts the
/// kind command builds non-empty. Keeps `go test ./...` green by default while
/// touching the generated composition root.
fn build_kind_app_test(spec: &GoToolSpec) -> GoFile {
    let (cmd_builder, _) = match spec.kind {
        ToolKind::Service => ("ServeCommand", "serve"),
        ToolKind::Daemon => ("RunCommand", "run"),
        _ => ("ActionCommand", "action"),
    };
    let mut f = GoFile::new("app");
    f.imports = vec![GoImport::plain("testing")];

    // cmd := <Cmd>Command(); if cmd.Name == "" { t.Fatal(...) }
    let mut body = GoBlock::new();
    body.push(GoStmt::ShortDecl {
        names: vec!["cmd".into()],
        values: vec![GoExpr::call(GoExpr::ident(cmd_builder), vec![])],
    });
    let mut if_body = GoBlock::new();
    if_body.push(GoStmt::Expr(GoExpr::call(
        GoExpr::sel(GoExpr::ident("t"), "Fatal"),
        vec![GoExpr::str("command builder returned an unnamed command")],
    )));
    body.push(GoStmt::If {
        init: None,
        cond: GoExpr::binary("==", GoExpr::sel(GoExpr::ident("cmd"), "Name"), GoExpr::str("")),
        body: if_body,
        else_body: None,
    });
    // _ = DefaultConfig() — touch the config baseline.
    body.push(GoStmt::Assign {
        lhs: vec![GoExpr::ident("_")],
        rhs: vec![GoExpr::call(GoExpr::ident("DefaultConfig"), vec![])],
    });

    f.decls.push(GoDecl::Func(GoFuncDecl {
        name: "TestCommandBuilds".into(),
        doc: Some(
            "TestCommandBuilds is the green-by-default smoke test: it builds the kind command\n\
             and exercises the config baseline, asserting the command is named (non-empty)."
                .into(),
        ),
        recv: None,
        params: vec![GoParam {
            name: "t".into(),
            ty: GoType::pointer(GoType::qualified("testing", "T")),
        }],
        returns: vec![],
        body,
    }));
    f
}

/// The proven leaf-CLI / bare-binary shape (the M1 vertical).
fn lower_cli(spec: &GoToolSpec) -> Vec<(PathBuf, GoFile)> {
    let mut files = vec![(PathBuf::from("main.go"), build_main(spec))];
    files.extend(base_app_files(spec));
    files
}

/// The library shape: no `main`. pleme-doc-gen's existing library scaffold owns
/// it, so `lower` defers entirely (emits no Go files) and the GAPS note records
/// the hand-off. Returning an empty vec leaves the scaffolder's starter in place.
fn lower_library(_spec: &GoToolSpec) -> Vec<(PathBuf, GoFile)> {
    vec![]
}

// ── main.go ─────────────────────────────────────────────────────────────────

/// Build `main.go` — the §3.5 funnel + §4 worked shape, verbatim to the
/// exemplar:
///
/// ```go
/// func main() { errs.Exit(run(context.Background())) }
/// func run(ctx context.Context) error {
///   cfg, err := app.LoadConfig(ctx); if err != nil { return app.ErrConfig(err) }
///   log, err := logging.FromConfig(cfg.Logging); if err != nil { return app.ErrConfig(err) }
///   theme := borealis.Nord()
///   root := app.New(cfg, log, theme)
///   return exit.Map(borealis.Execute(ctx, root))
/// }
/// ```
fn build_main(spec: &GoToolSpec) -> GoFile {
    let module = spec.resolved_module_path();
    let mut f = GoFile::new("main");
    f.doc = Some(format!(
        "Command {name} is a borealis-profiled Go tool generated by go-tool-synthesizer.\n\
         \n\
         It is the §4 three-line-tool shape: load → derive → theme → grammar → execute → exit.\n\
         The entrypoint (borealis.Execute) is outermost; errs.Exit is the single exit funnel.",
        name = spec.name
    ));
    f.imports = vec![
        GoImport::plain("context"),
        GoImport::plain("github.com/pleme-io/borealis"),
        GoImport::plain("github.com/pleme-io/cli-go/exit"),
        GoImport::aliased("errs", "github.com/pleme-io/errors-go"),
        GoImport::aliased("logging", "github.com/pleme-io/logging-go"),
        GoImport::plain(format!("{module}/internal/app")),
    ];

    // func main() { errs.Exit(run(context.Background())) }
    let mut main_body = GoBlock::new();
    main_body.push(GoStmt::Comment(
        "One funnel, at main: run() returns a typed error, errs.Exit reduces it to a\n\
         deterministic exit code and terminates (BOREALIS §3.5)."
            .to_string(),
    ));
    main_body.push(GoStmt::Expr(GoExpr::call(
        GoExpr::path(&["errs", "Exit"]),
        vec![GoExpr::call(
            GoExpr::ident("run"),
            vec![GoExpr::call(GoExpr::path(&["context", "Background"]), vec![])],
        )],
    )));
    f.decls.push(GoDecl::Func(GoFuncDecl {
        name: "main".into(),
        doc: None,
        recv: None,
        params: vec![],
        returns: vec![],
        body: main_body,
    }));

    f.decls.push(GoDecl::Func(build_run(spec)));
    f
}

/// The `run(ctx context.Context) error` body — the load→derive→theme→grammar→
/// execute shape.
fn build_run(spec: &GoToolSpec) -> GoFuncDecl {
    let mut body = GoBlock::new();

    // cfg, err := app.LoadConfig(ctx)
    body.push(GoStmt::Comment(
        "1. config — loaded once via the canonical shikumi loader (Law 3).".into(),
    ));
    body.push(GoStmt::ShortDecl {
        names: vec!["cfg".into(), "err".into()],
        values: vec![GoExpr::call(
            GoExpr::path(&["app", "LoadConfig"]),
            vec![GoExpr::ident("ctx")],
        )],
    });
    body.push(err_check_return(GoExpr::call(
        GoExpr::path(&["app", "ErrConfig"]),
        vec![GoExpr::ident("err")],
    )));
    body.push(GoStmt::Blank);

    // log, err := logging.FromConfig(cfg.Logging)
    body.push(GoStmt::Comment(
        "2. logging — a pure function of the shikumi sub-struct (Law 3, §2.3).".into(),
    ));
    body.push(GoStmt::ShortDecl {
        names: vec!["log".into(), "err".into()],
        values: vec![GoExpr::call(
            GoExpr::path(&["logging", "FromConfig"]),
            vec![GoExpr::path(&["cfg", "Logging"])],
        )],
    });
    body.push(err_check_return(GoExpr::call(
        GoExpr::path(&["app", "ErrConfig"]),
        vec![GoExpr::ident("err")],
    )));
    body.push(GoStmt::Blank);

    // theme := borealis.Nord()  (or .Tundra())
    body.push(GoStmt::Comment(
        "3. theme — the resolved borealis token bundle, one name fleet-wide.".into(),
    ));
    body.push(GoStmt::ShortDecl {
        names: vec!["theme".into()],
        values: vec![GoExpr::call(
            GoExpr::path(&["borealis", spec.theme_constructor()]),
            vec![],
        )],
    });
    body.push(GoStmt::Blank);

    // root := app.New(cfg, log, theme)
    body.push(GoStmt::Comment(
        "4. grammar — the typed cli-go App (single source of truth for parse + help).".into(),
    ));
    body.push(GoStmt::ShortDecl {
        names: vec!["root".into()],
        values: vec![GoExpr::call(
            GoExpr::path(&["app", "New"]),
            vec![GoExpr::ident("cfg"), GoExpr::ident("log"), GoExpr::ident("theme")],
        )],
    });
    body.push(GoStmt::Blank);

    // return exit.Map(borealis.Execute(ctx, root))
    body.push(GoStmt::Comment(
        "5. entrypoint — borealis.Execute is ALWAYS outermost; its result is mapped\n\
         through cli-go's exit adapter so usage/help sentinels become errors-go exit\n\
         codes before the single errs.Exit funnel reduces them (BOREALIS §3.5)."
            .into(),
    ));
    body.push(GoStmt::Return(vec![GoExpr::call(
        GoExpr::path(&["exit", "Map"]),
        vec![GoExpr::call(
            GoExpr::path(&["borealis", "Execute"]),
            vec![GoExpr::ident("ctx"), GoExpr::ident("root")],
        )],
    )]));

    GoFuncDecl {
        name: "run".into(),
        doc: Some("run is the worked section-4 shape.".into()),
        recv: None,
        params: vec![GoParam {
            name: "ctx".into(),
            ty: GoType::qualified("context", "Context"),
        }],
        returns: vec![GoType::named("error")],
        body,
    }
}

/// `if err != nil { return <expr> }`.
fn err_check_return(ret: GoExpr) -> GoStmt {
    let mut body = GoBlock::new();
    body.push(GoStmt::Return(vec![ret]));
    GoStmt::If {
        init: None,
        cond: GoExpr::binary("!=", GoExpr::ident("err"), GoExpr::nil()),
        body,
        else_body: None,
    }
}

// ── internal/app/config.go ──────────────────────────────────────────────────

/// Build `internal/app/config.go` — the typed shikumi Config + the canonical
/// `shikumi.For[Config](Name).EnvPrefix(…).Defaults(…).Validate(…).Load(ctx)`
/// loader. Mirrors the exemplar exactly.
fn build_config(spec: &GoToolSpec) -> GoFile {
    let mut f = GoFile::new("app");
    f.imports = vec![
        GoImport::plain("context"),
        GoImport::aliased("logging", "github.com/pleme-io/logging-go"),
        GoImport::aliased("shikumi", "github.com/pleme-io/shikumi-go"),
        GoImport::plain("github.com/pleme-io/shikumi-go/validate"),
    ];
    // The kind-specific primitive sub-structs each pull in their own import.
    for sub in primitive_sub_structs(spec) {
        f.imports.push(GoImport::aliased(sub.pkg_alias, sub.import_path));
    }

    // const Name = "<name>"
    f.decls.push(GoDecl::Var(GoVarDecl {
        name: "Name".into(),
        ty: None,
        value: Some(GoExpr::str(&spec.name)),
        doc: Some(
            "Name is the canonical tool name — the single token threaded into the shikumi\n\
             loader (config discovery dir / env prefix base) and the CLI root (App name)."
                .into(),
        ),
        block_id: None,
    }));
    // The var decl above is rendered as `var Name = "…"`. Go has no top-level
    // immutable `const` node in the AST; `var` is semantically equivalent for
    // a package-level string and renders cleanly.

    // const EnvPrefix = "<NAME>_"
    f.decls.push(GoDecl::Var(GoVarDecl {
        name: "EnvPrefix".into(),
        ty: None,
        value: Some(GoExpr::str(spec.env_prefix())),
        doc: Some(
            "EnvPrefix is the env-var namespace for the typed config (Law 3: env override of\n\
             every load-bearing knob)."
                .into(),
        ),
        block_id: None,
    }));

    // type Config struct { ... }
    f.decls.push(GoDecl::Type(build_config_struct(spec)));

    // func DefaultConfig() Config { ... }
    f.decls.push(GoDecl::Func(build_default_config(spec)));

    // func LoadConfig(ctx context.Context) (Config, error) { ... }
    f.decls.push(GoDecl::Func(build_load_config()));

    f
}

fn build_config_struct(spec: &GoToolSpec) -> GoTypeDecl {
    let mut fields: Vec<GoField> = spec
        .config_fields
        .iter()
        .map(|cf| GoField {
            name: Some(pascal_case(&cf.name)),
            ty: config_field_type(&cf.ty),
            doc: None,
            markers: vec![],
            tags: config_field_tags(cf),
        })
        .collect();

    // Every tool embeds the logging-go sub-struct, consumed by
    // logging.FromConfig (BOREALIS §2.3). Named field `Logging`.
    fields.push(GoField {
        name: Some("Logging".into()),
        ty: GoType::qualified("logging", "Config"),
        doc: Some(
            "Logging is the logging-go sub-struct, consumed by logging.FromConfig\n\
             (BOREALIS §2.3) — one root config owns every primitive's knobs."
                .into(),
        ),
        markers: vec![],
        tags: vec![
            GoStructTag::Yaml(YamlTag { name: "logging".into(), omitempty: false, inline: false }),
            GoStructTag::Json(JsonTag { name: "logging".into(), omitempty: false, inline: false }),
        ],
    });

    // Kind-specific primitive sub-structs (Lifecycle / Server / Refresh /
    // Controller) — one named field per composed runtime primitive, each
    // consumed via its canonical FromConfig(cfg.<Sub>) in the composition root.
    for sub in primitive_sub_structs(spec) {
        fields.push(GoField {
            name: Some(sub.field.into()),
            ty: GoType::qualified(sub.pkg_alias, "Config"),
            doc: Some(sub.doc.into()),
            markers: vec![],
            tags: vec![
                GoStructTag::Yaml(YamlTag { name: sub.yaml.into(), omitempty: false, inline: false }),
                GoStructTag::Json(JsonTag { name: sub.yaml.into(), omitempty: false, inline: false }),
            ],
        });
    }

    GoTypeDecl {
        name: "Config".into(),
        doc: Some(
            "Config is the typed, yaml-tagged root config (BOREALIS Law 3). It is loaded once\n\
             via shikumi.For[Config]; each primitive consumes its sub-struct through FromConfig."
                .into(),
        ),
        markers: vec![],
        body: GoTypeBody::Struct(fields),
    }
}

fn build_default_config(spec: &GoToolSpec) -> GoFuncDecl {
    // Seed defaults for string fields that carry a `validate:"required"` so the
    // happy path passes the validator (mirrors the exemplar's Greeting="Hello").
    let mut composite_fields: Vec<(Option<String>, GoExpr)> = vec![];
    for cf in &spec.config_fields {
        if matches!(cf.ty, FieldType::Str) && cf.validate.as_deref() == Some("required") {
            composite_fields.push((
                Some(pascal_case(&cf.name)),
                GoExpr::str(default_seed_for(&cf.name)),
            ));
        }
    }
    // For the daemon's refresh-loop-go sub-struct, seed the required Tool tag
    // (an empty Tool fails refreshloop.FromConfig) with the tool name, so the
    // happy path builds the loop without a config file.
    if spec.kind == ToolKind::Daemon && has_primitive(spec, "refresh-loop-go") {
        composite_fields.push((
            Some("Refresh".into()),
            GoExpr::Composite {
                ty: GoType::qualified("refreshloop", "Config"),
                fields: vec![(Some("Tool".into()), GoExpr::str(&spec.name))],
                addr_of: false,
            },
        ));
    }

    let mut body = GoBlock::new();
    body.push(GoStmt::Return(vec![GoExpr::Composite {
        ty: GoType::named("Config"),
        fields: composite_fields,
        addr_of: false,
    }]));

    GoFuncDecl {
        name: "DefaultConfig".into(),
        doc: Some(
            "DefaultConfig is the typed lowest-precedence baseline seeded into the loader\n\
             (the canonical shikumi Defaults layer). Env and file layers override it."
                .into(),
        ),
        recv: None,
        params: vec![],
        returns: vec![GoType::named("Config")],
        body,
    }
}

/// A presentational default seed for a required string field.
fn default_seed_for(name: &str) -> String {
    match name {
        "greeting" => "Hello".to_string(),
        "locale" => "en".to_string(),
        other => pascal_case(other),
    }
}

fn build_load_config() -> GoFuncDecl {
    // return shikumi.For[Config](Name).EnvPrefix(EnvPrefix).Defaults(DefaultConfig()).Validate(validate.New()).Load(ctx)
    //
    // The fluent builder is a chained selector/call. We build it inside-out.
    let for_call = GoExpr::call(
        // shikumi.For[Config] — the generic instantiation rendered as a Named
        // selector target. We model `shikumi.For[Config]` as a Selector whose
        // sel carries the bracketed generic, so it renders byte-correct.
        GoExpr::Selector {
            recv: Box::new(GoExpr::ident("shikumi")),
            sel: "For[Config]".into(),
        },
        vec![GoExpr::ident("Name")],
    );
    let env_call = GoExpr::call(
        GoExpr::sel(for_call, "EnvPrefix"),
        vec![GoExpr::ident("EnvPrefix")],
    );
    let defaults_call = GoExpr::call(
        GoExpr::sel(env_call, "Defaults"),
        vec![GoExpr::call(GoExpr::ident("DefaultConfig"), vec![])],
    );
    let validate_call = GoExpr::call(
        GoExpr::sel(defaults_call, "Validate"),
        vec![GoExpr::call(GoExpr::path(&["validate", "New"]), vec![])],
    );
    let load_call = GoExpr::call(
        GoExpr::sel(validate_call, "Load"),
        vec![GoExpr::ident("ctx")],
    );

    let mut body = GoBlock::new();
    body.push(GoStmt::Return(vec![load_call]));

    GoFuncDecl {
        name: "LoadConfig".into(),
        doc: Some(
            "LoadConfig is the single config load for the tool — the canonical\n\
             shikumi.For[Root](name)…Load(ctx) call (BOREALIS §3.5). Called once from\n\
             the command layer, never inside a FromConfig constructor."
                .into(),
        ),
        recv: None,
        params: vec![GoParam {
            name: "ctx".into(),
            ty: GoType::qualified("context", "Context"),
        }],
        returns: vec![GoType::named("Config"), GoType::named("error")],
        body,
    }
}

// ── internal/app/errors.go ──────────────────────────────────────────────────

/// Build `internal/app/errors.go` — the typed errors-go vocabulary.
fn build_errors(_spec: &GoToolSpec) -> GoFile {
    let mut f = GoFile::new("app");
    f.imports = vec![GoImport::aliased("errs", "github.com/pleme-io/errors-go")];

    // func ErrConfig(cause error) error { return errs.Build()...Wrap(cause, "...") }
    let chain = GoExpr::call(
        GoExpr::sel(
            GoExpr::call(
                GoExpr::sel(
                    GoExpr::call(
                        GoExpr::sel(
                            GoExpr::call(
                                GoExpr::sel(
                                    GoExpr::call(GoExpr::path(&["errs", "Build"]), vec![]),
                                    "Code",
                                ),
                                vec![GoExpr::str("E_CONFIG")],
                            ),
                            "ExitCode",
                        ),
                        vec![GoExpr::path(&["errs", "ExitConfig"])],
                    ),
                    "Public",
                ),
                vec![GoExpr::str("invalid configuration")],
            ),
            "Wrap",
        ),
        vec![GoExpr::ident("cause"), GoExpr::str("load configuration")],
    );

    let mut body = GoBlock::new();
    body.push(GoStmt::Return(vec![chain]));

    f.decls.push(GoDecl::Func(GoFuncDecl {
        name: "ErrConfig".into(),
        doc: Some(
            "ErrConfig wraps a configuration-load/validation failure and maps it to EX_CONFIG\n\
             (78) via errs.WithExitCode, so the single errs.Exit funnel in main reduces the\n\
             failure deterministically (BOREALIS §2.4 / §3.5)."
                .into(),
        ),
        recv: None,
        params: vec![GoParam { name: "cause".into(), ty: GoType::named("error") }],
        returns: vec![GoType::named("error")],
        body,
    }));

    f
}

// ── internal/app/app.go ─────────────────────────────────────────────────────

/// Build `internal/app/app.go` — the cli-go App tree + borealis rendering.
fn build_app(spec: &GoToolSpec) -> GoFile {
    let mut f = GoFile::new("app");
    f.doc = Some(
        "Package app builds the command tree — the worked §4 shape: a typed shikumi Config\n\
         loaded once, a logging-go logger, a cli-go App whose subcommands carry typed\n\
         Flag[T] declarations, and output rendered through the one borealis.Render verb."
            .into(),
    );
    f.imports = vec![
        GoImport::plain("context"),
        GoImport::plain("flag"),
        GoImport::plain("fmt"),
        GoImport::plain("log/slog"),
        GoImport::plain("github.com/pleme-io/borealis"),
        GoImport::plain("github.com/pleme-io/borealis/comp"),
        GoImport::aliased("cli", "github.com/pleme-io/cli-go"),
    ];

    // var Version = "0.1.0"
    f.decls.push(GoDecl::Var(GoVarDecl {
        name: "Version".into(),
        ty: None,
        value: Some(GoExpr::str("0.1.0")),
        doc: Some(
            "Version is the tool version, surfaced by the version command and the borealis\n\
             help footer. Overridable at build time via ldflags."
                .into(),
        ),
        block_id: None,
    }));

    // func New(cfg Config, log *slog.Logger, theme borealis.Theme) *cli.App
    f.decls.push(GoDecl::Func(build_new(spec)));

    // versionCmd + configShowCmd + one builder per command
    f.decls.push(GoDecl::Func(build_version_cmd()));
    f.decls.push(GoDecl::Func(build_config_show_cmd(spec)));
    f.decls.push(GoDecl::Func(build_config_pairs(spec)));
    for cmd in &spec.commands {
        f.decls.push(GoDecl::Func(build_command_fn(spec, cmd)));
    }

    f
}

/// `func New(cfg Config, log *slog.Logger, theme borealis.Theme) *cli.App`.
fn build_new(spec: &GoToolSpec) -> GoFuncDecl {
    let mut body = GoBlock::new();
    // root := cli.NewApp(Name, cli.WithVersion(Version), cli.WithDescription("..."))
    body.push(GoStmt::ShortDecl {
        names: vec!["root".into()],
        values: vec![GoExpr::call(
            GoExpr::path(&["cli", "NewApp"]),
            vec![
                GoExpr::ident("Name"),
                GoExpr::call(
                    GoExpr::path(&["cli", "WithVersion"]),
                    vec![GoExpr::ident("Version")],
                ),
                GoExpr::call(
                    GoExpr::path(&["cli", "WithDescription"]),
                    vec![GoExpr::str(&spec.description)],
                ),
            ],
        )],
    });

    // root.Add(versionCmd(theme), configShowCmd(cfg, theme), <each command>(...))
    let mut add_args = vec![
        GoExpr::call(GoExpr::ident("versionCmd"), vec![GoExpr::ident("theme")]),
        GoExpr::call(
            GoExpr::ident("configShowCmd"),
            vec![GoExpr::ident("cfg"), GoExpr::ident("theme")],
        ),
    ];
    for cmd in &spec.commands {
        add_args.push(GoExpr::call(
            GoExpr::ident(command_fn_name(&cmd.name)),
            command_fn_call_args(),
        ));
    }
    body.push(GoStmt::Expr(GoExpr::call(
        GoExpr::sel(GoExpr::ident("root"), "Add"),
        add_args,
    )));
    body.push(GoStmt::Return(vec![GoExpr::ident("root")]));

    GoFuncDecl {
        name: "New".into(),
        doc: Some(
            "New builds the fully-wired *cli.App from an already-loaded Config, a logger, and\n\
             the resolved borealis Theme. It is the single source of truth for the command\n\
             grammar; borealis.Execute (in main) lowers it to cobra under the themed decorator."
                .into(),
        ),
        recv: None,
        params: vec![
            GoParam { name: "cfg".into(), ty: GoType::named("Config") },
            GoParam {
                name: "log".into(),
                ty: GoType::pointer(GoType::qualified("slog", "Logger")),
            },
            GoParam { name: "theme".into(), ty: GoType::qualified("borealis", "Theme") },
        ],
        returns: vec![GoType::pointer(GoType::qualified("cli", "App"))],
        body,
    }
}

/// The arguments every command builder takes: (cfg, log, theme).
fn command_fn_call_args() -> Vec<GoExpr> {
    vec![
        GoExpr::ident("cfg"),
        GoExpr::ident("log"),
        GoExpr::ident("theme"),
    ]
}

fn command_fn_name(cmd_name: &str) -> String {
    format!("{}Cmd", camel_case(cmd_name))
}

/// `func versionCmd(theme borealis.Theme) cli.Command`.
fn build_version_cmd() -> GoFuncDecl {
    // borealis.Render(theme, []comp.Pair{ {K:"tool",V:Name}, {K:"version",V:Version} })
    let pairs = GoExpr::SliceLit {
        elem_type: GoType::qualified("comp", "Pair"),
        elements: vec![
            GoExpr::Composite {
                ty: GoType::qualified("comp", "Pair"),
                fields: vec![
                    (Some("K".into()), GoExpr::str("tool")),
                    (Some("V".into()), GoExpr::ident("Name")),
                ],
                addr_of: false,
            },
            GoExpr::Composite {
                ty: GoType::qualified("comp", "Pair"),
                fields: vec![
                    (Some("K".into()), GoExpr::str("version")),
                    (Some("V".into()), GoExpr::ident("Version")),
                ],
                addr_of: false,
            },
        ],
    };
    let run_body = {
        let mut b = GoBlock::new();
        b.push(GoStmt::Expr(GoExpr::call(
            GoExpr::path(&["fmt", "Println"]),
            vec![GoExpr::call(
                GoExpr::path(&["borealis", "Render"]),
                vec![GoExpr::ident("theme"), pairs],
            )],
        )));
        b.push(GoStmt::Return(vec![GoExpr::nil()]));
        b
    };

    let mut body = GoBlock::new();
    body.push(GoStmt::Return(vec![GoExpr::Composite {
        ty: GoType::qualified("cli", "Command"),
        fields: vec![
            (Some("Name".into()), GoExpr::str("version")),
            (Some("Summary".into()), GoExpr::str("print the tool version and identity")),
            (
                Some("Long".into()),
                GoExpr::str("Print the tool name and version, rendered through the borealis design system."),
            ),
            (Some("Run".into()), run_closure(run_body)),
        ],
        addr_of: false,
    }]));

    GoFuncDecl {
        name: "versionCmd".into(),
        doc: Some(
            "versionCmd renders the tool's identity as a borealis KV block via the one render\n\
             verb (borealis.Render over []comp.Pair), never hand-formatted fmt (BOREALIS Law 4)."
                .into(),
        ),
        recv: None,
        params: vec![GoParam { name: "theme".into(), ty: GoType::qualified("borealis", "Theme") }],
        returns: vec![GoType::qualified("cli", "Command")],
        body,
    }
}

/// `func configShowCmd(cfg Config, theme borealis.Theme) cli.Command`.
fn build_config_show_cmd(_spec: &GoToolSpec) -> GoFuncDecl {
    let show_run_body = {
        let mut b = GoBlock::new();
        b.push(GoStmt::Expr(GoExpr::call(
            GoExpr::path(&["fmt", "Println"]),
            vec![GoExpr::call(
                GoExpr::path(&["borealis", "Render"]),
                vec![
                    GoExpr::ident("theme"),
                    GoExpr::call(GoExpr::ident("configPairs"), vec![GoExpr::ident("cfg")]),
                ],
            )],
        )));
        b.push(GoStmt::Return(vec![GoExpr::nil()]));
        b
    };

    let show_cmd = GoExpr::Composite {
        ty: GoType::qualified("cli", "Command"),
        fields: vec![
            (Some("Name".into()), GoExpr::str("show")),
            (Some("Summary".into()), GoExpr::str("render the effective config (secrets redacted)")),
            (
                Some("Long".into()),
                GoExpr::str("Render the effective shikumi config as a key/value block. Secret-typed fields render as [REDACTED]."),
            ),
            (Some("Run".into()), run_closure(show_run_body)),
        ],
        addr_of: false,
    };

    let mut body = GoBlock::new();
    body.push(GoStmt::Return(vec![GoExpr::Composite {
        ty: GoType::qualified("cli", "Command"),
        fields: vec![
            (Some("Name".into()), GoExpr::str("config")),
            (Some("Summary".into()), GoExpr::str("configuration introspection")),
            (
                Some("Long".into()),
                GoExpr::str("Inspect the effective configuration loaded via shikumi (defaults → env → file)."),
            ),
            (
                Some("Sub".into()),
                GoExpr::SliceLit {
                    elem_type: GoType::qualified("cli", "Command"),
                    elements: vec![show_cmd],
                },
            ),
        ],
        addr_of: false,
    }]));

    GoFuncDecl {
        name: "configShowCmd".into(),
        doc: Some(
            "configShowCmd renders the effective shikumi config through borealis, with any\n\
             secret-typed field redacted by construction (shikumi.Secret.String) (BOREALIS §2.1)."
                .into(),
        ),
        recv: None,
        params: vec![
            GoParam { name: "cfg".into(), ty: GoType::named("Config") },
            GoParam { name: "theme".into(), ty: GoType::qualified("borealis", "Theme") },
        ],
        returns: vec![GoType::qualified("cli", "Command")],
        body,
    }
}

/// `func configPairs(cfg Config) []comp.Pair` — projects Config to renderable
/// pairs, with the logging level/format always shown and any Secret redacted.
fn build_config_pairs(spec: &GoToolSpec) -> GoFuncDecl {
    let mut elements: Vec<GoExpr> = vec![];
    for cf in &spec.config_fields {
        let field = pascal_case(&cf.name);
        let key = cf.yaml.clone().unwrap_or_else(|| camel_case(&cf.name));
        let value_expr = match cf.ty {
            // Secret renders through its redacting String() — never Expose().
            FieldType::Secret => GoExpr::call(
                GoExpr::sel(GoExpr::path(&["cfg", &field]), "String"),
                vec![],
            ),
            FieldType::Str => GoExpr::path(&["cfg", &field]),
            // Non-string scalars: render via fmt.Sprint to keep V a string.
            FieldType::Int | FieldType::Bool => GoExpr::call(
                GoExpr::path(&["fmt", "Sprint"]),
                vec![GoExpr::path(&["cfg", &field])],
            ),
        };
        elements.push(GoExpr::Composite {
            ty: GoType::qualified("comp", "Pair"),
            fields: vec![
                (Some("K".into()), GoExpr::str(key)),
                (Some("V".into()), value_expr),
            ],
            addr_of: false,
        });
    }
    // Always surface the logging knobs.
    elements.push(GoExpr::Composite {
        ty: GoType::qualified("comp", "Pair"),
        fields: vec![
            (Some("K".into()), GoExpr::str("logging.level")),
            (Some("V".into()), GoExpr::path(&["cfg", "Logging", "Level"])),
        ],
        addr_of: false,
    });
    elements.push(GoExpr::Composite {
        ty: GoType::qualified("comp", "Pair"),
        fields: vec![
            (Some("K".into()), GoExpr::str("logging.format")),
            (Some("V".into()), GoExpr::path(&["cfg", "Logging", "Format"])),
        ],
        addr_of: false,
    });

    let mut body = GoBlock::new();
    body.push(GoStmt::Return(vec![GoExpr::SliceLit {
        elem_type: GoType::qualified("comp", "Pair"),
        elements,
    }]));

    GoFuncDecl {
        name: "configPairs".into(),
        doc: Some(
            "configPairs projects the typed Config onto the renderable []comp.Pair the borealis\n\
             KV component consumes. Secret-typed fields render via their redacting String()."
                .into(),
        ),
        recv: None,
        params: vec![GoParam { name: "cfg".into(), ty: GoType::named("Config") }],
        returns: vec![GoType::slice(GoType::qualified("comp", "Pair"))],
        body,
    }
}

/// Build one command builder function: `func <name>Cmd(cfg Config, log
/// *slog.Logger, theme borealis.Theme) cli.Command`.
fn build_command_fn(spec: &GoToolSpec, cmd: &CommandSpec) -> GoFuncDecl {
    let mut body = GoBlock::new();

    // Flag declarations: <var> := cli.NewFlag[T]("name", default, "usage")[.Env(...)][.Validate(...)]
    for flag in &cmd.flags {
        body.push(GoStmt::ShortDecl {
            names: vec![flag_var(&flag.name)],
            values: vec![build_flag_expr(spec, flag)],
        });
    }
    if !cmd.flags.is_empty() {
        body.push(GoStmt::Blank);
    }

    // The Run closure body.
    let run_body = build_command_run_body(spec, cmd);

    // Assemble the cli.Command composite.
    let mut fields: Vec<(Option<String>, GoExpr)> = vec![
        (Some("Name".into()), GoExpr::str(&cmd.name)),
        (Some("Summary".into()), GoExpr::str(&cmd.summary)),
    ];
    if let Some(long) = &cmd.long {
        fields.push((Some("Long".into()), GoExpr::str(long)));
    }
    // Flags: func(fs *flag.FlagSet) { <var>.Bind(fs); ... }
    if !cmd.flags.is_empty() {
        fields.push((Some("Flags".into()), build_flags_closure(cmd)));
    }
    fields.push((Some("Run".into()), run_closure(run_body)));

    body.push(GoStmt::Return(vec![GoExpr::Composite {
        ty: GoType::qualified("cli", "Command"),
        fields,
        addr_of: false,
    }]));

    GoFuncDecl {
        name: command_fn_name(&cmd.name),
        doc: Some(format!(
            "{fn} builds the `{name}` command — typed Flag[T] declarations, a structured slog\n\
             line, and output rendered through the one borealis.Render verb.",
            fn = command_fn_name(&cmd.name),
            name = cmd.name
        )),
        recv: None,
        params: vec![
            GoParam { name: "cfg".into(), ty: GoType::named("Config") },
            GoParam {
                name: "log".into(),
                ty: GoType::pointer(GoType::qualified("slog", "Logger")),
            },
            GoParam { name: "theme".into(), ty: GoType::qualified("borealis", "Theme") },
        ],
        returns: vec![GoType::qualified("cli", "Command")],
        body,
    }
}

/// `cli.NewFlag[T]("name", default, "usage").Env(EnvPrefix+"NAME")[.Validate(...)]`.
fn build_flag_expr(_spec: &GoToolSpec, flag: &FlagSpec) -> GoExpr {
    let go_t = flag_go_type(&flag.ty);
    let usage = flag.usage.clone().unwrap_or_else(|| format!("the {} flag", flag.name));
    // cli.NewFlag[T] — generic instantiation as a Selector sel carrying the
    // bracketed form, so it renders byte-correct: `cli.NewFlag[string]`.
    let new_flag = GoExpr::call(
        GoExpr::Selector {
            recv: Box::new(GoExpr::ident("cli")),
            sel: format!("NewFlag[{go_t}]"),
        },
        vec![
            GoExpr::str(&flag.name),
            flag_default_expr(flag),
            GoExpr::str(usage),
        ],
    );
    // .Env(EnvPrefix + "NAME")
    let env_suffix = flag
        .name
        .chars()
        .map(|c| if c == '-' { '_' } else { c.to_ascii_uppercase() })
        .collect::<String>();
    let mut chain = GoExpr::call(
        GoExpr::sel(new_flag, "Env"),
        vec![GoExpr::binary(
            "+",
            GoExpr::ident("EnvPrefix"),
            GoExpr::str(env_suffix),
        )],
    );
    // .Validate(func(v T) error { if v == "" { return fmt.Errorf("...") }; return nil })
    if flag.require_non_empty && matches!(flag.ty, FieldType::Str | FieldType::Secret) {
        chain = GoExpr::call(GoExpr::sel(chain, "Validate"), vec![non_empty_validator()]);
    }
    chain
}

/// `func(v string) error { if v == "" { return fmt.Errorf("must not be empty") }; return nil }`.
fn non_empty_validator() -> GoExpr {
    let mut if_body = GoBlock::new();
    if_body.push(GoStmt::Return(vec![GoExpr::call(
        GoExpr::path(&["fmt", "Errorf"]),
        vec![GoExpr::str("must not be empty")],
    )]));
    let mut body = GoBlock::new();
    body.push(GoStmt::If {
        init: None,
        cond: GoExpr::binary("==", GoExpr::ident("v"), GoExpr::str("")),
        body: if_body,
        else_body: None,
    });
    body.push(GoStmt::Return(vec![GoExpr::nil()]));
    closure(vec![GoParam { name: "v".into(), ty: GoType::named("string") }], vec![GoType::named("error")], body)
}

/// `func(fs *flag.FlagSet) { <var>.Bind(fs); ... }`.
fn build_flags_closure(cmd: &CommandSpec) -> GoExpr {
    let mut body = GoBlock::new();
    for flag in &cmd.flags {
        body.push(GoStmt::Expr(GoExpr::call(
            GoExpr::sel(GoExpr::ident(flag_var(&flag.name)), "Bind"),
            vec![GoExpr::ident("fs")],
        )));
    }
    closure(
        vec![GoParam {
            name: "fs".into(),
            ty: GoType::pointer(GoType::qualified("flag", "FlagSet")),
        }],
        vec![],
        body,
    )
}

/// The body of a command's Run closure: a structured slog line + a themed
/// borealis render of a Success status item.
fn build_command_run_body(_spec: &GoToolSpec, cmd: &CommandSpec) -> GoBlock {
    let mut b = GoBlock::new();

    // Pull flag values: who := nameFlag.Get()
    let mut slog_args: Vec<GoExpr> = vec![GoExpr::str(&cmd.name)];
    for flag in &cmd.flags {
        let var = flag_var(&flag.name);
        let val_var = format!("{var}Val");
        b.push(GoStmt::ShortDecl {
            names: vec![val_var.clone()],
            values: vec![GoExpr::call(GoExpr::sel(GoExpr::ident(&var), "Get"), vec![])],
        });
        // slog typed attr per flag, keyed by flag name.
        let attr_fn = match flag.ty {
            FieldType::Int => "Int",
            FieldType::Bool => "Bool",
            _ => "String",
        };
        slog_args.push(GoExpr::call(
            GoExpr::path(&["slog", attr_fn]),
            vec![GoExpr::str(&flag.name), GoExpr::ident(&val_var)],
        ));
    }
    b.push(GoStmt::Blank);

    // log.InfoContext(ctx, "<name>", slog.String(...), ...)
    b.push(GoStmt::Comment(
        "Structured, logged action (BOREALIS §2.3) — the action is observed via slog.".into(),
    ));
    b.push(GoStmt::Expr(GoExpr::call(
        GoExpr::sel(GoExpr::ident("log"), "InfoContext"),
        {
            let mut args = vec![GoExpr::ident("ctx")];
            args.extend(slog_args);
            args
        },
    )));
    b.push(GoStmt::Blank);

    // api_op dispatch: build the abstract Client and call the typed operation.
    // The concrete client is supplied by a PRIVATE adapter (M3 absorption); the
    // generated tool only knows the interface (worlds-separate).
    if let Some(op) = &cmd.api_op {
        let op_pascal = pascal_case(op);
        b.push(GoStmt::Comment(
            "Dispatch through the abstract app.Client (one method per api_op). The concrete\n\
             client is a PRIVATE adapter supplied later (M3) — the public tool never names a\n\
             vendor SDK; here NewClient returns the not-yet-implemented seam sentinel."
                .into(),
        ));
        // client, err := NewClient(cfg)
        b.push(GoStmt::ShortDecl {
            names: vec!["client".into(), "err".into()],
            values: vec![GoExpr::call(GoExpr::ident("NewClient"), vec![GoExpr::ident("cfg")])],
        });
        b.push(err_check_return(GoExpr::call(
            GoExpr::ident("ErrConfig"),
            vec![GoExpr::ident("err")],
        )));
        // if _, err := client.<Op>(ctx, <Op>Request{}); err != nil { return err }
        let mut call_if = GoBlock::new();
        call_if.push(GoStmt::Return(vec![GoExpr::ident("err")]));
        b.push(GoStmt::If {
            init: Some(Box::new(GoStmt::ShortDecl {
                names: vec!["_".into(), "err".into()],
                values: vec![GoExpr::call(
                    GoExpr::sel(GoExpr::ident("client"), &op_pascal),
                    vec![
                        GoExpr::ident("ctx"),
                        GoExpr::Composite {
                            ty: GoType::named(format!("{op_pascal}Request")),
                            fields: vec![],
                            addr_of: false,
                        },
                    ],
                )],
            })),
            cond: GoExpr::binary("!=", GoExpr::ident("err"), GoExpr::nil()),
            body: call_if,
            else_body: None,
        });
        b.push(GoStmt::Blank);
    }

    // Build the rendered message. If there is a "name" flag and a "greeting"
    // config field, compose them; otherwise render a generic success label.
    let label_expr = build_command_label(cmd);

    // fmt.Println(borealis.Render(theme, []comp.Item{ {Role: borealis.Success, Label: <label>} }))
    b.push(GoStmt::Comment(
        "Rendered through the one verb as a Success status row, not fmt-printed (Law 4).".into(),
    ));
    b.push(GoStmt::Expr(GoExpr::call(
        GoExpr::path(&["fmt", "Println"]),
        vec![GoExpr::call(
            GoExpr::path(&["borealis", "Render"]),
            vec![
                GoExpr::ident("theme"),
                GoExpr::SliceLit {
                    elem_type: GoType::qualified("comp", "Item"),
                    elements: vec![GoExpr::Composite {
                        ty: GoType::qualified("comp", "Item"),
                        fields: vec![
                            (Some("Role".into()), GoExpr::path(&["borealis", "Success"])),
                            (Some("Label".into()), label_expr),
                        ],
                        addr_of: false,
                    }],
                },
            ],
        )],
    )));
    b.push(GoStmt::Return(vec![GoExpr::nil()]));
    b
}

/// The rendered label expression for a command. When the command has a `name`
/// flag, compose a greeting from any `greeting` config field + the name;
/// otherwise emit a static "<name> ok" label.
fn build_command_label(cmd: &CommandSpec) -> GoExpr {
    let has_name_flag = cmd.flags.iter().any(|fl| fl.name == "name");
    if has_name_flag {
        // cfg.Greeting + ", " + nameVal + "!"
        let name_val = format!("{}Val", flag_var("name"));
        GoExpr::binary(
            "+",
            GoExpr::binary(
                "+",
                GoExpr::binary(
                    "+",
                    GoExpr::path(&["cfg", "Greeting"]),
                    GoExpr::str(", "),
                ),
                GoExpr::ident(name_val),
            ),
            GoExpr::str("!"),
        )
    } else {
        GoExpr::str(format!("{} ok", cmd.name))
    }
}

// ── internal/app/app_test.go ────────────────────────────────────────────────

/// Build a green smoke test that exercises the real wiring: construct the App
/// from the typed defaults + a logger + the profile's theme, and assert it is
/// non-nil. Ships `go test ./...` green by default while genuinely touching
/// New + DefaultConfig + the borealis theme (not a vacuous 2+2 assertion).
fn build_app_test(spec: &GoToolSpec) -> GoFile {
    let mut f = GoFile::new("app");
    f.imports = vec![
        GoImport::plain("testing"),
        GoImport::plain("github.com/pleme-io/borealis"),
    ];

    // root := New(DefaultConfig(), nil, borealis.Nord())
    let mut body = GoBlock::new();
    body.push(GoStmt::ShortDecl {
        names: vec!["root".into()],
        values: vec![GoExpr::call(
            GoExpr::ident("New"),
            vec![
                GoExpr::call(GoExpr::ident("DefaultConfig"), vec![]),
                // A nil *slog.Logger is fine — New only stores it; the smoke
                // test does not invoke a command Run (which would deref it).
                GoExpr::nil(),
                GoExpr::call(GoExpr::path(&["borealis", spec.theme_constructor()]), vec![]),
            ],
        )],
    });
    // if root == nil { t.Fatal("New returned nil App") }
    let mut if_body = GoBlock::new();
    if_body.push(GoStmt::Expr(GoExpr::call(
        GoExpr::sel(GoExpr::ident("t"), "Fatal"),
        vec![GoExpr::str("New returned a nil App")],
    )));
    body.push(GoStmt::If {
        init: None,
        cond: GoExpr::binary("==", GoExpr::ident("root"), GoExpr::nil()),
        body: if_body,
        else_body: None,
    });

    f.decls.push(GoDecl::Func(GoFuncDecl {
        name: "TestNewBuildsApp".into(),
        doc: Some(
            "TestNewBuildsApp is the green-by-default smoke test: it exercises the real\n\
             wiring (DefaultConfig + New + the profile theme) and asserts a non-nil App."
                .into(),
        ),
        recv: None,
        params: vec![GoParam {
            name: "t".into(),
            ty: GoType::pointer(GoType::qualified("testing", "T")),
        }],
        returns: vec![],
        body,
    }));

    f
}

// ── Kind main.go (service / daemon / action) ─────────────────────────────────

/// Build a `main.go` for a kind whose root grammar adds a single composition
/// subcommand (`serve` / `run` / `action`) and runs it through the OUTERMOST
/// `borealis.Execute`. Mirrors the proven borealis-service-example main:
///
/// ```go
/// func main() { errs.Exit(run()) }
/// func run() error {
///   ctx := context.Background()
///   root := cli.NewApp(appName, cli.WithVersion(version), cli.WithDescription("…"))
///   root.Add(app.<Cmd>Command(ctx))
///   return borealis.Execute(ctx, root)
/// }
/// ```
fn build_kind_main(spec: &GoToolSpec, cmd_builder: &str, kind_label: &str) -> GoFile {
    let module = spec.resolved_module_path();
    let mut f = GoFile::new("main");
    f.doc = Some(format!(
        "Command {name} is a borealis-profiled Go {kind} generated by go-tool-synthesizer.\n\
         \n\
         It is the §4 {kind} shape: borealis.Execute is the OUTERMOST entrypoint over the\n\
         typed cli-go App; the {builder} subcommand's Run is the composition root where the\n\
         lifecycle/loop nests; errs.Exit(run()) is the single process-exit funnel.",
        name = spec.name,
        kind = kind_label,
        builder = cmd_builder,
    ));
    f.imports = vec![
        GoImport::plain("context"),
        GoImport::plain("github.com/pleme-io/borealis"),
        GoImport::aliased("cli", "github.com/pleme-io/cli-go"),
        GoImport::aliased("errs", "github.com/pleme-io/errors-go"),
        GoImport::plain(format!("{module}/internal/app")),
    ];

    // func main() { errs.Exit(run()) }
    let mut main_body = GoBlock::new();
    main_body.push(GoStmt::Comment(
        "One funnel, at main: run() returns a typed error, errs.Exit reduces it to a\n\
         deterministic exit code and terminates (BOREALIS §3.5)."
            .to_string(),
    ));
    main_body.push(GoStmt::Expr(GoExpr::call(
        GoExpr::path(&["errs", "Exit"]),
        vec![GoExpr::call(GoExpr::ident("run"), vec![])],
    )));
    f.decls.push(GoDecl::Func(GoFuncDecl {
        name: "main".into(),
        doc: None,
        recv: None,
        params: vec![],
        returns: vec![],
        body: main_body,
    }));

    // func run() error { … borealis.Execute(ctx, root) }
    let mut body = GoBlock::new();
    body.push(GoStmt::ShortDecl {
        names: vec!["ctx".into()],
        values: vec![GoExpr::call(GoExpr::path(&["context", "Background"]), vec![])],
    });
    body.push(GoStmt::Blank);
    body.push(GoStmt::Comment(
        "The typed cli-go App is the single source of truth for parse + help; the\n\
         composition subcommand carries the run loop."
            .into(),
    ));
    body.push(GoStmt::ShortDecl {
        names: vec!["root".into()],
        values: vec![GoExpr::call(
            GoExpr::path(&["cli", "NewApp"]),
            vec![
                GoExpr::path(&["app", "Name"]),
                GoExpr::call(
                    GoExpr::path(&["cli", "WithVersion"]),
                    vec![GoExpr::path(&["app", "Version"])],
                ),
                GoExpr::call(
                    GoExpr::path(&["cli", "WithDescription"]),
                    vec![GoExpr::str(&spec.description)],
                ),
            ],
        )],
    });
    body.push(GoStmt::Expr(GoExpr::call(
        GoExpr::sel(GoExpr::ident("root"), "Add"),
        vec![GoExpr::call(
            GoExpr::path(&["app", cmd_builder]),
            vec![],
        )],
    )));
    body.push(GoStmt::Blank);
    body.push(GoStmt::Comment(
        "borealis.Execute is ALWAYS outermost (BOREALIS §3.5); its result funnels through\n\
         errs.Exit in main."
            .into(),
    ));
    body.push(GoStmt::Return(vec![GoExpr::call(
        GoExpr::path(&["borealis", "Execute"]),
        vec![GoExpr::ident("ctx"), GoExpr::ident("root")],
    )]));

    f.decls.push(GoDecl::Func(GoFuncDecl {
        name: "run".into(),
        doc: Some(
            "run builds the typed cli-go App and hands it to borealis.Execute — the single\n\
             outermost entrypoint. The error it returns is reduced by errs.Exit in main."
                .into(),
        ),
        recv: None,
        params: vec![],
        returns: vec![GoType::named("error")],
        body,
    }));
    f
}

// ── Service ───────────────────────────────────────────────────────────────

/// Lower a [`ToolKind::Service`] tool: the proven §4 service shape — a `serve`
/// subcommand whose Run nests `lifecycle.New(cfg.Lifecycle, …).Go("work",
/// run).Run(ctx)`. The server-go / controller-go leaf is wired only when that
/// primitive is declared.
fn lower_service(spec: &GoToolSpec) -> Vec<(PathBuf, GoFile)> {
    let mut files = vec![(PathBuf::from("main.go"), build_kind_main(spec, "ServeCommand", "service"))];
    files.extend(base_app_files(spec));
    files.push((PathBuf::from("internal/app/serve.go"), build_serve(spec)));
    files
}

/// Build `internal/app/serve.go` — the `ServeCommand()` builder + the `Serve`
/// composition root. The composition root loads the config once, builds the
/// logger, and nests the lifecycle owner with a ctx-aware `work` Go-routine.
fn build_serve(spec: &GoToolSpec) -> GoFile {
    let wires_server = has_primitive(spec, "server-go");
    let wires_controller = has_primitive(spec, "controller-go");

    let mut f = GoFile::new("app");
    f.doc = Some(
        "ServeCommand + Serve are the §4 SERVICE shape: a serve subcommand whose Run loads\n\
         the typed config once, builds the logger, and nests the lifecycle-go owner — the\n\
         single owner of the run loop, graceful drain, and the three health planes — with the\n\
         service's work registered as a ctx-aware App.Go unit. borealis.Execute stays\n\
         outermost (see package main); the lifecycle App.Run loop nests INSIDE serve."
            .into(),
    );
    let mut imports = vec![
        GoImport::plain("context"),
        GoImport::plain("flag"),
        GoImport::aliased("errs", "github.com/pleme-io/errors-go"),
        GoImport::aliased("lifecycle", "github.com/pleme-io/lifecycle-go"),
        GoImport::aliased("logging", "github.com/pleme-io/logging-go"),
        GoImport::aliased("cli", "github.com/pleme-io/cli-go"),
    ];
    if wires_server {
        imports.push(GoImport::plain("net/http"));
        imports.push(GoImport::aliased("server", "github.com/pleme-io/server-go"));
    }
    if wires_controller {
        imports.push(GoImport::aliased("controller", "github.com/pleme-io/controller-go"));
    }
    f.imports = imports;

    // serveCommand builder.
    f.decls.push(GoDecl::Func(build_serve_command(spec)));
    // The Serve composition root.
    f.decls.push(GoDecl::Func(build_serve_fn(spec, wires_server, wires_controller)));
    f
}

/// `func ServeCommand() cli.Command { … Run: …{ cfg, err := LoadConfig(ctx); … return Serve(ctx, cfg) } }`.
fn build_serve_command(spec: &GoToolSpec) -> GoFuncDecl {
    let mut run_body = GoBlock::new();
    run_body.push(GoStmt::Comment(
        "Load the ONE typed config via the canonical shikumi loader (Law 3), then call the\n\
         composition root. The load happens here, inside Run, never inside a FromConfig."
            .into(),
    ));
    run_body.push(GoStmt::ShortDecl {
        names: vec!["cfg".into(), "err".into()],
        values: vec![GoExpr::call(GoExpr::ident("LoadConfig"), vec![GoExpr::ident("ctx")])],
    });
    run_body.push(err_check_return(GoExpr::call(
        GoExpr::ident("ErrConfig"),
        vec![GoExpr::ident("err")],
    )));
    run_body.push(GoStmt::Return(vec![GoExpr::call(
        GoExpr::ident("Serve"),
        vec![GoExpr::ident("ctx"), GoExpr::ident("cfg")],
    )]));

    let mut body = GoBlock::new();
    body.push(GoStmt::Return(vec![GoExpr::Composite {
        ty: GoType::qualified("cli", "Command"),
        fields: vec![
            (Some("Name".into()), GoExpr::str("serve")),
            (Some("Summary".into()), GoExpr::str("run the service: lifecycle-owned run loop + health planes")),
            (
                Some("Long".into()),
                GoExpr::str(
                    "serve loads the typed config via shikumi (defaults > env > file), builds the \
                     logging-go logger, and runs the lifecycle-go owner which owns the run loop, the \
                     graceful drain, and the /livez|/readyz|/startupz health planes. Stop it with \
                     SIGINT/SIGTERM for a graceful drain.",
                ),
            ),
            (Some("Run".into()), run_closure(run_body)),
        ],
        addr_of: false,
    }]));

    GoFuncDecl {
        name: "ServeCommand".into(),
        doc: Some(format!(
            "ServeCommand is {name}'s serve subcommand. Its Run is the §4 service body: load the\n\
             ONE typed config via shikumi, then call the Serve composition root which runs the\n\
             lifecycle owner. borealis.Execute (in main) is the outermost shell.",
            name = spec.name,
        )),
        recv: None,
        params: vec![],
        returns: vec![GoType::qualified("cli", "Command")],
        body,
    }
}

/// `func Serve(ctx context.Context, cfg Config) error { … lifecycle.New(...).Go("work", …).Run(ctx) }`.
fn build_serve_fn(spec: &GoToolSpec, wires_server: bool, wires_controller: bool) -> GoFuncDecl {
    let mut body = GoBlock::new();

    // log, err := logging.FromConfig(cfg.Logging)
    body.push(GoStmt::Comment(
        "logging — a pure function of the shikumi sub-struct (Law 3, §2.3).".into(),
    ));
    body.push(GoStmt::ShortDecl {
        names: vec!["log".into(), "err".into()],
        values: vec![GoExpr::call(
            GoExpr::path(&["logging", "FromConfig"]),
            vec![GoExpr::path(&["cfg", "Logging"])],
        )],
    });
    body.push(err_check_return(GoExpr::call(
        GoExpr::ident("ErrConfig"),
        vec![GoExpr::ident("err")],
    )));
    body.push(GoStmt::Blank);

    // app, err := lifecycle.New(cfg.Lifecycle, lifecycle.WithLogger(log))
    body.push(GoStmt::Comment(
        "lifecycle-go is the single owner of the run loop, graceful drain, and the three\n\
         health planes (§2.5). It is built from its sub-struct via the canonical New."
            .into(),
    ));
    body.push(GoStmt::ShortDecl {
        names: vec!["app".into(), "err".into()],
        values: vec![GoExpr::call(
            GoExpr::path(&["lifecycle", "New"]),
            vec![
                GoExpr::path(&["cfg", "Lifecycle"]),
                GoExpr::call(
                    GoExpr::path(&["lifecycle", "WithLogger"]),
                    vec![GoExpr::ident("log")],
                ),
            ],
        )],
    });
    body.push(err_check_return(GoExpr::call(
        GoExpr::ident("ErrConfig"),
        vec![GoExpr::ident("err")],
    )));
    body.push(GoStmt::Blank);

    if wires_server {
        // srv, err := server.New(cfg.Server, server.WithLogger(log), server.WithHandler("GET /healthz", …))
        body.push(GoStmt::Comment(
            "server-go is declared: build the inbound HTTP leaf and register it as a lifecycle\n\
             ACTOR (the oklog/run shape — net.Listener.Accept cannot watch a ctx). It mounts the\n\
             health planes on its shared mux; lifecycle owns the choreography (§2.7)."
                .into(),
        ));
        body.push(GoStmt::ShortDecl {
            names: vec!["srv".into(), "err".into()],
            values: vec![GoExpr::call(
                GoExpr::path(&["server", "New"]),
                vec![
                    GoExpr::path(&["cfg", "Server"]),
                    GoExpr::call(
                        GoExpr::path(&["server", "WithLogger"]),
                        vec![GoExpr::ident("log")],
                    ),
                    GoExpr::call(
                        GoExpr::path(&["server", "WithHandler"]),
                        vec![GoExpr::str("GET /hello"), helloHandlerExpr()],
                    ),
                ],
            )],
        });
        body.push(err_check_return(GoExpr::call(
            GoExpr::ident("ErrConfig"),
            vec![GoExpr::ident("err")],
        )));
        body.push(GoStmt::Expr(GoExpr::call(
            GoExpr::sel(GoExpr::ident("srv"), "Register"),
            vec![GoExpr::ident("app")],
        )));
        body.push(GoStmt::Blank);
    }

    if wires_controller {
        // ctrl, err := controller.New(cfg.Controller, reconciler, controller.For(gvk), controller.WithLogger(log))
        body.push(GoStmt::Comment(
            "controller-go is declared: build the reconcile chassis via the canonical\n\
             controller.New(cfg, Reconciler, opts...) — a ReconcileFunc reconciler, a watched\n\
             kind (controller.For; controller.New returns ErrNoKind without one), and the shared\n\
             logger. Then run it as the ctx-aware App.Go(\"reconcile\", ctrl.Run) unit: the\n\
             controller owns its manager + work queue; lifecycle owns the spine (§2.7)."
                .into(),
        ));
        body.push(GoStmt::ShortDecl {
            names: vec!["ctrl".into(), "err".into()],
            values: vec![GoExpr::call(
                GoExpr::path(&["controller", "New"]),
                vec![
                    GoExpr::path(&["cfg", "Controller"]),
                    reconcileFuncExpr(),
                    GoExpr::call(
                        GoExpr::path(&["controller", "For"]),
                        vec![watchedGVKExpr()],
                    ),
                    GoExpr::call(
                        GoExpr::path(&["controller", "WithLogger"]),
                        vec![GoExpr::ident("log")],
                    ),
                ],
            )],
        });
        body.push(err_check_return(GoExpr::call(
            GoExpr::ident("ErrConfig"),
            vec![GoExpr::ident("err")],
        )));
        body.push(GoStmt::Expr(GoExpr::call(
            GoExpr::sel(GoExpr::ident("app"), "Go"),
            vec![
                GoExpr::str("reconcile"),
                GoExpr::sel(GoExpr::ident("ctrl"), "Run"),
            ],
        )));
        body.push(GoStmt::Blank);
    }

    // app.Go("work", func(ctx) error { … }) — the canonical ctx-aware work unit.
    body.push(GoStmt::Comment(
        "The service's work is a ctx-aware App.Go unit: lifecycle owns its lifetime, so a\n\
         SIGINT/SIGTERM cancels its ctx after the readiness-down + drain window and it returns\n\
         nil for a clean stop. This is the BOREALIS §4 lifecycle.New(…).Go(\"work\", run).Run(ctx)."
            .into(),
    ));
    body.push(GoStmt::Expr(GoExpr::call(
        GoExpr::sel(GoExpr::ident("app"), "Go"),
        vec![GoExpr::str("work"), workClosureExpr()],
    )));
    body.push(GoStmt::Blank);

    body.push(GoStmt::Expr(GoExpr::call(
        GoExpr::sel(GoExpr::ident("log"), "InfoContext"),
        vec![
            GoExpr::ident("ctx"),
            GoExpr::str(format!("{} starting", spec.name)),
        ],
    )));
    body.push(GoStmt::Blank);

    // if err := app.Run(ctx); err != nil { return errs.Wrap(...) }
    body.push(GoStmt::Comment(
        "app.Run blocks until a signal (or fatal work error), then runs the graceful\n\
         shutdown choreography. A clean signalled shutdown returns nil."
            .into(),
    ));
    let mut run_if = GoBlock::new();
    run_if.push(GoStmt::Return(vec![GoExpr::call(
        GoExpr::path(&["errs", "Wrap"]),
        vec![
            GoExpr::ident("err"),
            GoExpr::str("service run loop"),
            GoExpr::call(
                GoExpr::path(&["errs", "WithExitCode"]),
                vec![GoExpr::path(&["errs", "ExitUnavailable"])],
            ),
        ],
    )]));
    body.push(GoStmt::If {
        init: Some(Box::new(GoStmt::ShortDecl {
            names: vec!["err".into()],
            values: vec![GoExpr::call(
                GoExpr::sel(GoExpr::ident("app"), "Run"),
                vec![GoExpr::ident("ctx")],
            )],
        })),
        cond: GoExpr::binary("!=", GoExpr::ident("err"), GoExpr::nil()),
        body: run_if,
        else_body: None,
    });
    body.push(GoStmt::Return(vec![GoExpr::nil()]));

    GoFuncDecl {
        name: "Serve".into(),
        doc: Some(
            "Serve is the SERVICE composition root (theory/BOREALIS.md §4). Given an\n\
             already-loaded Config it builds the logger, the lifecycle owner, and (when the\n\
             primitive is declared) the server-go / controller-go leaf, registers the service's\n\
             work as a ctx-aware App.Go unit, and runs the lifecycle owner — no os.Exit here."
                .into(),
        ),
        recv: None,
        params: vec![
            GoParam { name: "ctx".into(), ty: GoType::qualified("context", "Context") },
            GoParam { name: "cfg".into(), ty: GoType::named("Config") },
        ],
        returns: vec![GoType::named("error")],
        body,
    }
}

/// The sample `http.Handler` for the server-go leaf — a stdlib handler so the
/// service stays "composition over framework" (server-go owns the chain).
#[allow(non_snake_case)]
fn helloHandlerExpr() -> GoExpr {
    let mut body = GoBlock::new();
    body.push(GoStmt::Expr(GoExpr::call(
        GoExpr::sel(GoExpr::ident("w"), "WriteHeader"),
        vec![GoExpr::path(&["http", "StatusOK"])],
    )));
    body.push(GoStmt::Assign {
        lhs: vec![GoExpr::ident("_"), GoExpr::ident("_")],
        rhs: vec![GoExpr::call(
            GoExpr::sel(GoExpr::ident("w"), "Write"),
            vec![GoExpr::call(
                GoExpr::TypeExpr(GoType::slice(GoType::named("byte"))),
                vec![GoExpr::str("ok")],
            )],
        )],
    });
    GoExpr::call(
        GoExpr::path(&["http", "HandlerFunc"]),
        vec![closure(
            vec![
                GoParam { name: "w".into(), ty: GoType::qualified("http", "ResponseWriter") },
                GoParam { name: "_".into(), ty: GoType::pointer(GoType::qualified("http", "Request")) },
            ],
            vec![],
            body,
        )],
    )
}

/// The watched-kind selector for the controller-go leaf — a generic
/// `controller.GVKConfig{Group: "", Version: "v1", Kind: "ConfigMap"}`. It names a
/// core-group kind the controller-runtime default scheme already knows, so the
/// chassis builds without registering custom types — `controller.New` would return
/// `ErrNoKind` without a watched kind (or a `Config.Kind`). A real operator
/// overrides this to its own CRD GVK (or sets `cfg.Controller.Kind` in yaml).
#[allow(non_snake_case)]
fn watchedGVKExpr() -> GoExpr {
    GoExpr::Composite {
        ty: GoType::qualified("controller", "GVKConfig"),
        fields: vec![
            (Some("Group".into()), GoExpr::str("")),
            (Some("Version".into()), GoExpr::str("v1")),
            (Some("Kind".into()), GoExpr::str("ConfigMap")),
        ],
        addr_of: false,
    }
}

/// The sample reconcile body for the controller-go leaf — a `ReconcileFunc`
/// that returns `controller.Done`.
#[allow(non_snake_case)]
fn reconcileFuncExpr() -> GoExpr {
    let mut body = GoBlock::new();
    body.push(GoStmt::Return(vec![GoExpr::path(&["controller", "Done"]), GoExpr::nil()]));
    GoExpr::call(
        GoExpr::path(&["controller", "ReconcileFunc"]),
        vec![closure(
            vec![
                GoParam { name: "_".into(), ty: GoType::qualified("context", "Context") },
                GoParam { name: "_".into(), ty: GoType::qualified("controller", "Request") },
            ],
            vec![GoType::qualified("controller", "Result"), GoType::named("error")],
            body,
        )],
    )
}

/// The ctx-aware `work` unit body passed to `app.Go("work", …)`. It blocks on
/// ctx (the lifecycle owner cancels it on shutdown) and returns nil for a clean
/// stop — the minimal honest ctx-aware unit (a real service replaces the body).
#[allow(non_snake_case)]
fn workClosureExpr() -> GoExpr {
    let mut body = GoBlock::new();
    body.push(GoStmt::Comment(
        "Block until the lifecycle owner cancels us (post-drain) for a clean stop. A real\n\
         service replaces this body with its work; the ctx-aware shape is the contract."
            .into(),
    ));
    // <-ctx.Done()
    body.push(GoStmt::Expr(GoExpr::Receive(Box::new(GoExpr::call(
        GoExpr::sel(GoExpr::ident("ctx"), "Done"),
        vec![],
    )))));
    body.push(GoStmt::Return(vec![GoExpr::nil()]));
    closure(
        vec![GoParam { name: "ctx".into(), ty: GoType::qualified("context", "Context") }],
        vec![GoType::named("error")],
        body,
    )
}

// ── Daemon ──────────────────────────────────────────────────────────────────

/// Lower a [`ToolKind::Daemon`] tool: a `run` subcommand whose Run drives a
/// long-running keep-fresh loop. When `refresh-loop-go` is declared the loop is
/// a `refreshloop.FromConfig(…)` + `loop.Run(ctx, interval)`; otherwise a
/// lifecycle Go-routine with a ticker. `spec.oneshot` selects a single tick
/// then exit.
fn lower_daemon(spec: &GoToolSpec) -> Vec<(PathBuf, GoFile)> {
    let mut files = vec![(PathBuf::from("main.go"), build_kind_main(spec, "RunCommand", "daemon"))];
    files.extend(base_app_files(spec));
    files.push((PathBuf::from("internal/app/daemon.go"), build_daemon(spec)));
    files
}

/// Build `internal/app/daemon.go` — the `RunCommand()` builder + the `Run`
/// composition root driving the keep-fresh loop.
fn build_daemon(spec: &GoToolSpec) -> GoFile {
    let uses_refresh = has_primitive(spec, "refresh-loop-go");

    let mut f = GoFile::new("app");
    f.doc = Some(
        "RunCommand + Run are the §4 DAEMON shape: a run subcommand whose Run loads the typed\n\
         config once, builds the logger, and drives a long-running keep-fresh loop. When\n\
         refresh-loop-go is declared the loop is the keep-fresh FSM (refreshloop.Run); the\n\
         lifecycle choreography (signal → drain → ordered teardown) is the fleet contract."
            .into(),
    );
    let mut imports = vec![
        GoImport::plain("context"),
        GoImport::plain("flag"),
        GoImport::plain("time"),
        GoImport::aliased("errs", "github.com/pleme-io/errors-go"),
        GoImport::aliased("logging", "github.com/pleme-io/logging-go"),
        GoImport::aliased("cli", "github.com/pleme-io/cli-go"),
    ];
    if uses_refresh {
        imports.push(GoImport::aliased("refreshloop", "github.com/pleme-io/refresh-loop-go"));
    }
    f.imports = imports;

    f.decls.push(GoDecl::Func(build_daemon_command(spec)));
    f.decls.push(GoDecl::Func(build_daemon_fn(spec, uses_refresh)));
    f
}

/// `func RunCommand() cli.Command { … Run: …{ cfg, err := LoadConfig(ctx); … return Run(ctx, cfg) } }`.
fn build_daemon_command(spec: &GoToolSpec) -> GoFuncDecl {
    let mut run_body = GoBlock::new();
    run_body.push(GoStmt::Comment(
        "Load the ONE typed config via the canonical shikumi loader (Law 3), then call the\n\
         daemon composition root."
            .into(),
    ));
    run_body.push(GoStmt::ShortDecl {
        names: vec!["cfg".into(), "err".into()],
        values: vec![GoExpr::call(GoExpr::ident("LoadConfig"), vec![GoExpr::ident("ctx")])],
    });
    run_body.push(err_check_return(GoExpr::call(
        GoExpr::ident("ErrConfig"),
        vec![GoExpr::ident("err")],
    )));
    run_body.push(GoStmt::Return(vec![GoExpr::call(
        GoExpr::ident("Run"),
        vec![GoExpr::ident("ctx"), GoExpr::ident("cfg")],
    )]));

    let summary = if spec.oneshot {
        "run the daemon's keep-fresh loop ONCE, then exit (one-shot)"
    } else {
        "run the keep-fresh daemon: drive the loop on a wall-clock cadence"
    };

    let mut body = GoBlock::new();
    body.push(GoStmt::Return(vec![GoExpr::Composite {
        ty: GoType::qualified("cli", "Command"),
        fields: vec![
            (Some("Name".into()), GoExpr::str("run")),
            (Some("Summary".into()), GoExpr::str(summary)),
            (
                Some("Long".into()),
                GoExpr::str(
                    "run loads the typed config via shikumi (defaults > env > file), builds the \
                     logging-go logger, and drives the keep-fresh loop. Stop it with SIGINT/SIGTERM \
                     for a clean stop (ctx cancellation drains the in-flight tick).",
                ),
            ),
            (Some("Run".into()), run_closure(run_body)),
        ],
        addr_of: false,
    }]));

    GoFuncDecl {
        name: "RunCommand".into(),
        doc: Some(format!(
            "RunCommand is {name}'s run subcommand. Its Run is the §4 daemon body: load the ONE\n\
             typed config via shikumi, then call the Run composition root which drives the\n\
             keep-fresh loop. borealis.Execute (in main) is the outermost shell.",
            name = spec.name,
        )),
        recv: None,
        params: vec![],
        returns: vec![GoType::qualified("cli", "Command")],
        body,
    }
}

/// One honest tick of the no-keep-fresh-primitive fallback daemon loop: a
/// structured log line marking the periodic work. It is the behaviour seam — a
/// real daemon replaces this single statement with its idempotent periodic work
/// (often a PRIVATE vendor adapter), keeping the for/select shape intact.
fn daemon_tick_stmt(spec: &GoToolSpec) -> GoStmt {
    GoStmt::Expr(GoExpr::call(
        GoExpr::sel(GoExpr::ident("log"), "InfoContext"),
        vec![
            GoExpr::ident("ctx"),
            GoExpr::str(format!("{} tick", spec.name)),
        ],
    ))
}

/// `func Run(ctx context.Context, cfg Config) error { … refreshloop / ticker … }`.
fn build_daemon_fn(spec: &GoToolSpec, uses_refresh: bool) -> GoFuncDecl {
    let mut body = GoBlock::new();

    // log, err := logging.FromConfig(cfg.Logging)
    body.push(GoStmt::ShortDecl {
        names: vec!["log".into(), "err".into()],
        values: vec![GoExpr::call(
            GoExpr::path(&["logging", "FromConfig"]),
            vec![GoExpr::path(&["cfg", "Logging"])],
        )],
    });
    body.push(err_check_return(GoExpr::call(
        GoExpr::ident("ErrConfig"),
        vec![GoExpr::ident("err")],
    )));
    body.push(GoStmt::Blank);

    if uses_refresh {
        // loop, err := refreshloop.FromConfig(cfg.Refresh)
        body.push(GoStmt::Comment(
            "refresh-loop-go is the keep-fresh FSM (built ON shigoto): declare WHAT to keep\n\
             fresh + HOW stale is too stale; the loop owns the timer/retry/audit (§2.5)."
                .into(),
        ));
        body.push(GoStmt::ShortDecl {
            names: vec!["loop".into(), "err".into()],
            values: vec![GoExpr::call(
                GoExpr::path(&["refreshloop", "FromConfig"]),
                vec![GoExpr::path(&["cfg", "Refresh"])],
            )],
        });
        body.push(err_check_return(GoExpr::call(
            GoExpr::ident("ErrConfig"),
            vec![GoExpr::ident("err")],
        )));
        body.push(GoStmt::Blank);

        // The keep-fresh item: a generic Spec with a no-op-but-honest Refresher.
        // The Refresher is the seam a real consumer (or a private adapter) fills.
        body.push(GoStmt::Comment(
            "Register one keep-fresh item. The Refresher is the behaviour seam (Law 5): a real\n\
             daemon supplies the side-effecting work (often a PRIVATE vendor adapter); here it\n\
             is a generic idempotent no-op so the loop is provable end-to-end with no vendor."
                .into(),
        ));
        let mut refresher_body = GoBlock::new();
        refresher_body.push(GoStmt::Comment(
            "Idempotent keep-fresh work goes here. Honour ctx; return nil when fresh.".into(),
        ));
        refresher_body.push(GoStmt::Return(vec![GoExpr::call(
            GoExpr::sel(GoExpr::ident("ctx"), "Err"),
            vec![],
        )]));
        let refresher = GoExpr::call(
            GoExpr::path(&["refreshloop", "RefresherFunc"]),
            vec![closure(
                vec![
                    GoParam { name: "ctx".into(), ty: GoType::qualified("context", "Context") },
                    GoParam { name: "_".into(), ty: GoType::named("string") },
                ],
                vec![GoType::named("error")],
                refresher_body,
            )],
        );
        let spec_lit = GoExpr::Composite {
            ty: GoType::qualified("refreshloop", "Spec"),
            fields: vec![
                (Some("Kind".into()), GoExpr::path(&["refreshloop", "KindSecretRotation"])),
                (Some("Subject".into()), GoExpr::str("default")),
                (
                    Some("Interval".into()),
                    GoExpr::binary("*", GoExpr::Lit(go_synthesizer::GoLit::Int(1)), GoExpr::path(&["time", "Hour"])),
                ),
                (Some("Refresher".into()), refresher),
            ],
            addr_of: false,
        };
        // if err := loop.Register(Spec{…}); err != nil { return ErrConfig(err) }
        let mut reg_if = GoBlock::new();
        reg_if.push(GoStmt::Return(vec![GoExpr::call(
            GoExpr::ident("ErrConfig"),
            vec![GoExpr::ident("err")],
        )]));
        body.push(GoStmt::If {
            init: Some(Box::new(GoStmt::ShortDecl {
                names: vec!["err".into()],
                values: vec![GoExpr::call(
                    GoExpr::sel(GoExpr::ident("loop"), "Register"),
                    vec![spec_lit],
                )],
            })),
            cond: GoExpr::binary("!=", GoExpr::ident("err"), GoExpr::nil()),
            body: reg_if,
            else_body: None,
        });
        body.push(GoStmt::Blank);

        body.push(GoStmt::Expr(GoExpr::call(
            GoExpr::sel(GoExpr::ident("log"), "InfoContext"),
            vec![
                GoExpr::ident("ctx"),
                GoExpr::str(format!("{} starting", spec.name)),
            ],
        )));
        body.push(GoStmt::Blank);

        if spec.oneshot {
            // _, err := loop.Tick(ctx) — one keep-fresh round, then exit.
            body.push(GoStmt::Comment(
                "One-shot: a single keep-fresh round, then exit (the BINARY-flavoured daemon).".into(),
            ));
            body.push(GoStmt::ShortDecl {
                names: vec!["_".into(), "tickErr".into()],
                values: vec![GoExpr::call(
                    GoExpr::sel(GoExpr::ident("loop"), "Tick"),
                    vec![GoExpr::ident("ctx")],
                )],
            });
            body.push(GoStmt::Return(vec![GoExpr::ident("tickErr")]));
        } else {
            // return loop.Run(ctx, cfg.Refresh.TickInterval())
            body.push(GoStmt::Comment(
                "Recurring: Run ticks immediately, then every interval until ctx is cancelled\n\
                 (a clean stop returns ctx.Err(), which the daemon treats as nil at exit)."
                    .into(),
            ));
            // err := loop.Run(ctx, cfg.Refresh.TickInterval())
            let mut run_if = GoBlock::new();
            // if errors.Is via ctx: a cancelled daemon is a clean stop. We keep it
            // simple: surface a non-context error, swallow the cancellation.
            let mut inner_if = GoBlock::new();
            inner_if.push(GoStmt::Return(vec![GoExpr::nil()]));
            run_if.push(GoStmt::If {
                init: None,
                cond: GoExpr::binary(
                    "==",
                    GoExpr::ident("err"),
                    GoExpr::call(GoExpr::sel(GoExpr::ident("ctx"), "Err"), vec![]),
                ),
                body: inner_if,
                else_body: None,
            });
            run_if.push(GoStmt::Return(vec![GoExpr::call(
                GoExpr::path(&["errs", "Wrap"]),
                vec![
                    GoExpr::ident("err"),
                    GoExpr::str("daemon run loop"),
                    GoExpr::call(
                        GoExpr::path(&["errs", "WithExitCode"]),
                        vec![GoExpr::path(&["errs", "ExitUnavailable"])],
                    ),
                ],
            )]));
            body.push(GoStmt::If {
                init: Some(Box::new(GoStmt::ShortDecl {
                    names: vec!["err".into()],
                    values: vec![GoExpr::call(
                        GoExpr::sel(GoExpr::ident("loop"), "Run"),
                        vec![
                            GoExpr::ident("ctx"),
                            GoExpr::call(GoExpr::sel(GoExpr::path(&["cfg", "Refresh"]), "TickInterval"), vec![]),
                        ],
                    )],
                })),
                cond: GoExpr::binary("!=", GoExpr::ident("err"), GoExpr::nil()),
                body: run_if,
                else_body: None,
            });
            body.push(GoStmt::Return(vec![GoExpr::nil()]));
        }
    } else {
        // Fallback: a stdlib ticker loop, ctx-aware. one-shot does one tick.
        body.push(GoStmt::Comment(
            "No keep-fresh primitive declared: a ctx-aware stdlib ticker loop (the honest\n\
             fallback). Replace the tick body with the periodic work; ctx cancellation stops it."
                .into(),
        ));
        body.push(GoStmt::Expr(GoExpr::call(
            GoExpr::sel(GoExpr::ident("log"), "InfoContext"),
            vec![
                GoExpr::ident("ctx"),
                GoExpr::str(format!("{} starting", spec.name)),
            ],
        )));
        body.push(GoStmt::Blank);

        if spec.oneshot {
            body.push(GoStmt::Comment("One-shot: do the work once, then exit.".into()));
            body.push(daemon_tick_stmt(spec));
            body.push(GoStmt::Return(vec![GoExpr::call(
                GoExpr::sel(GoExpr::ident("ctx"), "Err"),
                vec![],
            )]));
        } else {
            // ticker := time.NewTicker(time.Second)
            body.push(GoStmt::ShortDecl {
                names: vec!["ticker".into()],
                values: vec![GoExpr::call(
                    GoExpr::path(&["time", "NewTicker"]),
                    vec![GoExpr::path(&["time", "Second"])],
                )],
            });
            body.push(GoStmt::Comment(
                "The real ticker loop (lifecycle / refresh-loop-go shape): a ctx-aware\n\
                 for { select { … } } that ticks on ticker.C and exits cleanly on ctx\n\
                 cancellation (SIGINT/SIGTERM). The Stop runs on the cancellation arm —\n\
                 the only loop exit — releasing the ticker before we return."
                    .into(),
            ));
            // for {
            //     select {
            //     case <-ctx.Done():
            //         ticker.Stop()
            //         return ctx.Err()
            //     case <-ticker.C:
            //         <tick>
            //     }
            // }
            let done_case = GoSelectCase {
                comm: Some(GoStmt::Expr(GoExpr::Receive(Box::new(GoExpr::call(
                    GoExpr::sel(GoExpr::ident("ctx"), "Done"),
                    vec![],
                ))))),
                body: vec![
                    GoStmt::Expr(GoExpr::call(
                        GoExpr::sel(GoExpr::ident("ticker"), "Stop"),
                        vec![],
                    )),
                    GoStmt::Return(vec![GoExpr::call(
                        GoExpr::sel(GoExpr::ident("ctx"), "Err"),
                        vec![],
                    )]),
                ],
            };
            let tick_case = GoSelectCase {
                comm: Some(GoStmt::Expr(GoExpr::Receive(Box::new(GoExpr::sel(
                    GoExpr::ident("ticker"),
                    "C",
                ))))),
                body: vec![daemon_tick_stmt(spec)],
            };
            let mut for_body = GoBlock::new();
            for_body.push(GoStmt::Select { cases: vec![done_case, tick_case] });
            body.push(GoStmt::For {
                init: None,
                cond: None,
                post: None,
                body: for_body,
            });
        }
    }

    GoFuncDecl {
        name: "Run".into(),
        doc: Some(
            "Run is the DAEMON composition root (theory/BOREALIS.md §4). Given an\n\
             already-loaded Config it builds the logger and drives the keep-fresh loop —\n\
             recurring (refreshloop.Run) or one-shot per the spec — returning a typed error or\n\
             nil on a clean stop. No os.Exit here (BOREALIS §3.5)."
                .into(),
        ),
        recv: None,
        params: vec![
            GoParam { name: "ctx".into(), ty: GoType::qualified("context", "Context") },
            GoParam { name: "cfg".into(), ty: GoType::named("Config") },
        ],
        returns: vec![GoType::named("error")],
        body,
    }
}

// ── Action ──────────────────────────────────────────────────────────────────

/// Lower a [`ToolKind::Action`] tool: a GitHub-action entrypoint composing
/// `pleme-actions-shared-go`. The binary has two capabilities: `action` (the
/// runtime entrypoint — `ParseInputs` into config, run the work) and `gen` (emit
/// the typed `action.yml` composite metadata). Inputs map from config_fields.
fn lower_action(spec: &GoToolSpec) -> Vec<(PathBuf, GoFile)> {
    let mut files = vec![(PathBuf::from("main.go"), build_kind_main(spec, "ActionCommand", "action"))];
    files.extend(base_app_files(spec));
    files.push((PathBuf::from("internal/app/action.go"), build_action(spec)));
    files
}

/// Build `internal/app/action.go` — the `ActionCommand()` builder (the run
/// entrypoint that parses inputs into the typed config + a `gen` sub that emits
/// action.yml) and the typed `ActionMeta()` model.
fn build_action(spec: &GoToolSpec) -> GoFile {
    let mut f = GoFile::new("app");
    f.doc = Some(
        "ActionCommand + ActionMeta are the §4 ACTION shape: the action subcommand is the\n\
         GitHub-action runtime entrypoint (ParseInputs hoists INPUT_* env into the typed\n\
         Inputs, run the work), and its gen sub-command renders the typed action.yml composite\n\
         metadata via pleme-actions-shared-go — the metadata is a typed value, never a\n\
         hand-edited action.yml that drifts from the binary it wraps."
            .into(),
    );
    f.imports = vec![
        GoImport::plain("context"),
        GoImport::plain("flag"),
        GoImport::plain("fmt"),
        GoImport::aliased("errs", "github.com/pleme-io/errors-go"),
        GoImport::aliased("cli", "github.com/pleme-io/cli-go"),
        GoImport::aliased("actions", "github.com/pleme-io/pleme-actions-shared-go"),
    ];

    // type Inputs struct { … input:"name" tags … } — one field per config field.
    f.decls.push(GoDecl::Type(build_action_inputs_struct(spec)));
    // func ActionCommand() cli.Command { … }
    f.decls.push(GoDecl::Func(build_action_command(spec)));
    // func ActionMeta() *actions.Action { … }
    f.decls.push(GoDecl::Func(build_action_meta(spec)));
    f
}

/// `type Inputs struct { <Field> string `input:"<name>[,required]"`; … }`.
fn build_action_inputs_struct(spec: &GoToolSpec) -> GoTypeDecl {
    let mut fields: Vec<GoField> = vec![];
    if spec.config_fields.is_empty() {
        // Always give the action at least one input so the struct is non-empty.
        fields.push(GoField {
            name: Some("Message".into()),
            ty: GoType::named("string"),
            doc: Some("Message is the default action input (no config fields declared).".into()),
            markers: vec![],
            tags: vec![GoStructTag::Custom { key: "input".into(), value: "message".into() }],
        });
    }
    for cf in &spec.config_fields {
        let input_name = cf.yaml.clone().unwrap_or_else(|| camel_case(&cf.name));
        let required = cf.validate.as_deref() == Some("required");
        let tag_val = if required {
            format!("{input_name},required")
        } else {
            input_name
        };
        fields.push(GoField {
            name: Some(pascal_case(&cf.name)),
            ty: action_input_type(&cf.ty),
            doc: None,
            markers: vec![],
            tags: vec![GoStructTag::Custom { key: "input".into(), value: tag_val }],
        });
    }
    GoTypeDecl {
        name: "Inputs".into(),
        doc: Some(
            "Inputs is the typed action-input surface. actions.ParseInputs hoists each\n\
             INPUT_<NAME> env var (GitHub Actions' input→env convention) into the matching\n\
             field, coerced to its Go type — the action analog of shikumi config loading."
                .into(),
        ),
        markers: vec![],
        body: GoTypeBody::Struct(fields),
    }
}

/// The Go field type for an action input (string/int/bool; Secret → string).
fn action_input_type(ty: &FieldType) -> GoType {
    match ty {
        FieldType::Int => GoType::named("int"),
        FieldType::Bool => GoType::named("bool"),
        FieldType::Str | FieldType::Secret => GoType::named("string"),
    }
}

/// `func ActionCommand() cli.Command { … action runtime + gen sub … }`.
fn build_action_command(spec: &GoToolSpec) -> GoFuncDecl {
    // The action runtime Run: parse inputs, run the work, emit an output.
    let mut run_body = GoBlock::new();
    run_body.push(GoStmt::Comment(
        "Parse the GitHub-action inputs (INPUT_* env → typed Inputs) — the action's config\n\
         load. A required-but-missing input fails here with a typed usage error."
            .into(),
    ));
    run_body.push(GoStmt::Var {
        name: "in".into(),
        ty: GoType::named("Inputs"),
    });
    let mut parse_if = GoBlock::new();
    parse_if.push(GoStmt::Return(vec![GoExpr::call(
        GoExpr::path(&["errs", "Wrap"]),
        vec![
            GoExpr::ident("err"),
            GoExpr::str("parse action inputs"),
            GoExpr::call(
                GoExpr::path(&["errs", "WithExitCode"]),
                vec![GoExpr::path(&["errs", "ExitUsage"])],
            ),
        ],
    )]));
    run_body.push(GoStmt::If {
        init: Some(Box::new(GoStmt::ShortDecl {
            names: vec!["err".into()],
            values: vec![GoExpr::call(
                GoExpr::Selector {
                    recv: Box::new(GoExpr::ident("actions")),
                    sel: "ParseInputs[Inputs]".into(),
                },
                vec![GoExpr::AddressOf(Box::new(GoExpr::ident("in")))],
            )],
        })),
        cond: GoExpr::binary("!=", GoExpr::ident("err"), GoExpr::nil()),
        body: parse_if,
        else_body: None,
    });
    run_body.push(GoStmt::Blank);
    run_body.push(GoStmt::Comment(
        "Do the action's work here using the typed inputs. This generic action echoes its\n\
         inputs were parsed; a real action runs its logic and writes outputs via\n\
         actions.SetOutput."
            .into(),
    ));
    run_body.push(GoStmt::Expr(GoExpr::call(
        GoExpr::path(&["fmt", "Println"]),
        vec![GoExpr::str(format!("{} action: inputs parsed", spec.name))],
    )));
    run_body.push(GoStmt::Return(vec![GoExpr::nil()]));

    // The gen sub-command: write action.yml to stdout.
    let mut gen_body = GoBlock::new();
    gen_body.push(GoStmt::Comment(
        "Render the typed action.yml composite metadata to stdout (pipe to action.yml). The\n\
         metadata is a pure function of ActionMeta — same input, identical bytes."
            .into(),
    ));
    gen_body.push(GoStmt::Expr(GoExpr::call(
        GoExpr::path(&["fmt", "Print"]),
        vec![GoExpr::call(
            GoExpr::ident("string"),
            vec![GoExpr::call(
                GoExpr::sel(GoExpr::call(GoExpr::ident("ActionMeta"), vec![]), "RenderActionYAML"),
                vec![],
            )],
        )],
    )));
    gen_body.push(GoStmt::Return(vec![GoExpr::nil()]));
    let gen_cmd = GoExpr::Composite {
        ty: GoType::qualified("cli", "Command"),
        fields: vec![
            (Some("Name".into()), GoExpr::str("gen")),
            (Some("Summary".into()), GoExpr::str("render the typed action.yml composite metadata")),
            (Some("Run".into()), run_closure(gen_body)),
        ],
        addr_of: false,
    };

    let mut body = GoBlock::new();
    body.push(GoStmt::Return(vec![GoExpr::Composite {
        ty: GoType::qualified("cli", "Command"),
        fields: vec![
            (Some("Name".into()), GoExpr::str("action")),
            (Some("Summary".into()), GoExpr::str("the GitHub-action runtime entrypoint (parse inputs, run)")),
            (
                Some("Long".into()),
                GoExpr::str(
                    "action is the binary's GitHub-action entrypoint: it parses the INPUT_* env \
                     vars into the typed Inputs and runs the action's work. The gen sub-command \
                     renders the action.yml composite metadata.",
                ),
            ),
            (
                Some("Sub".into()),
                GoExpr::SliceLit {
                    elem_type: GoType::qualified("cli", "Command"),
                    elements: vec![gen_cmd],
                },
            ),
            (Some("Run".into()), run_closure(run_body)),
        ],
        addr_of: false,
    }]));

    GoFuncDecl {
        name: "ActionCommand".into(),
        doc: Some(format!(
            "ActionCommand is {name}'s GitHub-action entrypoint. Its Run parses the action\n\
             inputs into the typed Inputs and runs the work; its gen sub-command renders the\n\
             typed action.yml. borealis.Execute (in main) is the outermost shell.",
            name = spec.name,
        )),
        recv: None,
        params: vec![],
        returns: vec![GoType::qualified("cli", "Command")],
        body,
    }
}

/// `func ActionMeta() *actions.Action { a, _ := actions.NewAction(name, …); return a }`.
fn build_action_meta(spec: &GoToolSpec) -> GoFuncDecl {
    // Build the option args: description, the composite runs, one input per field.
    let mut new_args: Vec<GoExpr> = vec![GoExpr::str(&spec.name)];
    new_args.push(GoExpr::call(
        GoExpr::path(&["actions", "WithActionDescription"]),
        vec![GoExpr::str(&spec.description)],
    ));
    // Runs: a composite action that runs this binary's `action` step.
    let runs = GoExpr::Composite {
        ty: GoType::qualified("actions", "Runs"),
        fields: vec![
            (Some("Using".into()), GoExpr::path(&["actions", "RunComposite"])),
            (
                Some("Steps".into()),
                GoExpr::SliceLit {
                    elem_type: GoType::qualified("actions", "Step"),
                    elements: vec![GoExpr::Composite {
                        ty: GoType::qualified("actions", "Step"),
                        fields: vec![
                            (Some("Name".into()), GoExpr::str(format!("run {}", spec.name))),
                            (Some("Run".into()), GoExpr::str(format!("{} action", spec.name))),
                            (Some("Shell".into()), GoExpr::str("bash")),
                        ],
                        addr_of: false,
                    }],
                },
            ),
        ],
        addr_of: false,
    };
    new_args.push(GoExpr::call(GoExpr::path(&["actions", "WithRuns"]), vec![runs]));
    // One declared input per config field (or the default Message input).
    if spec.config_fields.is_empty() {
        new_args.push(GoExpr::call(
            GoExpr::path(&["actions", "WithActionInput"]),
            vec![GoExpr::Composite {
                ty: GoType::qualified("actions", "ActionInput"),
                fields: vec![
                    (Some("Name".into()), GoExpr::str("message")),
                    (Some("Description".into()), GoExpr::str("the default action input")),
                ],
                addr_of: false,
            }],
        ));
    }
    for cf in &spec.config_fields {
        let input_name = cf.yaml.clone().unwrap_or_else(|| camel_case(&cf.name));
        let required = cf.validate.as_deref() == Some("required");
        new_args.push(GoExpr::call(
            GoExpr::path(&["actions", "WithActionInput"]),
            vec![GoExpr::Composite {
                ty: GoType::qualified("actions", "ActionInput"),
                fields: vec![
                    (Some("Name".into()), GoExpr::str(&input_name)),
                    (Some("Description".into()), GoExpr::str(format!("the {} input", cf.name))),
                    (Some("Required".into()), GoExpr::Lit(go_synthesizer::GoLit::Bool(required))),
                ],
                addr_of: false,
            }],
        ));
    }

    let mut body = GoBlock::new();
    // a, _ := actions.NewAction(name, …)
    body.push(GoStmt::ShortDecl {
        names: vec!["a".into(), "_".into()],
        values: vec![GoExpr::call(GoExpr::path(&["actions", "NewAction"]), new_args)],
    });
    body.push(GoStmt::Return(vec![GoExpr::ident("a")]));

    GoFuncDecl {
        name: "ActionMeta".into(),
        doc: Some(
            "ActionMeta is the typed action.yml model: the action's metadata as a typed value\n\
             (name, description, composite runs, one declared input per config field), rendered\n\
             to byte-stable action.yml by RenderActionYAML. The metadata never drifts from the\n\
             binary because both are generated from the one spec."
                .into(),
        ),
        recv: None,
        params: vec![],
        returns: vec![GoType::pointer(GoType::qualified("actions", "Action"))],
        body,
    }
}

// ── api_op Client seam ────────────────────────────────────────────────────────

/// Build `internal/app/client.go` — the abstract `Client` interface (one
/// method per api_op, ctx-first, typed req/resp placeholders) + the `NewClient`
/// constructor seam returning a not-yet-implemented adapter. The concrete client
/// is supplied later by a PRIVATE adapter (the absorption layer, M3). This is
/// the worlds-separate seam: the public engine NEVER bakes in a vendor SDK.
fn build_client(spec: &GoToolSpec) -> GoFile {
    let ops = declared_api_ops(spec);
    let references_tundra = has_primitive(spec, "tundra-openapi");

    let mut f = GoFile::new("app");
    f.doc = Some(
        "Client is the abstract API seam: one method per declared api_op, ctx-first, over\n\
         typed request/response placeholders. ONLY this interface (and its placeholder types)\n\
         is generated — the DO NOT EDIT header above applies to the INTERFACE in this file.\n\
         \n\
         The CONCRETE adapter that implements Client is NOT generated and must NOT live here:\n\
         author it by hand in a SEPARATE file (e.g. client_adapter.go), free of any generated\n\
         header, where it may import a vendor SDK. The public engine never bakes in a vendor\n\
         SDK (worlds-separate); regenerating this file overwrites the interface but never\n\
         touches your hand-written adapter."
            .into(),
    );
    f.imports = vec![GoImport::plain("context")];

    // One placeholder request/response type per op + the Client interface.
    let mut iface_methods: Vec<go_synthesizer::GoIfaceMethod> = vec![];
    for op in &ops {
        let op_pascal = pascal_case(op);
        let req = format!("{op_pascal}Request");
        let resp = format!("{op_pascal}Response");
        // type <Op>Request struct{} and <Op>Response struct{} placeholders.
        f.decls.push(GoDecl::Type(GoTypeDecl {
            name: req.clone(),
            doc: Some(format!(
                "{req} is the typed request placeholder for the {op} operation. A private\n\
                 adapter maps it onto the concrete SDK request (M3).",
            )),
            markers: vec![],
            body: GoTypeBody::Struct(vec![]),
        }));
        f.decls.push(GoDecl::Type(GoTypeDecl {
            name: resp.clone(),
            doc: Some(format!(
                "{resp} is the typed response placeholder for the {op} operation.",
            )),
            markers: vec![],
            body: GoTypeBody::Struct(vec![]),
        }));
        // The interface method: <Op>(ctx context.Context, req <Op>Request) (<Op>Response, error)
        iface_methods.push(go_synthesizer::GoIfaceMethod {
            name: op_pascal.clone(),
            doc: Some(format!("{op_pascal} invokes the {op} API operation.")),
            params: vec![
                GoParam { name: "ctx".into(), ty: GoType::qualified("context", "Context") },
                GoParam { name: "req".into(), ty: GoType::named(req) },
            ],
            returns: vec![GoType::named(resp), GoType::named("error")],
        });
    }
    f.decls.push(GoDecl::Type(GoTypeDecl {
        name: "Client".into(),
        doc: Some(
            "Client is the abstract API surface the tool's commands call. It is implemented by\n\
             a PRIVATE adapter (the absorption layer) — the public engine only declares the\n\
             shape, so no vendor SDK leaks into the generated public tool."
                .into(),
        ),
        markers: vec![],
        body: GoTypeBody::Interface(iface_methods),
    }));

    // var ErrClientNotImplemented = errs-style sentinel.
    f.decls.push(GoDecl::Var(GoVarDecl {
        name: "errClientNotImplemented".into(),
        ty: None,
        value: Some(GoExpr::call(
            GoExpr::path(&["fmt", "Errorf"]),
            vec![GoExpr::str("app.NewClient: no concrete client supplied (provide a private adapter — M3 absorption)")],
        )),
        doc: Some(
            "errClientNotImplemented is the seam sentinel: NewClient returns it until a private\n\
             adapter is wired in. It keeps the generated tool compiling while making the missing\n\
             concrete client a loud, typed runtime error rather than a silent nil deref."
                .into(),
        ),
        block_id: None,
    }));
    // The fmt import is needed for the sentinel.
    f.imports.push(GoImport::plain("fmt"));

    // var declaredAPIOps = []string{ … } — the op ids a private adapter implements.
    f.decls.push(GoDecl::Var(GoVarDecl {
        name: "declaredAPIOps".into(),
        ty: None,
        value: Some(GoExpr::SliceLit {
            elem_type: GoType::named("string"),
            elements: ops.iter().map(GoExpr::str).collect(),
        }),
        doc: Some(
            "declaredAPIOps lists every api_op the tool's commands dispatch through Client. A\n\
             private adapter implements exactly these methods; the slice documents the seam's\n\
             surface for the M3 absorption layer."
                .into(),
        ),
        block_id: None,
    }));

    // func NewClient(cfg Config) (Client, error) { return nil, errClientNotImplemented }
    let mut nc_body = GoBlock::new();
    if references_tundra && !ops.is_empty() {
        nc_body.push(GoStmt::Comment(
            "tundra-openapi is declared: a private adapter loads the spec once and resolves each\n\
             op's metadata via spec.Operation(id) — kept out of this public seam so no vendor\n\
             SDK leaks in. The op ids the adapter must implement are listed in declaredAPIOps."
                .into(),
        ));
    }
    nc_body.push(GoStmt::Return(vec![GoExpr::nil(), GoExpr::ident("errClientNotImplemented")]));
    f.decls.push(GoDecl::Func(GoFuncDecl {
        name: "NewClient".into(),
        doc: Some(
            "NewClient is the constructor seam: it returns the abstract Client a private adapter\n\
             implements (the M3 absorption layer). Until then it returns errClientNotImplemented\n\
             so the generated tool compiles and dispatches through the interface, with the\n\
             concrete client deferred — the public engine never names or imports a vendor SDK."
                .into(),
        ),
        recv: None,
        params: vec![GoParam { name: "cfg".into(), ty: GoType::named("Config") }],
        returns: vec![GoType::named("Client"), GoType::named("error")],
        body: nc_body,
    }));

    f
}

// ── Closure helpers ─────────────────────────────────────────────────────────

/// A cli.Run closure: `func(ctx context.Context, _ []string, _ *flag.FlagSet) error { <body> }`.
///
/// cli.Command.Run has signature `func(context.Context, []string, *flag.FlagSet) error`
/// (matching the exemplar). We model the function literal as a Named type whose
/// string is the full literal head, then attach the body — but the AST has no
/// function-literal expression node. Instead we represent the closure via the
/// FuncLit support added to the Go AST (see file.rs). If unavailable, this is
/// emitted through the dedicated GoExpr variant.
fn run_closure(body: GoBlock) -> GoExpr {
    closure(
        vec![
            GoParam { name: "ctx".into(), ty: GoType::qualified("context", "Context") },
            GoParam { name: "_".into(), ty: GoType::slice(GoType::named("string")) },
            GoParam {
                name: "_".into(),
                ty: GoType::pointer(GoType::qualified("flag", "FlagSet")),
            },
        ],
        vec![GoType::named("error")],
        body,
    )
}

/// Build a function-literal expression (closure). Delegates to the
/// `GoExpr::FuncLit` AST node.
fn closure(params: Vec<GoParam>, returns: Vec<GoType>, body: GoBlock) -> GoExpr {
    GoExpr::FuncLit { params, returns, body }
}
