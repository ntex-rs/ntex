use std::cell::Cell;
use std::net::SocketAddr;
use std::rc::Rc;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    addr: SocketAddr,
    secure: Rc<Cell<bool>>,
}

impl ServerConfig {
    pub fn new(addr: SocketAddr) -> Self {
        ServerConfig {
            addr,
            secure: Rc::new(Cell::new(false)),
        }
    }

    /// Returns the address of the local half of this TCP server socket
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Returns true if connection is secure (tls enabled)
    pub fn secure(&self) -> bool {
        self.secure.as_ref().get()
    }

    /// Set secure flag
    pub fn set_secure(&self) {
        self.secure.as_ref().set(true)
    }
}
