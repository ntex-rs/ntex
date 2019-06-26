use std::fmt;
use std::ops::{Deref, DerefMut};

use actix_codec::{Decoder, Encoder};

use crate::cell::Cell;
use crate::sink::Sink;

pub struct Item<S, Codec: Encoder + Decoder> {
    state: Cell<S>,
    sink: Sink<<Codec as Encoder>::Item>,
    item: <Codec as Decoder>::Item,
}

impl<St, Codec> Item<St, Codec>
where
    Codec: Encoder + Decoder,
{
    pub(crate) fn new(
        state: Cell<St>,
        sink: Sink<<Codec as Encoder>::Item>,
        item: <Codec as Decoder>::Item,
    ) -> Self {
        Item { state, sink, item }
    }

    #[inline]
    pub fn state(&self) -> &St {
        self.state.get_ref()
    }

    #[inline]
    pub fn state_mut(&mut self) -> &mut St {
        self.state.get_mut()
    }

    #[inline]
    pub fn sink(&self) -> &Sink<<Codec as Encoder>::Item> {
        &self.sink
    }

    #[inline]
    pub fn into_inner(self) -> <Codec as Decoder>::Item {
        self.item
    }
}

impl<St, Codec> Deref for Item<St, Codec>
where
    Codec: Encoder + Decoder,
{
    type Target = <Codec as Decoder>::Item;

    #[inline]
    fn deref(&self) -> &<Codec as Decoder>::Item {
        &self.item
    }
}

impl<St, Codec> DerefMut for Item<St, Codec>
where
    Codec: Encoder + Decoder,
{
    #[inline]
    fn deref_mut(&mut self) -> &mut <Codec as Decoder>::Item {
        &mut self.item
    }
}

impl<St, Codec> fmt::Debug for Item<St, Codec>
where
    Codec: Encoder + Decoder,
    <Codec as Decoder>::Item: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("FramedItem").field(&self.item).finish()
    }
}
