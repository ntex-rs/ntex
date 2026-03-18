use percent_encoding::{AsciiSet, CONTROLS};

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
