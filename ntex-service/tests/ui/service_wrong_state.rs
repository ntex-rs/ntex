use ntex_service::{Ctx, Pipeline, Service};

struct Echo;

impl Service<u32, ()> for Echo {
    type Res = ();
    type Error = ();

    async fn call(&self, _: (), _: Ctx<'_, Self, u32>) -> Result<(), ()> {
        Ok(())
    }
}

fn main() {
    let _ = Pipeline::new((), Echo);
}
