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
    GoBlock, GoDecl, GoExpr, GoField, GoFile, GoFuncDecl, GoImport, GoParam, GoStmt,
    GoStructTag, GoType, GoTypeBody, GoTypeDecl, GoVarDecl, JsonTag, YamlTag,
};

use crate::spec::{CommandSpec, ConfigField, FieldType, FlagSpec, GoToolSpec};

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
/// Only [`ToolKind::Cli`] is fully implemented (the proven vertical). Other
/// kinds currently lower to the same Cli shape with their commands; richer
/// Service/Daemon/Action wiring is the milestone-2 gap.
#[must_use]
pub fn lower(spec: &GoToolSpec) -> Vec<(PathBuf, GoFile)> {
    vec![
        (PathBuf::from("main.go"), build_main(spec)),
        (PathBuf::from("internal/app/config.go"), build_config(spec)),
        (PathBuf::from("internal/app/errors.go"), build_errors(spec)),
        (PathBuf::from("internal/app/app.go"), build_app(spec)),
        (PathBuf::from("internal/app/app_test.go"), build_app_test(spec)),
    ]
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
