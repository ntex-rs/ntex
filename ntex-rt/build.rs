use std::{collections::HashSet, env};

fn main() {
    let mut features = HashSet::<&'static str>::default();

    for (key, _) in env::vars() {
        let _ = match key.as_ref() {
            "CARGO_FEATURE_COMPIO" => features.insert("compio"),
            "CARGO_FEATURE_TOKIO" => features.insert("tokio"),
            "CARGO_FEATURE_NEON" => features.insert("neon"),
            _ => false,
        };
    }

    if features.len() > 1 {
        panic!(
            "Only one runtime feature could be selected, current selection {features:?}"
        );
    }
}
