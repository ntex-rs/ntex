use std::{future::Future, rc::Rc};

use crate::framed::{OnDisconnect, State};
use crate::ws;

pub struct WsSink(Rc<WsSinkInner>);

struct WsSinkInner {
    state: State,
    codec: ws::Codec,
}

impl WsSink {
    pub(crate) fn new(state: State, codec: ws::Codec) -> Self {
        Self(Rc::new(WsSinkInner { state, codec }))
    }

    /// Endcode and send message to the peer.
    pub fn send(
        &self,
        item: ws::Message,
    ) -> impl Future<Output = Result<(), ws::ProtocolError>> {
        let inner = self.0.clone();

        async move {
            inner.state.write().encode(item, &inner.codec)?;
            Ok(())
        }
    }

    /// Notify when connection get disconnected
    pub fn on_disconnect(&self) -> OnDisconnect {
        self.0.state.on_disconnect()
    }
}
