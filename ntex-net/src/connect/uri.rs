use super::Address;

impl Address for urly::Url {
    fn host(&self) -> &str {
        self.host().unwrap_or("")
    }

    fn port(&self) -> Option<u16> {
        // Azure Service Bus uses amqps
        self.port_or_known_default()
            .or_else(|| (self.scheme_str() == Some("sb")).then_some(5671))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_tests() {
        for (s, p) in [
            ("http", 80),
            ("https", 443),
            ("ws", 80),
            ("wss", 443),
            ("amqp", 5672),
            ("amqps", 5671),
            ("sb", 5671),
            ("mqtt", 1883),
            ("mqtts", 8883),
            ("ftp", 21),
        ] {
            let url = urly::Url::try_from(format!("{s}://h/")).unwrap();
            assert_eq!(Address::port(&url), Some(p), "{s}");
        }
        let url = urly::Url::from_static("unknowns://h/");
        assert_eq!(Address::port(&url), None);
    }

    #[test]
    fn url_address() {
        let url = urly::Url::from_static("mqtts://[::1]/a");
        assert_eq!(Address::host(&url), "[::1]");
        assert_eq!(Address::port(&url), Some(8883));
        let url = urly::Url::from_static("http://example.com:8080/");
        assert_eq!(Address::host(&url), "example.com");
        assert_eq!(Address::port(&url), Some(8080));
        let url = urly::Url::from_static("/a");
        assert_eq!(Address::host(&url), "");
        assert_eq!(Address::port(&url), None);
    }
}
