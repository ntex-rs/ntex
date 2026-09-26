/// Decodes percent-encoded bytes.
///
/// Returns `None` if there is nothing to decode, or if the decoded bytes are
/// not valid utf-8.
pub(super) fn requote(val: &[u8]) -> Option<String> {
    let mut has_pct = 0;
    let mut pct = [b'%', 0, 0];
    let mut idx = 0;
    let mut cloned: Option<Vec<u8>> = None;

    let len = val.len();
    while idx < len {
        let ch = val[idx];

        if has_pct != 0 {
            pct[has_pct] = val[idx];
            has_pct += 1;
            if has_pct == 3 {
                has_pct = 0;
                let buf = if let Some(ref mut buf) = cloned {
                    buf
                } else {
                    let mut c = Vec::with_capacity(len);
                    c.extend_from_slice(&val[..idx - 2]);
                    cloned = Some(c);
                    cloned.as_mut().unwrap()
                };

                if let Some(ch) = restore_ch(pct[1], pct[2]) {
                    buf.push(ch);
                } else {
                    buf.extend_from_slice(&pct[..]);
                }
            }
        } else if ch == b'%' {
            has_pct = 1;
        } else if let Some(ref mut cloned) = cloned {
            cloned.push(ch);
        }
        idx += 1;
    }

    if let Some(mut data) = cloned {
        if has_pct > 0 {
            data.extend(&pct[..has_pct]);
        }
        // decoded bytes are not necessarily valid utf-8, e.g. `%FF`, such
        // segment is kept percent-encoded
        String::from_utf8(data).ok()
    } else {
        None
    }
}

#[inline]
fn from_hex(v: u8) -> Option<u8> {
    if v.is_ascii_digit() {
        Some(v - 0x30) // ord('0') == 0x30
    } else if (b'A'..=b'F').contains(&v) {
        Some(v - 0x41 + 10) // ord('A') == 0x41
    } else if (b'a'..=b'f').contains(&v) {
        Some(v - 0x61 + 10) // ord('a') == 0x61
    } else {
        None
    }
}

#[inline]
fn restore_ch(d1: u8, d2: u8) -> Option<u8> {
    from_hex(d1).and_then(|d1| from_hex(d2).map(move |d2| (d1 << 4) | d2))
}

#[cfg(test)]
mod tests {
    use super::requote;

    #[test]
    fn requote_rejects_invalid_utf8() {
        assert_eq!(requote(b"test"), None);
        assert_eq!(requote(b"%C3%A9").as_deref(), Some("\u{e9}"));
        assert_eq!(requote(b"%FF"), None);
        assert_eq!(requote(b"%FF%FE"), None);
        assert_eq!(requote(b"a%C3"), None);
        assert_eq!(requote(b"%C3%A9%FF"), None);
    }
}
