use std::{future::Future, rc::Rc};

use crate::framed::State;
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

    pub fn send(
        &self,
        item: ws::Message,
    ) -> impl Future<Output = Result<(), ws::ProtocolError>> {
        let inner = self.0.clone();

        async move {
            inner.state.write_item(item, &inner.codec)?;
            Ok(())
        }
    }
}
