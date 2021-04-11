# Changes

## [0.3.15] - 2021-04-11

* Move various utils to ntex-util crate

## [0.3.14] - 2021-04-03

* server: prevent double socket registration if accept loop is in back-pressure state

* util: add custom Ready, Either future and several helper functions

* drop trust-dns, use blocking calls

* reduce futures crate dependencies

* make url crate optional

## [0.3.13] - 2021-03-26

* framed: add socket disconnect notification

* http: wake up reader on h1 payload error

* ws: add sink disconnect notification

* fix wrong api docs

## [0.3.12] - 2021-03-18

* http: add per request handler service for http1

## [0.3.11] - 2021-03-16

* web: use patterns for scope's prefix definitions

* web: allow case-insensitive request matching on scope level

* web: add helper method `App::finish()`, creates service factory with default `AppConfig`

* web: add `.filter()` method, allows to register request filters

## [0.3.10] - 2021-03-15

* add buffer_params() api

## [0.3.9] - 2021-03-15

* framed: refactor api

* update socket2 0.4

## [0.3.8] - 2021-03-11

* http: fix expect/continue support, wake up write task

* framed: wakeup write task if write buf has new data

## [0.3.7] - 2021-03-10

* http: Fix service error handling for h1 proto

## [0.3.6] - 2021-03-06

* http.client: Fix WsConnection::start() definition

* http.client: Introduce WsConnection::start_default() method

* web: TestServer::ws() returns WsConnection

* util: Add `SinkService` service

## [0.3.5] - 2021-03-04

* framed: add high/low watermark for read/write buffers

* framed: write task could panic if receives more that 512 bytes during shutdown

* http/web: add high/low watermark for read/write buffers

## [0.3.4] - 2021-03-02

* Allow to use async fn for server configuration

## [0.3.3] - 2021-02-27

* Remove unneeded set_nonblocking() call from server accept loop

* Do not set `reuse_address` for tcp listener on window os

* Set nodelay to accept/connect sockets

* Update ntex-router v0.4.1

* Update cookie v0.15.0

## [0.3.2] - 2021-02-25

* Re-export various types

* Use `main` and `test` proc macro from ntex-macros

## [0.3.1] - 2021-02-24

* server: Make TestServer::connect() async

## [0.3.0] - 2021-02-24

* Migrate to tokio 1.x

## [0.2.1] - 2021-02-22

* http: Fix http date header update task

* http: Add ClientResponse::header() method

* framed: Refactor write back-pressure support

## [0.2.0] - 2021-02-21

* 0.2 release

## [0.2.0-b.14] - 2021-02-20

* connect: Allow to access to inner type of Connect

## [0.2.0-b.13] - 2021-02-20

* http: Refactor date service

* http: Do not leak request/response pools

* server: Rename ServerBulder::system_exit to stop_runtime

* util: Drop Either service, use Variant instead

## [0.2.0-b.12] - 2021-02-18

* http: Fix KeepAlive::Os support for h1 dispatcher

* Handle EINTR in server accept loop

* Fix double registation for accept back-pressure

## [0.2.0-b.11] - 2021-02-02

* framed: fix wake write method dsp_restart_write_task

## [0.2.0-b.10] - 2021-01-28

* framed: Allow to wake up write io task

* framed: Prevent uneeded read task wakeups

* framed: Cleanup State impl

## [0.2.0-b.7] - 2021-01-25

* Fix error handling for framed disaptcher

* Refactor framed disaptcher write back-pressure support

* Replace actix-threadpool with tokio utils

## [0.2.0-b.6] - 2021-01-24

* http: Pass io stream to upgrade handler

## [0.2.0-b.5] - 2021-01-23

* accept shared ref in some methods of framed::State type

## [0.2.0-b.4] - 2021-01-23

* http: Refactor h1 dispatcher

* http: Remove generic type from `Request`

* http: Remove generic type from `Payload`

* Rename FrameReadTask/FramedWriteTask to ReadTask/WriteTask

## [0.2.0-b.3] - 2021-01-21

* Allow to use framed write task for io flushing

## [0.2.0-b.2] - 2021-01-20

* Fix flush framed write task

## [0.2.0-b.1] - 2021-01-19

* Introduce ntex::framed module

* Upgrade to ntex-codec 0.2

* Drop deprecated ntex::util::order

## [0.1.29] - 2021-01-14

* Revert http/1 disapatcher changes

## [0.1.28] - 2021-01-14

* Flush and close io after ws handler exit

* Deprecate ntex::util::order

## [0.1.27] - 2021-01-13

* Use ahash instead of fxhash

* Use pin-project-lite instead of pin-project

## [0.1.26] - 2020-12-22

* Update deps

* Optimize set_date_header

## [0.1.25] - 2020-11-30

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
