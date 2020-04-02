use std::io;

use bytes::BytesMut;
use percent_encoding::{AsciiSet, CONTROLS};

use super::extensions::Extensions;

pub(crate) struct Writer<'a>(pub(crate) &'a mut BytesMut);

impl<'a> io::Write for Writer<'a> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) trait DataFactory {
    fn set(&self, ext: &mut Extensions);
}

pub(crate) struct Data<T>(pub(crate) T);

impl<T: Clone + 'static> DataFactory for Data<T> {
    fn set(&self, ext: &mut Extensions) {
        ext.insert(self.0.clone())
    }
}

/// https://url.spec.whatwg.org/#fragment-percent-encode-set
const FRAGMENT: &AsciiSet = &CONTROLS.add(b' ').add(b'"').add(b'<').add(b'>').add(b'`');

/// https://url.spec.whatwg.org/#path-percent-encode-set
const PATH: &AsciiSet = &FRAGMENT.add(b'#').add(b'?').add(b'{').add(b'}');

#[allow(dead_code)]
/// https://url.spec.whatwg.org/#userinfo-percent-encode-set
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
