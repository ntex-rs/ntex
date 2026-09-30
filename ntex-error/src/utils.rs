use std::{borrow::Cow, cell::RefCell, convert, error::Error as StdError, fmt, io, path};

use ntex_bytes::ByteString;

use crate::{Error, ErrorDiagnostic, ResultType};

/// The retry policy of the error.
pub trait Retryable {
    /// Returns `true` if the failed operation can be retried.
    fn is_retryable(&self) -> bool;
}

impl<T, E> Retryable for Result<T, E>
where
    E: Retryable,
{
    fn is_retryable(&self) -> bool {
        match self {
            Ok(_) => false,
            Err(err) => err.is_retryable(),
        }
    }
}

/// Helper type holding a result classification signature.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ResultSignature(pub &'static str);

impl ResultSignature {
    /// Creates a new `ResultSignature`.
    pub fn new(sig: &'static str) -> Self {
        Self(sig)
    }

    /// Returns a stable identifier for the result classification.
    pub fn signature(self) -> &'static str {
        self.0
    }
}

impl<'a, E: ErrorDiagnostic> From<&'a E> for ResultSignature {
    fn from(err: &'a E) -> Self {
        ResultSignature::new(err.signature())
    }
}

impl<'a, T, E: ErrorDiagnostic> From<&'a Result<T, E>> for ResultSignature {
    fn from(result: &'a Result<T, E>) -> Self {
        match result {
            Ok(_) => ResultSignature(ResultType::Success.as_str()),
            Err(err) => ResultSignature(err.signature()),
        }
    }
}

impl ErrorDiagnostic for convert::Infallible {
    fn signature(&self) -> &'static str {
        unreachable!()
    }
}

impl ErrorDiagnostic for io::Error {
    fn signature(&self) -> &'static str {
        match self.kind() {
            io::ErrorKind::InvalidData => "std-io-InvalidData",
            io::ErrorKind::InvalidInput => "std-io-InvalidInput",
            io::ErrorKind::Unsupported => "std-io-Unsupported",
            io::ErrorKind::UnexpectedEof => "std-io-UnexpectedEof",
            io::ErrorKind::BrokenPipe => "std-io-BrokenPipe",
            io::ErrorKind::ConnectionReset => "std-io-ConnectionReset",
            io::ErrorKind::ConnectionAborted => "std-io-ConnectionAborted",
            io::ErrorKind::NotConnected => "std-io-NotConnected",
            io::ErrorKind::TimedOut => "std-io-TimedOut",
            _ => "std-io-Error",
        }
    }
}

/// Marker diagnostic type representing a successful result.
///
/// Its signature is [`ResultType::Success`].
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct Success;

impl StdError for Success {}

impl ErrorDiagnostic for Success {
    fn signature(&self) -> &'static str {
        ResultType::Success.as_str()
    }
}

impl fmt::Display for Success {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Success")
    }
}

