//! The typed tool-spec — the platform engine's authoring surface.
//!
//! A [`GoToolSpec`] is the single typed input that the [`crate::lower`] engine
//! turns into a complete, GSDS-conformant, borealis-profiled Go tool composing
//! the pleme-io fleet primitives (cli-go / shikumi-go / borealis / errors-go /
//! logging-go). It is authored declaratively as a `(defgotool …)` Lisp form
//! via `#[derive(DeriveTataraDomain)]` — exactly the MonitorSpec pattern from
//! `tatara-domains`.
//!
//! ```lisp
//! (defgotool
//!   :name "borealis-greet"
//!   :kind Cli
//!   :description "A generic borealis-profiled greeter."
//!   :profile "nord"
//!   :primitives ("cli-go" "shikumi-go" "borealis" "errors-go" "logging-go")
//!   :config-fields ((:name "greeting" :ty Str :yaml "greeting" :validate "required"))
//!   :commands ((:name "greet" :summary "print a themed greeting"
//!               :flags ((:name "name" :ty Str :default "world")))))
//! ```
//!
//! Non-basic field kinds (the `ToolKind` enum, the nested `ConfigField` /
//! `CommandSpec` / `FlagSpec` structs, and the `Vec<Nested>` lists) lower
//! through the derive's serde fall-through (`sexp_to_json` + serde_json), so
//! the whole tree — including the recursive `CommandSpec.sub` — compiles from
//! one Lisp form with zero hand-written parsing.

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

/// The kind of tool to synthesize. Each lowers to its BOREALIS §4 shape (see
/// [`crate::lower`]).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum ToolKind {
    /// A leaf command-line tool: the `run(ctx)` body runs the cli-go grammar
    /// through `exit.Map(borealis.Execute(...))`; commands do the work directly.
    Cli,
    /// A long-running service: a CLI whose `serve` subcommand's Run nests
    /// `lifecycle.New(cfg.Lifecycle, …).Go("work", …).Run(ctx)`. When `server-go`
    /// is declared the HTTP leaf is registered as a lifecycle actor; when
    /// `controller-go` is declared the reconcile chassis runs as an App.Go unit.
    Service,
    /// A long-running daemon: a `run` subcommand driving a `refresh-loop-go`
    /// keep-fresh loop (recurring `loop.Run`, or a single `loop.Tick` when
    /// [`GoToolSpec::oneshot`] is set). Falls back to a ctx-aware ticker when no
    /// keep-fresh primitive is declared.
    Daemon,
    /// A GitHub Action entrypoint: an `action` subcommand that `ParseInputs` the
    /// `INPUT_*` env into the typed `Inputs`, plus a `gen` sub that renders the
    /// typed `action.yml` composite metadata via `pleme-actions-shared-go`.
    Action,
    /// A bare binary — the proven CLI shape (`Cli` with no `serve`/`run`).
    Binary,
    /// A library (no `main`): [`crate::lower`] emits no Go source and defers to
    /// pleme-doc-gen's existing library scaffold.
    Library,
}

impl Default for ToolKind {
    fn default() -> Self {
        Self::Cli
    }
}

/// The scalar type of a config field. Drives the Go struct field type, the
/// yaml/json/validate tags, and (for `Secret`) the redacting `shikumi.Secret`
/// wrapper.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum FieldType {
    /// Go `string`.
    Str,
    /// Go `int`.
    Int,
    /// Go `bool`.
    Bool,
    /// `shikumi.Secret[string]` — redacts under every print/marshal path.
    Secret,
}

impl Default for FieldType {
    fn default() -> Self {
        Self::Str
    }
}

/// One typed config field — becomes a struct field on the tool's `Config`
/// (with yaml/json/validate tags) and a row in the `config show` dump.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[tatara(keyword = "defconfigfield")]
pub struct ConfigField {
    /// Field name in snake_case authoring; lowered to a Go PascalCase field
    /// and a camelCase yaml/json tag.
    pub name: String,
    /// The scalar type. Defaults to `Str`.
    #[serde(default)]
    pub ty: FieldType,
    /// The yaml/json tag name. Defaults to the camelCase of `name`.
    pub yaml: Option<String>,
    /// An optional go-playground/validator tag body (e.g. "required").
    pub validate: Option<String>,
    /// An optional declared default value (as a string literal; coerced to the
    /// Go type at lower). Seeds `DefaultConfig()` — the lowest-precedence
    /// shikumi baseline. When absent the field gets the zero value of its type
    /// (`""` for Str/Secret, `0` for Int, `false` for Bool). NEVER the field
    /// name (the historical placeholder bug).
    pub default: Option<String>,
    /// An optional env-var suffix appended to the tool's EnvPrefix. Currently
    /// informational (shikumi binds env from the field name); reserved.
    pub env_suffix: Option<String>,
}

