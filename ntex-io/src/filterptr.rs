use std::{any, cell::UnsafeCell, mem, ptr};

use crate::{Filter, FilterLayer, Layer, Sealed, filter::NullFilter};

enum Repr {
    Filter(*const u8, *const dyn Filter),
    Sealed(Box<dyn Filter>),
}

impl Repr {
    fn typed<F: Filter>(filter: F) -> Self {
        // derive the `dyn` pointer from the raw pointer, a reference taken
        // from the `Box` is invalidated by `Box::into_raw`
        let ptr = Box::into_raw(Box::new(filter));
        Repr::Filter(
            ptr.cast_const().cast(),
            ptr.cast_const() as *const dyn Filter,
        )
    }
}

impl Default for Repr {
    fn default() -> Self {
        Repr::Filter(ptr::null(), NullFilter::get())
    }
}

pub(crate) struct FilterPtr(UnsafeCell<Repr>);

impl FilterPtr {
    pub(crate) const fn null() -> Self {
        Self(UnsafeCell::new(Repr::Filter(
            ptr::null(),
            NullFilter::get(),
        )))
    }

    pub(crate) fn get(&self) -> &dyn Filter {
        match self.as_ref() {
            Repr::Filter(_, filter) => unsafe { filter.as_ref().unwrap() },
            Repr::Sealed(b) => b.as_ref(),
        }
    }

    pub(crate) fn set<F: Filter>(&self, filter: F) {
        *self.as_mut() = Repr::typed(filter);
    }

    /// Get filter, panic if it is not filter
    pub(crate) fn filter<F: Filter>(&self) -> &F {
        if let Repr::Filter(ptr, _) = self.as_ref() {
            assert!(!ptr.is_null(), "Filter is not set");
            unsafe { (*ptr).cast::<F>().as_ref().unwrap() }
        } else {
            panic!("Filter is sealed")
        }
    }

    #[allow(clippy::mut_from_ref)]
    fn as_mut(&self) -> &mut Repr {
        unsafe { &mut *self.0.get() }
    }

    fn as_ref(&self) -> &Repr {
        unsafe { &*self.0.get() }
    }

    #[allow(clippy::unnecessary_box_returns)]
    /// Get filter, panic if it is not set
    pub(crate) fn take_filter<F>(&self) -> Box<F> {
        if let Repr::Filter(ptr, _) = mem::take(self.as_mut()) {
            assert!(!ptr.is_null(), "Filter is not set");
            unsafe { Box::from_raw(ptr as *mut F) }
        } else {
            panic!("Filter is sealed")
        }
    }

    /// Get sealed, panic if it is already sealed
    fn take_sealed(&self) -> Sealed {
        if let Repr::Sealed(sealed) = mem::take(self.as_mut()) {
            Sealed(sealed)
        } else {
            panic!("Filter is not sealed")
        }
    }

    pub(crate) fn is_set(&self) -> bool {
        match self.as_ref() {
            Repr::Filter(ptr, _) => !ptr.is_null(),
            Repr::Sealed(_) => true,
        }
    }

    pub(crate) fn add_filter<F: Filter, T: FilterLayer>(&self, new: T) {
        assert!(self.is_set(), "Filter is not set");

        let repr = match self.as_ref() {
            Repr::Filter(..) => Repr::typed(Layer::new(new, *self.take_filter::<F>())),
            // The new layer is typed, only a bare `Sealed` stays in `Repr::Sealed`
            Repr::Sealed(..) => Repr::typed(Layer::new(new, self.take_sealed())),
        };
        *self.as_mut() = repr;
    }

    pub(crate) fn map_filter<F: Filter, U, R>(&self, f: U)
    where
        U: FnOnce(F) -> R,
        R: Filter,
    {
        *self.as_mut() = Repr::typed(f(self.take_owned::<F>()));
    }

    /// Takes the filter by value, `F` is `Sealed` if the filter is sealed
    fn take_owned<F: Filter>(&self) -> F {
        if matches!(self.as_ref(), Repr::Sealed(..)) {
            assert!(
                any::TypeId::of::<F>() == any::TypeId::of::<Sealed>(),
                "Filter is sealed"
            );
            let sealed = mem::ManuallyDrop::new(self.take_sealed());
            // Safety: `F` is `Sealed`
            unsafe { ptr::read(ptr::from_ref(&*sealed).cast::<F>()) }
        } else {
            *self.take_filter::<F>()
        }
    }

