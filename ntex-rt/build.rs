use std::{collections::HashSet, env};

fn main() {
    let mut features = HashSet::<&'static str>::default();

    for (key, _val) in env::vars() {
        let _ = match key.as_ref() {
            "CARGO_FEATURE_COMPIO" => features.insert("compio"),
            "CARGO_FEATURE_TOKIO" => features.insert("tokio"),
            "CARGO_FEATURE_GLOMMIO" => features.insert("glommio"),
            "CARGO_FEATURE_ASYNC_STD" => features.insert("async-std"),
            _ => false,
        };
    }

    if features.is_empty() {
        panic!("Runtime must be selected '--feature=ntex\\$runtime', available options are \"compio\", \"tokio\", \"async-std\", \"glommio\"");
    } else if features.len() > 1 {
        panic!(
            "Only one runtime feature could be selected, current selection {:?}",
            features
        );
    }
}
