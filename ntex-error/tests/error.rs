use std::{error::Error as StdError, fmt, io};

use ntex_bytes::Bytes;
use ntex_error::{
    AsError, Backtrace, BacktraceRaw, Error, ErrorDiagnostic, ErrorMapping, ErrorMessage, Failure,
    IntoFailure, ResultSignature, ResultType, Retryable, Success, fmt_diag_string, fmt_diag_typ,
    fmt_err_string, utils, with_service,
};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
enum MyError {
    #[error("connect: {0}")]
    Connect(&'static str),
    #[error("inner")]
    Inner(#[source] Inner),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("inner-cause")]
struct Inner;

impl ErrorDiagnostic for MyError {
    fn signature(&self) -> &'static str {
        match self {
            MyError::Connect(_) => "my-connect",
            MyError::Inner(_) => "my-inner",
        }
    }
}

impl From<&MyError> for ResultType {
    fn from(_: &MyError) -> Self {
        ResultType::ServiceError
    }
}

impl Retryable for MyError {
    fn is_retryable(&self) -> bool {
        matches!(self, MyError::Connect(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("wrapped: {0}")]
struct Wrapped(MyError);

impl From<MyError> for Wrapped {
    fn from(e: MyError) -> Self {
        Wrapped(e)
    }
}

impl ErrorDiagnostic for Wrapped {
    fn signature(&self) -> &'static str {
        "wrapped"
    }
}

/// Error type that provides its own tag and service.
#[derive(Debug, Clone, thiserror::Error)]
#[error("self-described")]
struct SelfDescribed(Bytes);

impl ErrorDiagnostic for SelfDescribed {
    fn signature(&self) -> &'static str {
        "self"
    }

    fn tag(&self) -> Option<&Bytes> {
        Some(&self.0)
    }

    fn service(&self) -> Option<&'static str> {
        Some("inner-svc")
    }
}

/// Error whose Display writes via `write_char`.
#[derive(Debug, Clone)]
struct CharErr;

impl StdError for CharErr {}

impl fmt::Display for CharErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Write::write_char(f, 'é')
    }
}

impl ErrorDiagnostic for CharErr {
    fn signature(&self) -> &'static str {
        "char"
    }
}

#[test]
fn retryable_and_signature() {
    let ok: Result<(), MyError> = Ok(());
    assert!(!ok.is_retryable());
    let err: Result<(), MyError> = Err(MyError::Connect("x"));
    assert!(err.is_retryable());
    let err2: Result<(), MyError> = Err(MyError::Inner(Inner));
    assert!(!err2.is_retryable());

    assert_eq!(ResultSignature::new("s").signature(), "s");
    assert_eq!(ResultSignature::from(&MyError::Inner(Inner)).0, "my-inner");
    assert_eq!(ResultSignature::from(&ok).signature(), "Success");
    assert_eq!(ResultSignature::from(&err).signature(), "my-connect");

    assert_eq!(Success.signature(), "Success");
    assert_eq!(Success.to_string(), "Success");
    assert_eq!(ResultType::ClientError.to_string(), "ClientError");
    assert_eq!(ResultType::ServiceError.signature(), "ServiceError");
}

#[test]
fn io_error_signatures() {
    use io::ErrorKind::*;

    for (kind, sig) in [
        (InvalidData, "std-io-InvalidData"),
        (InvalidInput, "std-io-InvalidInput"),
        (Unsupported, "std-io-Unsupported"),
        (UnexpectedEof, "std-io-UnexpectedEof"),
        (BrokenPipe, "std-io-BrokenPipe"),
        (ConnectionReset, "std-io-ConnectionReset"),
        (ConnectionAborted, "std-io-ConnectionAborted"),
        (NotConnected, "std-io-NotConnected"),
        (TimedOut, "std-io-TimedOut"),
        (Other, "std-io-Error"),
    ] {
        assert_eq!(io::Error::from(kind).signature(), sig);
    }
}

#[ntex::test]
async fn with_service_sets_missing() {
    let res: Result<(), Error<MyError>> =
        with_service("svc", async { Err(MyError::Connect("x").into()) }).await;
    assert_eq!(res.unwrap_err().service(), Some("svc"));

    let res: Result<(), Error<MyError>> = with_service("svc2", async {
        Err(Error::new(MyError::Connect("x"), "orig"))
    })
    .await;
    assert_eq!(res.unwrap_err().service(), Some("orig"));

    let res: Result<u32, Error<MyError>> = with_service("svc", async { Ok(1) }).await;
    assert_eq!(res.unwrap(), 1);
}

