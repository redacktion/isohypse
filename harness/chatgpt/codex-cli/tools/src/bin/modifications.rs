//! Write the MODIFICATIONS notice shipped with the built runtime.
//!
//! The template carries @@VERSION@@ wherever the Codex release version belongs.
//! An unsubstituted placeholder is an error: shipping it would misstate what the
//! runtime was built from.
//!
//! Usage: modifications <template> <output> <version>

use std::path::PathBuf;
use std::process::ExitCode;

use codex_tools::{arg_or_exit, fail, read, write};

const PLACEHOLDER: &str = "@@VERSION@@";

fn run() -> Result<(), String> {
    let argv: Vec<String> = std::env::args().collect();
    let usage = "usage: modifications <template> <output> <version>";
    let template = PathBuf::from(arg_or_exit(&argv, 1, usage));
    let output = PathBuf::from(arg_or_exit(&argv, 2, usage));
    let version = arg_or_exit(&argv, 3, usage);
    let text = read(&template)?.replace(PLACEHOLDER, &version);
    if text.contains(PLACEHOLDER) {
        return Err("MODIFICATIONS template placeholder was not substituted".to_string());
    }
    write(&output, &text)
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => fail(&error),
    }
}
