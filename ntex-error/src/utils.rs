use std::{
    borrow::Cow, cell::RefCell, convert::Infallible, error::Error as StdError, fmt, io,
    path::Path, path::PathBuf,
};

use ntex_bytes::ByteString;

use crate::{Error, ErrorDiagnostic, ResultType};

/// Helper type holding a result type and classification signature.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ResultInfo(ResultType, &'static str);

impl ResultInfo {
    /// Construct new `ResultInfo`
    pub fn new(typ: ResultType, sig: &'static str) -> Self {
        Self(typ, sig)
    }

    /// Returns the classification of the result (e.g. success, client error, service error).
    pub fn typ(&self) -> ResultType {
        self.0
    }

    /// Returns a stable identifier for the result classification.
    pub fn signature(&self) -> &'static str {
        self.1
    }
}

impl<'a, E: ErrorDiagnostic> From<&'a E> for ResultInfo {
    fn from(err: &'a E) -> Self {
        ResultInfo::new(err.typ(), err.signature())
    }
}

impl<'a, T, E: ErrorDiagnostic> From<&'a Result<T, E>> for ResultInfo {
    fn from(result: &'a Result<T, E>) -> Self {
        match result {
            Ok(_) => ResultInfo(ResultType::Success, ResultType::Success.as_str()),
            Err(err) => ResultInfo(err.typ(), err.signature()),
        }
    }
}

impl ErrorDiagnostic for Infallible {
    fn signature(&self) -> &'static str {
        unreachable!()
    }
}

impl ErrorDiagnostic for io::Error {
    fn typ(&self) -> ResultType {
        match self.kind() {
            io::ErrorKind::InvalidData
            | io::ErrorKind::Unsupported
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::NotConnected
            | io::ErrorKind::TimedOut => ResultType::ClientError,
            _ => ResultType::ServiceError,
        }
    }

    fn signature(&self) -> &'static str {
        match self.kind() {
            io::ErrorKind::InvalidData => "io-InvalidData",
            io::ErrorKind::Unsupported => "io-Unsupported",
            io::ErrorKind::UnexpectedEof => "io-UnexpectedEof",
            io::ErrorKind::BrokenPipe => "io-BrokenPipe",
            io::ErrorKind::ConnectionReset => "io-ConnectionReset",
            io::ErrorKind::ConnectionAborted => "io-ConnectionAborted",
            io::ErrorKind::NotConnected => "io-NotConnected",
            io::ErrorKind::TimedOut => "io-TimedOut",
            _ => "io-Error",
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct Success;

impl StdError for Success {}

impl ErrorDiagnostic for Success {
    fn typ(&self) -> ResultType {
        ResultType::Success
    }

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

/// Generates a module path from the given file path.
pub fn module_path(file_path: &str) -> ByteString {
    module_path_ext("", "", "::", "", file_path)
}

/// Generates a module path with a prefix from the given file path.
pub fn module_path_prefix(prefix: &'static str, file_path: &str) -> ByteString {
    module_path_ext(prefix, "", "::", "", file_path)
}

/// Generates a module path from a file path.
pub fn module_path_fs(file_path: &str) -> ByteString {
    module_path_ext("", "/src", "/", ".rs", file_path)
}

fn module_path_ext(
    prefix: &'static str,
    mod_sep: &str,
    sep: &str,
    suffix: &str,
    file_path: &str,
) -> ByteString {
    type HashMap<K, V> = std::collections::HashMap<K, V, foldhash::fast::RandomState>;
    thread_local! {
        static CACHE: RefCell<HashMap<&'static str, HashMap<String, ByteString>>> = RefCell::new(HashMap::default());
    }

    let cached = CACHE.with(|cache| {
        if let Some(c) = cache.borrow().get(prefix) {
            c.get(file_path).cloned()
        } else {
            None
        }
    });

    if let Some(cached) = cached {
        cached
    } else {
        let normalized_file_path = normalize_file_path(file_path);
        let (module_name, module_root) =
            module_root_from_file(mod_sep, &normalized_file_path);
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
                .entry(prefix)
                .or_default()
                .insert(file_path.to_string(), module.clone())
        });
        module
    }
}

fn normalize_file_path(file_path: &str) -> String {
    let path = Path::new(file_path);
    if path.is_absolute() {
        return path.to_string_lossy().into_owned();
    }

    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path).to_string_lossy().into_owned(),
        Err(_) => file_path.to_string(),
    }
}

fn module_root_from_file(mod_sep: &str, file_path: &str) -> (String, PathBuf) {
    let normalized = file_path.replace('\\', "/");
    if let Some((root, _)) = normalized.rsplit_once("/src/") {
        let mut root = PathBuf::from(root);
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

    let path = Path::new(file_path)
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);

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
    module_root: &Path,
    suffix: &str,
) -> ByteString {
    let normalized = file_path.replace('\\', "/");
    let module_root_norm = module_root.to_string_lossy().replace('\\', "/");

    let Some(relative) = normalized.strip_prefix(&(module_root_norm.clone() + "/")) else {
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