#[test]
fn error_container() {
    let err: Error<Wrapped> = Error::from_err(MyError::Connect("a"));
    assert_eq!(err.signature(), "wrapped");
    assert_eq!(err.service(), None);
    assert_eq!(err.tag(), None);
    assert!(err.backtrace().is_some());
    assert_eq!(format!("{err:?}"), "Wrapped(Connect(\"a\"))");
    assert_eq!(err.to_string(), "wrapped: connect: a");

    let dbg = format!("{:?}", err.debug());
    assert!(
        dbg.starts_with("Error { error: Wrapped(Connect(\"a\")), service: None, tag: None"),
        "{dbg}"
    );

    // StdError::source is transparent, delegates to the inner error
    assert!(StdError::source(&err).is_none());
    assert_eq!(err.as_diag(), &Wrapped(MyError::Connect("a")));

    // map_err preserves metadata
    let err = Error::<MyError>::new(MyError::Connect("b"), "svc")
        .set_tag("t1")
        .insert_item(10u32);
    let shared = err.clone();
    let mapped: Error<Wrapped> = err.map_err();
    assert_eq!(mapped.service(), Some("svc"));
    assert_eq!(mapped.tag(), Some(&Bytes::from_static(b"t1")));
    assert_eq!(mapped.get_item::<u32>(), Some(&10));
    assert_eq!(*mapped, Wrapped(MyError::Connect("b")));

    // into_error on a shared container clones the value
    assert_eq!(shared.clone().into_error(), MyError::Connect("b"));
    assert_eq!(shared.into_error(), MyError::Connect("b"));
}

#[test]
fn shared_container_mutation() {
    let err: Error<MyError> = MyError::Connect("a").into();
    let shared = err.clone();

    // mutating a shared container must not affect the other clone
    let err = err.set_tag("tag").set_service("svc").insert_item("item");
    assert_eq!(err.tag(), Some(&Bytes::from_static(b"tag")));
    assert_eq!(err.service(), Some("svc"));
    assert_eq!(err.get_item::<&str>(), Some(&"item"));
    assert_eq!(shared.tag(), None);
    assert_eq!(shared.service(), None);
    assert_eq!(shared.get_item::<&str>(), None);
}

#[test]
fn error_mapping() {
    let res: Result<(), MyError> = Err(MyError::Connect("a"));
    let res: Result<(), Error<Wrapped>> = res.into_error();
    let err = res.unwrap_err();
    assert_eq!(*err, Wrapped(MyError::Connect("a")));
    assert_eq!(err.service(), None);

    let ok: Result<u8, MyError> = Ok(1);
    let ok: Result<u8, Error<Wrapped>> = ok.into_error();
    assert_eq!(ok.unwrap(), 1);

    let res: Result<(), Error<MyError>> = Err(Error::new(MyError::Connect("b"), "svc"));
    let res: Result<(), Error<Wrapped>> = res.into_error();
    let err = res.unwrap_err();
    assert_eq!(err.service(), Some("svc"));

    let ok: Result<u8, Error<MyError>> = Ok(2);
    let ok: Result<u8, Error<Wrapped>> = ok.into_error();
    assert_eq!(ok.unwrap(), 2);
}

#[test]
fn repr_falls_back_to_inner_error() {
    let err: Error<SelfDescribed> = SelfDescribed(Bytes::from_static(b"inner-tag")).into();
    assert_eq!(err.tag(), Some(&Bytes::from_static(b"inner-tag")));
    assert_eq!(err.service(), Some("inner-svc"));

    let err = err.set_tag("outer").set_service("outer-svc");
    assert_eq!(err.tag(), Some(&Bytes::from_static(b"outer")));
    assert_eq!(err.service(), Some("outer-svc"));

    let f = Failure::from(err);
    assert_eq!(f.tag(), Some(&Bytes::from_static(b"outer")));
    assert_eq!(f.service(), Some("outer-svc"));
}

