use ntex_service::{Ctx, Service};

struct Wrapper<S> {
    service: S,
}

impl<S: Service<(), u32>> Service<(), u32> for Wrapper<S> {
    type Res = S::Res;
    type Error = S::Error;

    ntex_service::forward!((), inner.service);

    async fn call(&self, req: u32, ctx: Ctx<'_, Self>) -> Result<S::Res, S::Error> {
        ctx.call(&self.service, req).await
    }
}

fn main() {}
