# Changes

## [0.1.6] - 2022-01-03

* Use ntex-rt::spawn

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
