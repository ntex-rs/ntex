use ntex_http::Uri;

use super::Address;

impl Address for Uri {
    fn host(&self) -> &str {
        self.host().unwrap_or("")
    }

    fn port(&self) -> Option<u16> {
        if let Some(port) = self.port_u16() {
            Some(port)
        } else {
            port(self.scheme_str())
        }
    }
}

// TODO: load data from file
fn port(scheme: Option<&str>) -> Option<u16> {
    if let Some(scheme) = scheme {
        match scheme {
            "http" | "ws" => Some(80),
            "https" | "wss" => Some(443),
            "amqp" => Some(5672),
            "amqps" | "sb" => Some(5671),
            "mqtt" => Some(1883),
            "mqtts" => Some(8883),
            _ => None,
        }
    } else {
        None
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
        ] {
            assert_eq!(port(Some(s)), Some(p));
        }
        assert_eq!(port(Some("unknowns")), None);
        assert_eq!(port(None), None);
    }
}