/// Executes a future and ensures an error service is set.
///
/// If the error does not already have a service, the provided service is assigned.
pub async fn with_service<F, T, E>(svc: &'static str, fut: F) -> F::Output
where
    F: Future<Output = Result<T, Error<E>>>,
    E: ErrorDiagnostic + Clone,
{
    fut.await.map_err(|err: Error<E>| {
        if err.service().is_none() {
            err.set_service(svc)
        } else {
            err
        }
    })
}

/// Generates a Rust module path from the given source file path.
///
/// The path is resolved relative to the crate directory (the parent of `src`),
/// e.g. `/p/my-crate/src/net/io.rs` becomes `my_crate::net::io`.
pub fn module_path(file_path: &str) -> ByteString {
    module_path_ext("", "", "::", "", file_path)
}

/// Generates a Rust module path from the given source file path,
/// prepending `prefix`.
pub fn module_path_prefix(prefix: &'static str, file_path: &str) -> ByteString {
    module_path_ext(prefix, "", "::", "", file_path)
}

/// Generates a `/`-separated file path relative to the crate's parent directory.
///
/// e.g. `/p/my-crate/src/net/io.rs` becomes `my-crate/src/net/io.rs`.
pub fn module_path_fs(file_path: &str) -> ByteString {
    module_path_ext("", "/src", "/", ".rs", file_path)
}

fn module_path_ext(
    prefix: &'static str,
    mod_sep: &'static str,
    sep: &'static str,
    suffix: &'static str,
    file_path: &str,
) -> ByteString {
    type HashMap<K, V> = std::collections::HashMap<K, V, foldhash::fast::RandomState>;
    type Key = (&'static str, &'static str, &'static str, &'static str);
    thread_local! {
        static CACHE: RefCell<HashMap<Key, HashMap<String, ByteString>>> = RefCell::new(HashMap::default());
    }

    let key = (prefix, mod_sep, sep, suffix);
    let cached = CACHE.with(|cache| {
        if let Some(c) = cache.borrow().get(&key) {
            c.get(file_path).cloned()
        } else {
            None
        }
    });

    if let Some(cached) = cached {
        cached
    } else {
        let normalized_file_path = normalize_file_path(file_path);
        let (module_name, module_root) = module_root_from_file(mod_sep, &normalized_file_path);
        let module = module_path_from_file_with_root(
            prefix,
            sep,
            &normalized_file_path,
            &module_name,
            &module_root,
            suffix,
        );

        let _ = CACHE.with(|cache| {
            cache
                .borrow_mut()
                .entry(key)
                .or_default()
                .insert(file_path.to_string(), module.clone())
        });
        module
    }
}

fn normalize_file_path(file_path: &str) -> String {
    let path = path::Path::new(file_path);
    if path.is_absolute() {
        return path.to_string_lossy().into_owned();
    }

    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path).to_string_lossy().into_owned(),
        Err(_) => file_path.to_string(),
    }
}

fn module_root_from_file(mod_sep: &str, file_path: &str) -> (String, path::PathBuf) {
    let normalized = file_path.replace('\\', "/");
    if let Some((root, _)) = normalized.rsplit_once("/src/") {
        let mut root = path::PathBuf::from(root);
        let mod_name = root
            .file_name()
            .map_or(Cow::Borrowed("crate"), |s| s.to_string_lossy());
        let mod_name = if mod_sep.is_empty() {
            mod_name.replace('-', "_")
        } else {
            mod_name.to_string()
        };
        root.push("src");
        return (format!("{mod_name}{mod_sep}"), root);
    }

    let path = path::Path::new(file_path)
        .parent()
        .map_or_else(|| path::PathBuf::from("."), path::Path::to_path_buf);

    let m = path
        .parent()
        .and_then(|p| p.file_name())
        .map_or_else(|| Cow::Borrowed("crate"), |p| p.to_string_lossy());

    (format!("{m}{mod_sep}"), path)
}

fn module_path_from_file(sep: &str, file_path: &str) -> String {
    let normalized = file_path.replace('\\', "/");
    let relative = normalized
        .split_once("/src/")
        .map_or(normalized.as_str(), |(_, tail)| tail);

    if relative == "lib.rs" || relative == "main.rs" {
        return relative.to_string();
    }

    let without_ext = relative.strip_suffix(".rs").unwrap_or(relative);
    if without_ext.ends_with("/mod") {
        let parent = without_ext.strip_suffix("/mod").unwrap_or(without_ext);
        let parent = parent.trim_matches('/');
        return parent.replace('/', sep);
    }

    let module = without_ext.trim_matches('/').replace('/', sep);
    if module.is_empty() {
        "crate".to_string()
    } else {
        module
    }
}

