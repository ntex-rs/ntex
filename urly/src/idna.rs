//! Punycode (RFC 3492) and IDNA label conversion.
//!
//! Only lowercasing and punycode are applied, there is no full UTS #46 mapping.
use std::borrow::Cow;

const BASE: u32 = 36;
const TMIN: u32 = 1;
const TMAX: u32 = 26;
const SKEW: u32 = 38;
const DAMP: u32 = 700;
const INITIAL_BIAS: u32 = 72;
const INITIAL_N: u32 = 128;
const PREFIX: &str = "xn--";

fn adapt(mut delta: u32, num_points: u32, first: bool) -> u32 {
    delta /= if first { DAMP } else { 2 };
    delta += delta / num_points;
    let mut k = 0;
    while delta > ((BASE - TMIN) * TMAX) / 2 {
        delta /= BASE - TMIN;
        k += BASE;
    }
    k + (BASE - TMIN + 1) * delta / (delta + SKEW)
}

fn threshold(k: u32, bias: u32) -> u32 {
    if k <= bias {
        TMIN
    } else if k >= bias + TMAX {
        TMAX
    } else {
        k - bias
    }
}

fn digit(d: u32) -> char {
    // d < BASE
    if d < 26 {
        (b'a' + d as u8) as char
    } else {
        (b'0' + (d - 26) as u8) as char
    }
}

/// Encodes code points to punycode, without the `xn--` prefix.
pub(crate) fn encode(input: &[char]) -> Option<String> {
    let mut out: String = input.iter().filter(|c| c.is_ascii()).collect();
    let basic = u32::try_from(out.len()).ok()?;
    let len = u32::try_from(input.len()).ok()?;
    if basic > 0 {
        out.push('-');
    }

    let mut n = INITIAL_N;
    let mut delta: u32 = 0;
    let mut bias = INITIAL_BIAS;
    let mut h = basic;
    while h < len {
        let m = input.iter().map(|c| *c as u32).filter(|c| *c >= n).min()?;
        delta = delta.checked_add((m - n).checked_mul(h + 1)?)?;
        n = m;
        for c in input.iter().map(|c| *c as u32) {
            if c < n {
                delta = delta.checked_add(1)?;
            }
            if c == n {
                let mut q = delta;
                let mut k = BASE;
                loop {
                    let t = threshold(k, bias);
                    if q < t {
                        break;
                    }
                    out.push(digit(t + (q - t) % (BASE - t)));
                    q = (q - t) / (BASE - t);
                    k += BASE;
                }
                out.push(digit(q));
                bias = adapt(delta, h + 1, h == basic);
                delta = 0;
                h += 1;
            }
        }
        delta = delta.checked_add(1)?;
        n += 1;
    }
    Some(out)
}

/// Decodes punycode, without the `xn--` prefix.
pub(crate) fn decode(input: &str) -> Option<String> {
    let (basic, rest) = match input.rfind('-') {
        Some(i) => (&input[..i], &input[i + 1..]),
        None => ("", input),
    };
    if !basic.is_ascii() {
        return None;
    }
    let mut out: Vec<char> = basic.chars().collect();
    let mut n = INITIAL_N;
    let mut i: u32 = 0;
    let mut bias = INITIAL_BIAS;
    let mut digits = rest.bytes().peekable();
    while digits.peek().is_some() {
        let old_i = i;
        let mut w: u32 = 1;
        let mut k = BASE;
        loop {
            let d = match digits.next()? {
                b @ b'a'..=b'z' => b - b'a',
                b @ b'A'..=b'Z' => b - b'A',
                b @ b'0'..=b'9' => b - b'0' + 26,
                _ => return None,
            };
            let d = u32::from(d);
            i = i.checked_add(d.checked_mul(w)?)?;
            let t = threshold(k, bias);
            if d < t {
                break;
            }
            w = w.checked_mul(BASE - t)?;
            k += BASE;
        }
        let len = u32::try_from(out.len()).ok()? + 1;
        bias = adapt(i - old_i, len, old_i == 0);
        n = n.checked_add(i / len)?;
        i %= len;
        let c = char::from_u32(n).filter(|c| !c.is_ascii())?;
        out.insert(i as usize, c);
        i += 1;
    }
    Some(out.into_iter().collect())
}

fn is_label_separator(c: char) -> bool {
    matches!(c, '.' | '\u{3002}' | '\u{FF0E}' | '\u{FF61}')
}

/// Converts a domain to its ASCII form: labels are lowercased, non-ASCII
/// labels are punycode-encoded with the `xn--` prefix.
pub(crate) fn domain_to_ascii(domain: &str) -> Option<String> {
    let mut out = String::with_capacity(domain.len() + 8);
    for (idx, label) in domain.split(is_label_separator).enumerate() {
        if idx > 0 {
            out.push('.');
        }
        if label.is_ascii() {
            out.extend(label.chars().map(|c| c.to_ascii_lowercase()));
        } else {
            let chars: Vec<char> = label.chars().flat_map(char::to_lowercase).collect();
            out.push_str(PREFIX);
            out.push_str(&encode(&chars)?);
        }
    }
    Some(out)
}

/// Converts `xn--` labels of a domain to unicode. Labels that fail to decode
/// are kept as is.
pub(crate) fn domain_to_unicode(domain: &str) -> Cow<'_, str> {
    let has_prefix = |label: &str| {
        label
            .get(..PREFIX.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(PREFIX))
    };
    if !domain.split('.').any(has_prefix) {
        return Cow::Borrowed(domain);
    }
    let mut out = String::with_capacity(domain.len());
    for (idx, label) in domain.split('.').enumerate() {
        if idx > 0 {
            out.push('.');
        }
        match has_prefix(label)
            .then(|| decode(&label[PREFIX.len()..]))
            .flatten()
        {
            Some(decoded) => out.push_str(&decoded),
            None => out.push_str(label),
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn punycode() {
        let cases = [
            ("ü", "tda"),
            ("münchen", "mnchen-3ya"),
            ("bücher", "bcher-kva"),
            ("mañana", "maana-pta"),
            ("中国", "fiqs8s"),
            ("правда", "80aafi6cg"),
            // RFC 3492 7.1 (L)
            ("3年B組金八先生", "3B-ww4c5e180e575a65lsy2b"),
        ];
        for (unicode, encoded) in cases {
            let chars: Vec<char> = unicode.chars().collect();
            assert_eq!(encode(&chars).unwrap(), encoded, "{unicode}");
            assert_eq!(decode(encoded).unwrap(), unicode, "{encoded}");
        }
        assert_eq!(decode("a!"), None);
        assert_eq!(decode("99999999999"), None);
    }

    #[test]
    fn domains() {
        assert_eq!(domain_to_ascii("MÜnchen.DE").unwrap(), "xn--mnchen-3ya.de");
        assert_eq!(
            domain_to_ascii("例子。中国").unwrap(),
            "xn--fsqu00a.xn--fiqs8s"
        );
        assert_eq!(domain_to_unicode("xn--mnchen-3ya.de"), "münchen.de");
        assert_eq!(domain_to_unicode("XN--fiqs8s"), "中国");
        assert_eq!(domain_to_unicode("xn--a!.com"), "xn--a!.com");
        assert!(matches!(domain_to_unicode("a.com"), Cow::Borrowed(_)));
    }
}
