use ntex_service::{apply, fn_service};

fn main() {
    let _ = apply(
        async |req: u32| req,
        fn_service(async |req: u32| Ok::<_, ()>(req)),
    );
}