#[test]
fn failure() {
    let err = Error::<MyError>::new(MyError::Inner(Inner), "svc")
        .set_tag("tag")
        .insert_item(5u8);

    let f = Failure::from(&err);
    assert_eq!(f.signature(), "my-inner");
    assert_eq!(f.service(), Some("svc"));
    assert_eq!(f.tag(), Some(&Bytes::from_static(b"tag")));
    assert!(f.backtrace().is_some());
    assert_eq!(f.get_item::<u8>(), Some(&5));
    assert_eq!(f.get_item::<u16>(), None);
    assert_eq!(f.to_string(), "inner");
    assert_eq!(format!("{f:?}"), "Inner(Inner)");
    assert_eq!(StdError::source(&f).unwrap().to_string(), "inner-cause");

    let f2 = f.clone();
    assert_eq!(f2.signature(), "my-inner");
    let f2 = f2.fail();
    assert_eq!(f2.signature(), "my-inner");

    let diag = f.as_diag();
    assert_eq!(diag.signature(), "my-inner");
    assert_eq!(diag.service(), Some("svc"));
    assert_eq!(diag.tag(), Some(&Bytes::from_static(b"tag")));
    assert!(diag.backtrace().is_some());
    assert_eq!(diag.to_string(), "inner");
    assert_eq!(format!("{diag:?}"), "Inner(Inner)");
    assert_eq!(diag.source().unwrap().to_string(), "inner-cause");

    let f = Failure::from(err);
    assert_eq!(f.signature(), "my-inner");

    // Error<E>::fail() must share the container, not wrap it again
    let err = Error::<MyError>::new(MyError::Inner(Inner), "svc").insert_item(7u16);
    let f = err.clone().fail();
    assert_eq!(f.get_item::<u16>(), Some(&7));
    assert_eq!(f.service(), Some("svc"));
    assert_eq!(format!("{f:?}"), "Inner(Inner)");
    assert!(std::ptr::eq(
        f.backtrace().unwrap(),
        err.backtrace().unwrap()
    ));

    let f = MyError::Connect("c").fail();
    assert_eq!(f.signature(), "my-connect");
    assert!(StdError::source(&f).is_none());
}

#[test]
fn fmt_helpers() {
    let err = Error::<MyError>::new(MyError::Inner(Inner), "svc").set_tag("tag");
    let s = fmt_err_string(&err);
    assert_eq!(s, "inner\ninner-cause\n");

    let s = fmt_diag_string(&err);
    assert!(
        s.starts_with(
            "err: inner\ntype: ServiceError\nsignature: my-inner\ntag: tag\nservice: svc\n\ninner\n  inner-cause\n"
        ),
        "{s}"
    );

    // non-utf8 tag is printed with Debug
    let err = err.set_tag(Bytes::from_static(&[0xff, 0xfe]));
    let mut s = String::new();
    fmt_diag_typ(&mut s, None, &err).unwrap();
    assert!(s.contains("tag: b\"\\xff\\xfe\""), "{s}");
    assert!(!s.contains("type:"));

    // write_char and empty messages
    let err = CharErr;
    assert_eq!(fmt_err_string(&err), "é\n");
    let mut s = String::new();
    fmt_diag_typ(&mut s, None, &err).unwrap();
    assert_eq!(s, "err: é\nsignature: char\n\né\n");

    let msg = ErrorMessage::empty().with_source(io::Error::other("io"));
    assert_eq!(msg.to_string(), "");
    assert_eq!(format!("{msg:?}"), "");
    assert_eq!(fmt_err_string(&msg), "io\n");
    assert_eq!(msg.msg(), "");
}

#[test]
fn backtrace() {
    let bt = Backtrace::with_current();
    assert!(!bt.is_resolved());
    assert_eq!(bt.repr(), None);
    assert_eq!(format!("{bt}"), "");
    assert_eq!(format!("{bt:?}"), "");

    let bt = bt.resolve();
    assert!(bt.is_resolved());
    assert!(bt.repr().is_some());
    assert_eq!(format!("{bt}"), format!("{bt:?}"));

    // already resolved
    let bt2 = bt.clone().resolve();
    assert_eq!(bt2.repr(), bt.repr());

    let bt = Backtrace::with_filename(file!()).resolve();
    assert!(bt.is_resolved());

    let bt: Backtrace = BacktraceRaw::with_current().into();
    assert!(bt.resolve().repr().is_some());

    ntex_error::set_backtrace_start(file!(), 0);
    let bt = Backtrace::with_current().resolve();
    assert!(bt.repr().is_some());
}

#[test]
fn module_path_helpers() {
    assert_eq!(utils::module_path("/p/a-b/src/x.rs"), "a_b::x");
    assert_eq!(
        utils::module_path_prefix("p::", "/p/a-b/src/x.rs"),
        "p::a_b::x"
    );
    assert_eq!(utils::module_path_fs("/p/a-b/src/x.rs"), "a-b/src/x.rs");
}
