use crate::{Pipeline, ServiceFactory, fn_service_st, map_state, map_state_factory};

/// STATEMENT: State-mapped services and factories preserve calls and service shutdown.
///
/// HOW:
/// - Supply fixed inner state through cloned service and factory adapters.
/// - Check both call responses and the direct pipeline's shutdown transition.
#[ntex::test]
async fn test_map_state() {
    let svc = map_state(
        100,
        fn_service_st(|_: &usize, item: usize| async move { Ok::<_, ()>(item) }),
    )
    .clone();
    let _ = format!("{svc:?}");

    let svc = Pipeline::new((), svc);
    assert_eq!(svc.call(1).await.unwrap(), 1);
    assert!(!svc.is_shutdown());
    svc.shutdown().await;
    assert!(svc.is_shutdown());

    let factory = map_state_factory(
        100,
        fn_service_st(|_: &usize, item: usize| async move { Ok::<_, ()>(item) }),
    )
    .clone();
    let _ = format!("{factory:?}");

    let svc = Pipeline::new((), factory.create(&1).await.unwrap());
    assert_eq!(svc.call(1).await.unwrap(), 1);
}
