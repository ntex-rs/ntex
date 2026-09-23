# Changes

## [4.1.0] - 2026-09-21

* Stop the frame read timer when a frame completes, so it cannot close the connection as a keep-alive timeout

* Count bytes consumed by the codec as frame read progress; a shrinking read buffer no longer underflows the rate check

* Start frame read-rate tracking for the first frame when the connection arrives

* Transport failures and force-closes cancel the dispatcher-held pending response future

## [4.0.1] - 2026-09-18

* Api docs improvements

## [4.0.0] - 2026-09-14

* Migrate to ntex-service 5

## [3.2.1] - 2026-06-15

* Send `Control::WBackPressureEnabled` control message only once per back-pressure

## [3.2.0] - 2026-05-08

* Do not use deprecated methods

## [3.1.0] - 2026-02-12

* Ignore spurious DSP_TIMEOUT when keep-alive is disabled #756

## [3.0.0] - 2026-01-27

* Move ntex_io::Dispatcher to separate crate
