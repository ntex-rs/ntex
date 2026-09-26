# Changes

## [4.1.0] - 2026-09-21

* Report `Reason::Io(Some(UnexpectedEof))` when the peer closes cleanly while
  undecodable bytes remain in the read buffer, instead of a clean disconnect

* Simplify internal state

* Enforce the IoConfig write timeout from the moment write backpressure is
  enabled until it is disabled; a peer that does not release backpressure in
  time is stopped with the new Reason::WriteTimeout. Backpressure released while
  the service is not ready also ends the timeout

* Stop the frame read timer when a frame completes

* Stop keep-alive and frame read timers during write backpressure when no write
  timeout is configured, as frames are not decoded then

* Count bytes consumed by the codec as frame read progress

* Restart frame read-rate tracking with a fresh period and `max_timeout` budget
  when the service is not ready

* Start frame read-rate tracking for the first frame when the connection arrives

* Run the keep-alive timer only while the connection is idle; it is stopped
  while a frame is read or handled and starts once the last response is done

* Handle external timeouts (`IoRef::notify_timeout()`) the same way while the
  service is paused: an idle dispatcher, with no frames in flight, is stopped
  with Reason::KeepAliveTimeout, otherwise the timeout is ignored. Pausing no
  longer discards a pending external timeout

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
