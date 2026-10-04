/// Validates a `Host` header value without allocating.
///
/// `Host = uri-host [ ":" port ]`, an empty value is allowed, see
/// [RFC 9112 section 3.2](https://www.rfc-editor.org/rfc/rfc9112#section-3.2).
/// Accepts the same values as `http::uri::Authority` without userinfo, with
/// a numeric port.
pub(super) fn is_valid_host(val: &[u8]) -> bool {
    // `Authority` allows at most 8 colons inside an IPv6 literal
    const MAX_COLONS: u32 = 8;

    let mut colons = 0;
    let mut open = false;
    let mut closed = false;
    let mut percent = false;
    let mut port = false;
    let mut bad_port = false;

    for &b in val {
        match b {
            b'a'..=b'z'
            | b'A'..=b'Z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b'!'
            | b'$'
            | b'&'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b'+'
            | b','
            | b';'
            | b'=' => bad_port |= port && !b.is_ascii_digit(),
            b':' => {
                if colons == MAX_COLONS {
                    return false;
                }
                colons += 1;
                port = true;
            }
            b'[' => {
                if percent || open {
                    return false;
                }
                open = true;
            }
            b']' => {
                if !open || closed {
                    return false;
                }
                // colons and zone id `%` belong to the IPv6 literal
                closed = true;
                colons = 0;
                percent = false;
                port = false;
                bad_port = false;
            }
            // only allowed as a zone id inside an IPv6 literal
            b'%' => percent = true,
            _ => return false,
        }
    }
    open == closed && colons <= 1 && !percent && !bad_port
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_valid_host() {
        for host in [
            "",
            "a",
            "example.com",
            "example.com:8080",
            "example.com:",
            "127.0.0.1:80",
            "[::1]",
            "[::1]:80",
            "[1:2:3:4:5:6:7:8:9]",
            "[fe80::1%25eth0]:80",
            "a-b_c~d!$&'()*+,;=",
        ] {
            assert!(is_valid_host(host.as_bytes()), "{host:?}");
        }
        for host in [
            "user@example.com",
            "@",
            "exa mple.com",
            "example.com/path",
            "example.com?q",
            "example.com#f",
            "example.com:port",
            "example.com:80a",
            "[::1]:x",
            "a:1:2",
            "[1:2:3:4:5:6:7:8:9:0]",
            "[::1",
            "::1]",
            "[[::1]]",
            "[::1]]",
            "a%",
            "%[::1]",
            "[::1]%",
            "a\"b",
            "a\\b",
            "a{b}",
            "\u{e9}",
        ] {
            assert!(!is_valid_host(host.as_bytes()), "{host:?}");
        }
    }

    #[test]
    fn test_is_valid_host_matches_authority() {
        const ALPHABET: &[u8] = b"a1:[]%@/";

        // previous `Authority` based implementation
        fn reference(val: &[u8]) -> bool {
            if val.is_empty() {
                return true;
            }
            if val.contains(&b'@') || ntex_http::uri::Authority::try_from(val).is_err() {
                return false;
            }
            let host_end = val.iter().rposition(|&b| b == b']').unwrap_or(0);
            val[host_end..]
                .iter()
                .position(|&b| b == b':')
                .is_none_or(|pos| val[host_end + pos + 1..].iter().all(u8::is_ascii_digit))
        }

        for b in 0..=u8::MAX {
            for val in [
                vec![b],
                vec![b'a', b],
                vec![b'a', b':', b],
                vec![b'[', b':', b, b']'],
                vec![b'[', b':', b']', b],
            ] {
                assert_eq!(is_valid_host(&val), reference(&val), "{val:?}");
            }
        }

        // every combination of structural characters up to 6 bytes
        let mut val = Vec::new();
        for len in 1..=6u32 {
            for mut n in 0..ALPHABET.len().pow(len) {
                val.clear();
                for _ in 0..len {
                    val.push(ALPHABET[n % ALPHABET.len()]);
                    n /= ALPHABET.len();
                }
                assert_eq!(is_valid_host(&val), reference(&val), "{val:?}");
            }
        }
        // IPv6 literals around the colon limit
        for colons in 7..=10 {
            let val = format!("[{}1]:80", ":".repeat(colons));
            assert_eq!(
                is_valid_host(val.as_bytes()),
                reference(val.as_bytes()),
                "{val}"
            );
        }
    }
}