    pub(crate) fn seal<F: Filter>(&self) {
        assert!(self.is_set(), "Filter is not set");

        if matches!(self.as_ref(), Repr::Filter(..)) {
            *self.as_mut() = Repr::Sealed(Box::new(*self.take_filter::<F>()));
        }
    }

    pub(crate) fn drop_filter<F>(&self) {
        if let Repr::Filter(ptr, _) = self.as_ref() {
            if !ptr.is_null() {
                self.take_filter::<F>();
            }
        } else {
            self.take_sealed();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, io, rc::Rc};

    use ntex_bytes::Bytes;
    use ntex_codec::BytesCodec;

    use super::*;
    use crate::{Base, FilterBuf, Handle, Io, IoContext, IoStream, testing::IoTest};

    const BIN: &[u8] = b"GET /test HTTP/1\r\n\r\n";
    const TEXT: &str = "GET /test HTTP/1\r\n\r\n";

    #[derive(Debug)]
    struct DropFilter {
        p: Rc<Cell<usize>>,
    }

    impl Drop for DropFilter {
        fn drop(&mut self) {
            self.p.set(self.p.get() + 1);
        }
    }

    impl FilterLayer for DropFilter {
        fn process_read_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            buf.with_read_buffers(|src, dst| {
                if let Some(src) = src.take() {
                    dst.extend_from_slice(&src);
                }
                Ok(())
            })
        }

        fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            buf.with_write_buffers(|src, dst| {
                src.move_to(dst);
                Ok(())
            })
        }
    }

    struct IoTestWrapper;

    impl IoStream for IoTestWrapper {
        fn start(self, _: IoContext) -> Box<dyn Handle> {
            Box::new(TestHandle)
        }
    }

    struct TestHandle;

    impl Handle for TestHandle {}

    #[ntex::test]
    async fn drop_filter() {
        let p = Rc::new(Cell::new(0));

        let (client, server) = IoTest::create();
        let f = DropFilter { p: p.clone() };
        let _ = format!("{f:?}");
        let io = Io::from(server).add_filter(f);

        client.remote_buffer_cap(1024);
        client.write(TEXT);
        let msg = io.recv(&BytesCodec).await.unwrap().unwrap();
        assert_eq!(msg, Bytes::from_static(BIN));

        io.send(Bytes::from_static(b"test"), &BytesCodec)
            .await
            .unwrap();
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"test"));

        let io2 = io.take();
        let mut io3: crate::IoBoxed = io2.into();
        let io4 = io3.take();

        drop(io);
        drop(io3);
        drop(io4);

        assert_eq!(p.get(), 1);
    }

    #[test]
    fn miri_sealed_filter() {
        let p = Rc::new(Cell::new(0));
        let f = DropFilter { p: p.clone() };

        let io = Io::from(IoTestWrapper).seal();
        let _io: Io<Layer<DropFilter, Sealed>> = io.add_filter(f);
    }

    #[test]
    fn miri_sealed_layer_typed_access() {
        let p = Rc::new(Cell::new(0));

        let io = Io::from(IoTestWrapper).seal();
        let io = io.add_filter(DropFilter { p: p.clone() });
        assert!(Rc::ptr_eq(&io.filter().p, &p));

        let io = io.map_filter(|layer| layer);
        assert!(Rc::ptr_eq(&io.filter().p, &p));
        let io = io.add_filter(DropFilter { p: p.clone() });
        assert!(Rc::ptr_eq(&io.filter().p, &p));
        assert_eq!(p.get(), 0);

        drop(io);
        assert_eq!(p.get(), 2);
    }

    #[test]
    fn miri_sealed_map_filter() {
        let p = Rc::new(Cell::new(0));

        let io = Io::from(IoTestWrapper).seal();
        let io: Io<Sealed> = io.map_filter(|sealed| sealed);
        let io = io.map_filter(|sealed| Layer::new(DropFilter { p: p.clone() }, sealed));
        assert!(Rc::ptr_eq(&io.filter().p, &p));

        drop(io);
        assert_eq!(p.get(), 1);
    }

    #[ntex::test]
    async fn map_filter_panic_closes_connection() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let io = Io::from(server);
        let ioref = io.get_ref();
        io.encode_slice(b"out").unwrap();

        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            io.map_filter(|_: Base| -> Base { panic!("map failed") })
        }));
        assert!(res.is_err());
        assert!(!ioref.is_active());
        drop(client);
    }

    #[test]
    #[should_panic(expected = "Filter is not set")]
    fn miri_filter_access() {
        let fptr = FilterPtr::null();
        fptr.filter::<Base>();
    }
}
