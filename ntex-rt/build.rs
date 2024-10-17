use std::{collections::HashSet, env};

fn main() {
    let mut clippy = false;
    let mut features = HashSet::<&'static str>::default();

    for (key, val) in env::vars() {
        let _ = match key.as_ref() {
            "CARGO_FEATURE_COMPIO" => features.insert("compio"),
            "CARGO_FEATURE_TOKIO" => features.insert("tokio"),
            "CARGO_FEATURE_GLOMMIO" => features.insert("glommio"),
            "CARGO_FEATURE_ASYNC_STD" => features.insert("async-std"),
            "CARGO_CFG_FEATURE" => {
                if val.contains("cargo-clippy") {
                    clippy = true;
                }
                false
            }
            _ => false,
        };
    }

    if !clippy {
        if features.is_empty() {
            panic!("Runtime must be selected '--feature=ntex/$runtime', available options are \"compio\", \"tokio\", \"async-std\", \"glommio\"");
        } else if features.len() > 1 {
            panic!(
                "Only one runtime feature could be selected, current selection {:?}",
                features
            );
        }
    }
}
