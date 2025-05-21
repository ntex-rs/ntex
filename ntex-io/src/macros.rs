/// An implementation of [`poll_read/write_ready`] that forwards readiness checks to a field.
#[macro_export]
macro_rules! forward_ready {
    ($field:ident) => {
        #[inline]
        fn poll_read_ready(
            &self,
            cx: &mut std::task::Context<'_>,
        ) -> Poll<$crate::Readiness> {
            self.$field.poll_read_ready(cx)
        }

        #[inline]
        fn poll_write_ready(
            &self,
            cx: &mut std::task::Context<'_>,
        ) -> Poll<$crate::Readiness> {
            self.$field.poll_write_ready(cx)
        }
    };
}

#[macro_export]
macro_rules! forward_query {
    ($field:ident) => {
        #[inline]
        fn query(&self, id: std::any::TypeId) -> Option<Box<dyn std::any::Any>> {
            self.$field.query(id)
        }
    };
}

#[macro_export]
macro_rules! forward_shutdown {
    ($field:ident) => {
        #[inline]
        fn shutdown(
            &self,
            ctx: $crate::FilterCtx<'_>,
        ) -> std::io::Result<std::task::Poll<()>> {
            self.$field.shutdown(ctx)
        }
    };
}
