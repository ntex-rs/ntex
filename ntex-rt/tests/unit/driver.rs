use super::DriverType;

/// STATEMENT: Driver names and polling classification distinguish the supported I/O backends.
///
/// HOW:
/// - Check each driver's public name and verify only the polling driver reports polling mode.
#[test]
fn driver_type() {
    assert_eq!(DriverType::Poll.name(), "polling");
    assert_eq!(DriverType::IoUring.name(), "io-uring");
    assert_eq!(DriverType::Iocp.name(), "iocp");
    assert!(DriverType::Poll.is_polling());
    assert!(!DriverType::IoUring.is_polling());
    assert!(!DriverType::Iocp.is_polling());
}
