use percent_encoding::{AsciiSet, CONTROLS};
use serde::Serialize;

use crate::util::{Bytes, BytesMut};

/// Serializes `value` as JSON.
///
/// The JSON is written into the buffer that becomes the body, it is not
/// copied from an intermediate `String`.
pub(crate) fn json_body<T: ?Sized + Serialize>(value: &T) -> serde_json::Result<Bytes> {
    let mut buf = BytesMut::new();
    serde_json::to_writer(&mut buf, value)?;
    Ok(buf.freeze())
}

/// `<https://url.spec.whatwg.org/#fragment-percent-encode-set>`
const FRAGMENT: &AsciiSet = &CONTROLS.add(b' ').add(b'"').add(b'<').add(b'>').add(b'`');

/// `<https://url.spec.whatwg.org/#path-percent-encode-set>`
const PATH: &AsciiSet = &FRAGMENT.add(b'#').add(b'?').add(b'{').add(b'}');

#[allow(dead_code)]
/// `<https://url.spec.whatwg.org/#userinfo-percent-encode-set>`
pub(crate) const USERINFO: &AsciiSet = &PATH
    .add(b'/')
    .add(b':')
    .add(b';')
    .add(b'=')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'|');

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_body_matches_to_string() {
        let small = vec![1, 2, 3];
        let large: Vec<String> = (0..1000).map(|i| format!("item-{i}")).collect();
        let map: std::collections::BTreeMap<_, _> = (0..50).map(|i| (i.to_string(), i)).collect();

        assert_eq!(
            json_body(&small).unwrap(),
            serde_json::to_string(&small).unwrap()
        );
        assert_eq!(
            json_body(&large).unwrap(),
            serde_json::to_string(&large).unwrap()
        );
        assert_eq!(
            json_body(&map).unwrap(),
            serde_json::to_string(&map).unwrap()
        );
        assert_eq!(json_body("").unwrap(), "\"\"");

        // a small body is stored inline
        assert!(json_body(&small).unwrap().is_inline());
    }

    #[test]
    fn json_body_error() {
        let mut map = std::collections::HashMap::new();
        map.insert(vec![1u8], 1);
        assert!(json_body(&map).is_err());
    }
}
