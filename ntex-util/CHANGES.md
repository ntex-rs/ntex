# Changes

## [0.1.9] - 2022-01-12

* Add Pool::shrink_to_fit() method

## [0.1.8] - 2022-01-10

* Add pool::Receiver::poll_recv() method

* Add oneshot::Receiver::poll_recv() method

## [0.1.7] - 2022-01-04

* Add time::timeout_checked, if duration is zero then timeout is disabled

## [0.1.6] - 2022-01-03

* Use ntex-rt::spawn

* Move ntex::util services

## [0.1.5] - 2021-12-27

* Fix borrow error when timer get dropped immidietly after start

## [0.1.4] - 2021-12-21

* mpsc: add Receiver::poll_recv() method

## [0.1.3] - 2021-12-18

* move ntex::channel::mpsc

## [0.1.2] - 2021-12-10

* move in ntex::time utils

* replace tokio::time with futures-timer

## [0.1.1] - 2021-04-11

* next renamed to stream_recv

* send renamed to sink_write

## [0.1.0] - 2021-04-04

* Move utils to separate crate
