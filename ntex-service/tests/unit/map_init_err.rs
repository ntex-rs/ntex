use crate::{ServiceFactory, factory, fn_factory, fn_service};

/// STATEMENT: Initialization-error mapping preserves successful construction and converts failures.
///
/// HOW:
/// - Clone a factory using an async move closure and a unit-to-I/O error mapper.
/// - Exercise failing and successful configurations and check the factory can be formatted.
#[ntex::test]
async fn map_init_err() {
    let factory = factory(fn_factory(async move |err: &bool| {
        if *err {
            Err(())
        } else {
            Ok(fn_service(async |i: usize| Ok::<_, ()>(i * 2)))
        }
    }))
    .map_init_err(|()| std::io::Error::other("err"))
    .clone();

    assert!(factory.create(&true).await.is_err());
    assert!(factory.create(&false).await.is_ok());
    let _ = format!("{factory:?}");
}

/// STATEMENT: An initialization-error mapper also works with a non-move async factory closure.
///
/// HOW:
/// - Wrap and clone a configuration-dependent factory with an I/O error mapper.
/// - Verify failure and success results for opposite configurations and format the factory.
#[ntex::test]
async fn map_init_err2() {
    let factory = factory(fn_factory(async |err: &bool| {
        if *err {
            Err(())
        } else {
            Ok(fn_service(async |i: usize| Ok::<_, ()>(i * 2)))
        }
    }))
    .map_init_err(|()| std::io::Error::other("err"))
    .clone();

    assert!(factory.create(&true).await.is_err());
    assert!(factory.create(&false).await.is_ok());
    let _ = format!("{factory:?}");
}
