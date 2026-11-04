use ntex_service::{Ctx, Service};

struct MyError;

struct Wrapper<S> {
    service: S,
}

impl<S: Service<(), u32, Error = u8>> Service<(), u32> for Wrapper<S> {
    type Res = S::Res;
    type Error = MyError;

    ntex_service::forward!((), service);

    async fn call(&self, req: u32, ctx: Ctx<'_, Self>) -> Result<S::Res, MyError> {
        ctx.call(&self.service, req).await.map_err(|_| MyError)
    }
}

fn main() {}
