#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum HttpProtocol {
    Http1,
    Http2,
    Unknown,
}