/// One typed CLI flag on a command — becomes a `cli.NewFlag[T](…)` declaration
/// with an optional `.Env(…)` and `.Validate(…)`.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[tatara(keyword = "defflag")]
pub struct FlagSpec {
    /// Flag name (the `--name` long form).
    pub name: String,
    /// Scalar type. Defaults to `Str`.
    #[serde(default)]
    pub ty: FieldType,
    /// Default value literal (as a string; coerced to the Go type at lower).
    pub default: Option<String>,
    /// Usage string shown in help.
    pub usage: Option<String>,
    /// When true, a non-empty validator is attached to the flag.
    #[serde(default)]
    pub require_non_empty: bool,
}

/// One command in the tool's grammar. Recursive: `sub` carries nested
/// subcommands (lowered through the same serde path).
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[tatara(keyword = "defcommand")]
pub struct CommandSpec {
    /// Command name.
    pub name: String,
    /// One-line summary shown in the command list.
    pub summary: String,
    /// Optional long description.
    pub long: Option<String>,
    /// Typed flags on this command.
    #[serde(default)]
    pub flags: Vec<FlagSpec>,
    /// Optional tundra-openapi operationId this command references at runtime.
    pub api_op: Option<String>,
    /// Nested subcommands.
    #[serde(default)]
    pub sub: Vec<CommandSpec>,
}

/// The complete typed tool specification — the platform engine's input.
///
/// Authored as `(defgotool …)`. `kind`/`profile`/`go_version` carry sane
/// defaults so a minimal spec is one line of `:name` + a `:commands` list.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[tatara(keyword = "defgotool")]
pub struct GoToolSpec {
    /// Tool name — the binary name, the Go module leaf, the shikumi loader
    /// name, and the cli-go App name. One token, no drift.
    pub name: String,
    /// What to synthesize. Defaults to `Cli`.
    #[serde(default)]
    pub kind: ToolKind,
    /// One-line tool description (the App description).
    pub description: String,
    /// The borealis profile: "nord" (public/generic world) or "tundra" (the
    /// private arctic-palette world). Drives the theme verb + env prefix
    /// shape. Defaults to "tundra" per the fleet convention; the public
    /// generic proof tool overrides to "nord".
    #[serde(default = "default_profile")]
    pub profile: String,
    /// Explicit Go module path. Defaults to `github.com/pleme-io/<name>`.
    pub module_path: Option<String>,
    /// Go toolchain version directive. Defaults to the fleet/host standard
    /// [`DEFAULT_GO_VERSION`] ("1.25.9"). pleme-doc-gen pins both the `go` and
    /// `toolchain` go.mod directives to this single value so `go mod tidy` can
    /// never silently bump the tool to a dependency's higher directive.
    #[serde(default = "default_go_version")]
    pub go_version: String,
    /// The fleet primitives this tool composes (e.g. "cli-go", "shikumi-go",
    /// "borealis", "errors-go", "logging-go"). Drives the go.mod require +
    /// pre-publish replace directives.
    #[serde(default)]
    pub primitives: Vec<String>,
    /// Typed config fields → the `Config` struct + `LoadConfig`.
    #[serde(default)]
    pub config_fields: Vec<ConfigField>,
    /// The command grammar → the cli-go App tree.
    #[serde(default)]
    pub commands: Vec<CommandSpec>,
    /// tundra-openapi operationIds the tool references (informational at the
    /// engine layer; the generated tool imports tundra-openapi at runtime).
    #[serde(default)]
    pub api_ops: Vec<String>,
    /// For [`ToolKind::Daemon`]: when true the daemon runs its loop ONCE and
    /// exits (the one-shot reconcile), instead of the recurring keep-fresh loop.
    /// Ignored for every other kind. Defaults to false (recurring).
    #[serde(default)]
    pub oneshot: bool,
    /// Optional released-binary name override. When set, the caixa/flake
    /// emission names the built binary this (e.g. "akl-auth") instead of the
    /// module leaf (`name`). Lets the published binary differ from the Go
    /// module leaf. Resolved via [`GoToolSpec::resolved_binary_name`].
    pub binary_name: Option<String>,
    /// Override for the Nix flake builder function name. Reserved — the
    /// scaffolder (pleme-doc-gen) owns flake emission via flake_builder_for.
    pub flake_builder: Option<String>,
}

