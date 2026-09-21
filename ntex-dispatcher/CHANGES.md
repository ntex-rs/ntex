# Changes

## [4.1.0] - 2026-09-21

* Start frame rate timing for new connections immediately

* Preserve partial-frame timing across service readiness pauses

* Preserve frame read-timeout classification when service readiness pauses reads

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