fn module_path_from_file_with_root(
    prefix: &str,
    sep: &str,
    file_path: &str,
    module_name: &str,
    module_root: &path::Path,
    suffix: &str,
) -> ByteString {
    let normalized = file_path.replace('\\', "/");
    let module_root_norm = module_root.to_string_lossy().replace('\\', "/");
    // filesystem roots (`/`, `C:/`) already end with a separator
    let module_root_norm = module_root_norm.trim_end_matches('/');

    let Some(relative) = normalized.strip_prefix(&(module_root_norm.to_string() + "/")) else {
        return format!(
            "{prefix}{module_name}{sep}{}{suffix}",
            module_path_from_file(sep, file_path)
        )
        .into();
    };
    if relative == "lib.rs" || relative == "main.rs" {
        return ByteString::from(format!("{prefix}{module_name}{sep}{relative}"));
    }

    let without_ext = relative.strip_suffix(".rs").unwrap_or(relative);
    if without_ext.ends_with("/mod") {
        let parent = without_ext.strip_suffix("/mod").unwrap_or(without_ext);
        let parent = parent.trim_matches('/');
        return format!(
            "{prefix}{module_name}{sep}{}{sep}mod{suffix}",
            parent.replace('/', sep)
        )
        .into();
    }

    let module = without_ext.trim_matches('/').replace('/', sep);
    if module.is_empty() {
        ByteString::from(format!("{prefix}{module_name}{suffix}"))
    } else {
        format!("{prefix}{module_name}{sep}{module}{suffix}").into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_paths() {
        assert_eq!(module_path("/p/my-crate/src/lib.rs"), "my_crate::lib.rs");
        assert_eq!(module_path("/p/my-crate/src/main.rs"), "my_crate::main.rs");
        assert_eq!(
            module_path("/p/my-crate/src/net/mod.rs"),
            "my_crate::net::mod"
        );
        assert_eq!(
            module_path("/p/my-crate/src/net/io.rs"),
            "my_crate::net::io"
        );
        assert_eq!(module_path("/p/my-crate/src/.rs"), "my_crate");
        assert_eq!(module_path("/p/my-crate/src/a/src/b.rs"), "a::b");
        assert_eq!(module_path("/a/b/c.rs"), "a::c");
        assert_eq!(module_path("/c.rs"), "crate::c");
        assert_eq!(module_path("src/lib.rs"), "ntex_error::lib.rs");
        assert_eq!(
            module_path("C:\\p\\my-crate\\src\\net\\io.rs"),
            "my_crate::net::io"
        );
        // cached
        assert_eq!(
            module_path("/p/my-crate/src/net/io.rs"),
            "my_crate::net::io"
        );

        assert_eq!(
            module_path_prefix("pfx::", "/p/my-crate/src/net/io.rs"),
            "pfx::my_crate::net::io"
        );
        assert_eq!(
            module_path_fs("/p/my-crate/src/lib.rs"),
            "my-crate/src/lib.rs"
        );
        assert_eq!(
            module_path_fs("/p/my-crate/src/net/mod.rs"),
            "my-crate/src/net/mod.rs"
        );
        assert_eq!(
            module_path_fs("/p/my-crate/src/net/io.rs"),
            "my-crate/src/net/io.rs"
        );
        assert_eq!(module_path_fs("/p/my-crate/src/.rs"), "my-crate/src.rs");
    }

    #[test]
    fn module_path_cache_per_format() {
        // the same file path must not share cache entries between formats
        let p = "/p/other-crate/src/x/y.rs";
        assert_eq!(module_path(p), "other_crate::x::y");
        assert_eq!(module_path_fs(p), "other-crate/src/x/y.rs");
        assert_eq!(module_path(p), "other_crate::x::y");
    }

    #[test]
    fn module_path_from_file_variants() {
        assert_eq!(module_path_from_file("::", "/x/src/lib.rs"), "lib.rs");
        assert_eq!(module_path_from_file("::", "/x/src/main.rs"), "main.rs");
        assert_eq!(module_path_from_file("::", "/x/src/net/mod.rs"), "net");
        assert_eq!(module_path_from_file("::", "/x/src/net/io.rs"), "net::io");
        assert_eq!(module_path_from_file("::", "/x/src/.rs"), "crate");
        assert_eq!(module_path_from_file("/", "a/b.rs"), "a/b");
    }
}