/// The fleet/host-standard Go toolchain version. pleme-doc-gen pins both the
/// go.mod / go.work `go` directive AND a `toolchain go<VER>` line to this single
/// constant, so `go mod tidy` / `go work sync` cannot silently bump the tool to
/// a dependency's higher directive (e.g. akeyless-go declares `go 1.26`, which
/// bumped a tool to 1.26.0 and then `GOTOOLCHAIN=local` refused to build). One
/// place to change the whole fleet's pin.
pub const DEFAULT_GO_VERSION: &str = "1.25.9";

fn default_profile() -> String {
    "tundra".to_string()
}

fn default_go_version() -> String {
    DEFAULT_GO_VERSION.to_string()
}

impl GoToolSpec {
    /// The resolved profile, applying the "tundra" default when empty.
    ///
    /// NOTE: the `#[derive(TataraDomain)]` derive honors the *presence* of a
    /// `#[serde(default = "…")]` attribute but always falls back to
    /// `String::default()` (empty), not the named default function — so a
    /// missing `:profile` compiles to `""`. We normalize at the accessor layer
    /// rather than relying on the derive's default path. (See crate GAPS.)
    #[must_use]
    pub fn resolved_profile(&self) -> String {
        if self.profile.is_empty() {
            default_profile()
        } else {
            self.profile.clone()
        }
    }

    /// The resolved Go toolchain version, applying the [`DEFAULT_GO_VERSION`]
    /// default when empty (same derive-default caveat as `resolved_profile`).
    #[must_use]
    pub fn resolved_go_version(&self) -> String {
        if self.go_version.is_empty() {
            default_go_version()
        } else {
            self.go_version.clone()
        }
    }

    /// The resolved released-binary name: the explicit `binary_name` slot, else
    /// the module/tool leaf `name`. Lets a tool's published binary differ from
    /// its Go module leaf (e.g. module `tundra-auth` → binary `akl-auth`).
    #[must_use]
    pub fn resolved_binary_name(&self) -> String {
        self.binary_name
            .clone()
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| self.name.clone())
    }

    /// The resolved Go module path: the explicit `module_path` or the fleet
    /// default `github.com/pleme-io/<name>`.
    #[must_use]
    pub fn resolved_module_path(&self) -> String {
        self.module_path
            .clone()
            .unwrap_or_else(|| format!("github.com/pleme-io/{}", self.name))
    }

    /// The UPPER_SNAKE env prefix for the tool, derived from `name`. e.g.
    /// `borealis-greet` → `BOREALIS_GREET_`.
    #[must_use]
    pub fn env_prefix(&self) -> String {
        let mut s: String = self
            .name
            .chars()
            .map(|c| {
                if c == '-' || c == '.' {
                    '_'
                } else {
                    c.to_ascii_uppercase()
                }
            })
            .collect();
        s.push('_');
        s
    }

    /// True when the tundra (private/arctic) profile is selected.
    #[must_use]
    pub fn is_tundra(&self) -> bool {
        self.resolved_profile().eq_ignore_ascii_case("tundra")
    }

    /// The borealis theme constructor for this profile: `Tundra` or `Nord`.
    #[must_use]
    pub fn theme_constructor(&self) -> &'static str {
        if self.is_tundra() {
            "Tundra"
        } else {
            "Nord"
        }
    }

    /// Register the (defgotool …) family with the global tatara-lisp
    /// dispatcher. Call once per binary that wants to compile the form via
    /// `tatara_lisp::domain::lookup`.
    pub fn register() {
        tatara_lisp::domain::register::<GoToolSpec>();
        tatara_lisp::domain::register::<ConfigField>();
        tatara_lisp::domain::register::<FlagSpec>();
        tatara_lisp::domain::register::<CommandSpec>();
    }
}
