use http::Uri;

use crate::Address;

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
            "http" => Some(80),
            "https" => Some(443),
            "ws" => Some(80),
            "wss" => Some(443),
            "amqp" => Some(5672),
            "amqps" => Some(5671),
            "sb" => Some(5671),
            "mqtt" => Some(1883),
            "mqtts" => Some(8883),
            _ => None,
        }
    } else {
        None
    }
}
