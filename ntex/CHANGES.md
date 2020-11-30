# Changes

## [0.1.25] - 2020-xx-xx

* Better names for Variant service

* Add Debug impl for FrozenClientRequest

* Add mpsc::WeakSender<T> type

## [0.1.24] - 2020-09-22

* Fix ws::stream::StreamDecoder, decodes buffer before reading from io #27

* Drop deprecated ntex::framed mod

## [0.1.23] - 2020-09-04

* Fix http1 pipeline requests with payload handling

## [0.1.22] - 2020-08-27

* Wake http client connection pool support future on drop, prevents memory leak.

* Make `Counter` non clonable.

* Fix `Address` trait usage for `net::SocketAddr` type

## [0.1.21] - 2020-07-29

* Optimize http/1 dispatcher

## [0.1.20] - 2020-07-06

* ntex::util: Add `Buffer` service

* ntex::framed: Deprecate

## [0.1.19] - 2020-06-12

* ntex::framed: Deprecate `Connect` and `ConnectResult`

* ntex::http: Move `Extensions` type to `ntex::util`

## [0.1.18] - 2020-05-29

* ntex::connect: Add `connect` helper function

* ntex::connect: Add `Address` impl for `SocketAddr`

## [0.1.17] - 2020-05-18

* ntex::util: Add Variant service

## [0.1.16] - 2020-05-10

* ntex::http: Remove redundant BodySize::Sized64

* ntex::http: Do not check h1 keep-alive during response processing

* ntex::channel: Split pooled oneshot to separate module

## [0.1.15] - 2020-05-03

* ntex::util: Refactor stream dispatcher

* ntex::http: Drop camel case headers support

* ntex::http: Fix upgrade service readiness check

* ntex::http: Add client websockets helper

* ntex::ws: Add stream and sink wrappers for ws protocol

* ntex::web: Add websockets helper

## [0.1.14] - 2020-04-27

* ntex::http: Stop client connections pool support future

* ntex::http: Removed IntoHeaderValue trait, use TryFrom instead

* ntex::ws: Fix wrong opcode for ws text and binary continuation frames

## [0.1.13] - 2020-04-21

* ntex::http: Refactor client connection pool

## [0.1.12] - 2020-04-20

* ntex::channel: Add mpsc close checks

* ntex::channel: Add oneshot close checks

## [0.1.11] - 2020-04-15

* ntex::web: Allow to add multiple routes at once

* ntex::web: Add `App::with_config` method, simplifies app service factory.

* ntex::web: Fix error type for Either responder

## [0.1.10] - 2020-04-13

* ntex::channel: mpsc::Sender::close() must close receiver

## [0.1.9] - 2020-04-13

* ntex::util: Refcator framed dispatcher

* ntex::framed: Use framed dispatcher instead of custom one

* ntex::channel: Fix mpsc::Sender close method.

## [0.1.8] - 2020-04-12

* ntex::web: Fix definition of `ok_service` and `default_service`.

* ntex::web: Add default error impl for `http::PayloadError`

* ntex::web: Add default error impl for `http::client::SendRequestError`

* ntex::web: Move `web::Data` to `web::types::Data`

* ntex::web: Simplify Responder trait

* ntex::web: Simplify WebResponse, remove `B` generic parameter

## [0.1.7] - 2020-04-10

* ntex::http: Fix handling of large http messages

* ntex::http: Refine read/write back-pressure for h1 dispatcher

* ntex::web: Restore proc macros for handler registration

## [0.1.6] - 2020-04-09

* ntex::web: Allow to add multiple services at once

* ntex::http: Remove ResponseBuilder::json2 method

## [0.1.5] - 2020-04-07

* ntex::http: enable client disconnect timeout by default

* ntex::http: properly close h1 connection

* ntex::framed: add connection disconnect timeout to framed service

## [0.1.4] - 2020-04-06

* Remove unneeded RefCell from client connector

* Add trace entries for http1 disaptcher

* Properly set timeout for test http client

## [0.1.3] - 2020-04-06

* Add server ssl handshake timeout

* Simplify server ssl erroor

## [0.1.2] - 2020-04-05

* HTTP1 dispatcher refactoring

* Replace net2 with socket2 crate

## [0.1.1] - 2020-04-01

* Project fork
