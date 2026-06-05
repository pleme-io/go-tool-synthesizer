//! One-shot forge driver: compile a `(defgotool …)` form and write the lowered
//! Go tool to a target directory. Generic — no akeyless, no vendor.
//!
//! Usage: `cargo run --example forge -- <out_dir> <spec.lisp>`

use std::{env, fs, path::PathBuf};

use go_tool_synthesizer::{lower, GoToolSpec};
use tatara_lisp::{domain::TataraDomain, read};

fn main() {
    let mut args = env::args().skip(1);
    let out_dir = PathBuf::from(args.next().expect("out_dir arg required"));
    let spec_path = args.next().expect("spec.lisp arg required");

    GoToolSpec::register();
    let src = fs::read_to_string(&spec_path).expect("read spec");
    let forms = read(&src).expect("parse spec");
    let spec = GoToolSpec::compile_from_sexp(&forms[0]).expect("compile spec");

    for (rel, file) in lower(&spec) {
        let path = out_dir.join(&rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        let go = go_synthesizer::print_file(&file);
        fs::write(&path, go).expect("write file");
        println!("wrote {}", path.display());
    }
}
