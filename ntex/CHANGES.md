# Changes

## [2.6.0] - 2024-09-25

* Disable default features for rustls

## [2.5.0] - 2024-09-24

* Allow to set io tag for web server

## [2.4.0] - 2024-09-05

* Add experimental `compio` runtime support

## [2.3.0] - 2024-08-13

* web: Allow to use generic middlewares #390

## [2.2.0] - 2024-08-12

* Http server gracefull shutdown support

## [2.1.0] - 2024-07-30

* Better handling for connection upgrade #385

## [2.0.3] - 2024-06-27

* Re-export server signals api

## [2.0.2] - 2024-06-22

* web: Cleanup http request in cache

* http: Fix handling of connection header

## [2.0.1] - 2024-05-29

* http: Fix handling payload timer after payload got consumed

* http: Fix handling not consumed request's payload

## [2.0.0] - 2024-05-28

* Use "async fn" for Service::ready() and Service::shutdown()

## [1.2.1] - 2024-03-28

* Feature gate websocket support #320

* Feature gate brotli2 encoder

## [1.2.0] - 2024-03-24

* Refactor server workers management

* Move ntex::server to separate crate

* Use ntex-net

## [1.1.2] - 2024-03-12

* Update ntex-h2

## [1.1.1] - 2024-03-11

* http: Replace EncodeError::Internal with Fmt error

## [1.1.0] - 2024-02-07

* http: Add http/1 control service

* http: Add http/2 control service

## [1.0.0] - 2024-01-09

* web: Use async fn for Responder and Handler traits

## [1.0.0-b.1] - 2024-01-08

* web: Refactor FromRequest trait, use async fn

* Refactor io tls filters

* Update cookie related api

## [1.0.0-b.0] - 2024-01-07

* Use "async fn" in trait for Service definition

## [0.7.17] - 2024-01-05

* Allow to set default response payload limit and timeout

## [0.7.16] - 2023-12-15

* Stop timer before handling UPGRADE h1 requests

## [0.7.15] - 2023-12-14

* Better io tags handling

## [0.7.14] - 2023-12-12

* Add io tag support for server

## [0.7.13] - 2023-11-29

* Refactor h1 timers

## [0.7.12] - 2023-11-22

* Replace async-oneshot with oneshot

## [0.7.11] - 2023-11-20

* Refactor http/1 timeouts

* Add http/1 payload read timeout

## [0.7.10] - 2023-11-12

* Start http client timeout after sending body

## [0.7.9] - 2023-11-11

* Update ntex io

## [0.7.8] - 2023-11-06

* Stopping Server does not release resources #233

* Drop num_cpu dep

## [0.7.7] - 2023-10-23

* Fix rust tls client TLS_SERVER_ROOTS #232

## [0.7.6] - 2023-10-16

* Upgrade ntex-h2 to 0.4

## [0.7.5] - 2023-10-01

* Fix compile error for 'compress' feature with async-std & glommio #226

## [0.7.4] - 2023-09-11

* Add missing fmt::Debug impls

## [0.7.3] - 2023-08-10

* Update ntex-service

## [0.7.1] - 2023-06-23

* `PipelineCall` is static

## [0.7.0] - 2023-06-22

* Release v0.7.0

## [0.7.0-beta.2] - 2023-06-21

* Remove unsafe from h1 dispatcher

## [0.7.0-beta.1] - 2023-06-19

* Rename Ctx to ServiceCtx

## [0.7.0-beta.0] - 2023-06-16

* Migrate to ntex-service 1.2

## [0.6.7] - 2023-04-14

* Remove Rc<Service> usage, update service and util deps

## [0.6.6] - 2023-04-11

* http: Better http2 config handling

## [0.6.5] - 2023-03-15

* web: Proper handling responses from ws web handler

## [0.6.4] - 2023-03-11

* http: Add `ClientResponse::headers_mut()` method

* http: Don't stop h1 dispatcher on upgrade handler with await #178

* web: `AppConfig` can be created with custom parameters via `new()`

## [0.6.2] - 2023-01-24

* Update ntex-io, ntex-tls deps

## [0.6.1] - 2023-01-23

* Refactor io subsystem

## [0.6.0] - 2023-01-04

* Upgrade to ntex-service 1.0

## [0.6.0-beta.0] - 2022-12-28

* Upgrade to ntex-service 0.4

* web: Refactor FromRequest trait, allow to borrow from request

* web: Remove useless Responder::Error

## [0.5.31] - 2022-11-30

* http: Don't require mutable self reference in `Response::extensions_mut()` method

## [0.5.30] - 2022-11-25

* Change `App::state()` behaviour

* Remove `App::app_state()` method

## [0.5.29] - 2022-11-03

* Handle io disconnect during h1/h2 server handling

* Cleanup internal h2 client data on request future drop, potential leak

## [0.5.28] - 2022-11-03

* Drop direct http crate dependency

## [0.5.27] - 2022-09-20

* server: Fix ServerBuilder::configure_async() helper method

## [0.5.26] - 2022-09-20

* server: Add ServerBuilder::configure_async() helper, async version of configure method

* web: Fix incorrect wordin for State extractor #134

## [0.5.25] - 2022-08-22

* http: Fix http2 content-length handling

* http: Fix parsing ambiguity in Transfer-Encoding and Content-Length headers for HTTP/1.0 requests

## [0.5.24] - 2022-07-14

* ws: Do not encode pong into binary message (#130)

## [0.5.23] - 2022-07-13

* http: Use new h2 client api

## [0.5.22] - 2022-07-12

* http: Handle h2 connection disconnect

## [0.5.21] - 2022-07-07

* http: fix h2 client, send scheme and authority

## [0.5.20] - 2022-06-27

* http: replace h2 crate with ntex-h2

## [0.5.19] - 2022-06-23

* connect: move to separate crate

* http: move basic types to separeate crate

## [0.5.18] - 2022-06-03

* http: Refactor client pool management

* http: Add client response body load timeout

## [0.5.17] - 2022-05-05

* http: Fix handling for zero slow-request timeout

## [0.5.16] - 2022-04-05

* ws: Add keep-alive timeout support to websockets client

* web: Disable keep-alive timeout for websockets endpoint

## [0.5.15] - 2022-02-18

* web: Fix unsupported web ws handling

## [0.5.14] - 2022-01-30

* Update ntex-io to 0.1.7

## [0.5.13] - 2022-01-28

* http: Refactor client pool support for http/2 connections

## [0.5.12] - 2022-01-27

* Replace derive_more with thiserror

## [0.5.11] - 2022-01-23

* web: Refactor ws support

* web: Rename data to state

* web: Add types::Payload::recv() and types::Payload::poll_recv() methods

## [0.5.10] - 2022-01-17

* rt: Add glommio runtime support

* http: Use Io::take() method for http/1 dispatcher

* http: Add Payload::recv() and Payload::poll_recv() methods

## [0.5.9] - 2022-01-12

* Update ws::WsTransport

## [0.5.8] - 2022-01-10

* Remove usage of ntex::io::Boxed types

* Remove unneeded re-exports

## [0.5.7] - 2022-01-05

* http: fix rustls feature

## [0.5.6] - 2022-01-03

* web: Restore `App::finish()` method

## [0.5.5] - 2022-01-03

* Disable default runtime selection

* Move ntex::util services to ntex-util

## [0.5.4] - 2022-01-02

* http1: Unregister keep-alive timer after request is received

* web: Add option to use default `AppConfig` for App type factory

## [0.5.3] - 2021-12-31

* Fix WsTransport shutdown, send close frame

## [0.5.2] - 2021-12-30

* Introduce new WsTransport implementation

## [0.5.1] - 2021-12-30

* Drop WsTransport

## [0.5.0] - 2021-12-30

* Upgrade to ntex-io 0.1

* Updrade to cookie 0.16

## [0.5.0-b.7] - 2021-12-30

* Update ntex-io to 0.1.0-b.10

## [0.5.0-b.6] - 2021-12-29

* Add `async-std` support

## [0.5.0-b.5] - 2021-12-28

* http: proper send payload, if request payload is not consumed

* ws: Fix handling for ws transport nested errors

## [0.5.0-b.4] - 2021-12-26

* Allow to get access to ws transport codec

* Move http::client::handshake to ws::handshake

## [0.5.0-b.3] - 2021-12-24

* Use new ntex-service traits

* Remove websocket support from http::client

* Add standalone ws::client

* Add websockets transport (io filter)

## [0.5.0-b.2] - 2021-12-22

* Refactor write back-pressure for http1

## [0.5.0-b.1] - 2021-12-20

* Refactor http/1 dispatcher

* Refactor Server service configuration

## [0.5.0-b.0] - 2021-12-19

* Migrate io to ntex-io

* Move ntex::time to ntex-util crate

* Replace mio with polling for accept loop

## [0.4.13] - 2021-12-07

* server: Rename .apply/.apply_async to .on_worker_start()

## [0.4.12] - 2021-12-06

* http: Use memory pools

## [0.4.11] - 2021-12-02

* framed: Use memory pools

## [0.4.10] - 2021-11-29

* Fix potential overflow sub in timer wheel

## [0.4.9] - 2021-11-20

* Update rustls to 0.20
* Update webpki to 0.22
* Update webpki-roots to 0.22
* Update tokio-rustls to 0.23
* Adapt code for rustls breaking changes

## [0.4.8] - 2021-11-08

* Add Clone impl for connect::ConnectError

## [0.4.7] - 2021-11-02

* h1: allow to override connection type in on-request handler

## [0.4.6] - 2021-10-29

* time: fix wheel time calculations

## [0.4.5] - 2021-10-20

* framed: Do not poll service for readiness if it failed before

## [0.4.4] - 2021-10-13

* Use wrapping_add for usize

* Better handling ws control frames

## [0.4.3] - 2021-10-06

* Do not modify lowres time outside of driver task

## [0.4.2] - 2021-10-06

* Update to nanorand 0.6.1

## [0.4.1] - 2021-09-27

* server: Send `ServerStatus::WorkerFailed` update if worker is failed

* server: Make `ServerBuilder::status_handler()` public

* framed: Read::resume() returns true if it was paused before

* http::client: Do not add content-length header for empty body #56

## [0.4.0] - 2021-09-17

* Refactor web middlewares/filters registration and management

* Use fxhash instead of ahash

## [0.4.0-b.13] - 2021-09-12

* Fix update timer wheel bucket calculation

## [0.4.0-b.12] - 2021-09-07

* Fix race in low res timer

## [0.4.0-b.11] - 2021-09-01

* Decrease lowres timer resolution to 5ms

## [0.4.0-b.10] - 2021-09-01

* Fix lowres timer restart

## [0.4.0-b.9] - 2021-09-01

* More timer wheel cleanups on driver drop

## [0.4.0-b.8] - 2021-09-01

* Add `ntex::time::now()` helper, returns low res time.

* Add `ntex::time::system_time()` helper, returns low res system time.

* Removed `LowResTime` and `SystemTime` services

## [0.4.0-b.7] - 2021-08-31

* Remove From<u64> for Millis impl

## [0.4.0-b.6] - 2021-08-30

* More timer wheel cleanups on driver drop

## [0.4.0-b.5] - 2021-08-28

* Cleanup timer wheel on driver drop

## [0.4.0-b.4] - 2021-08-28

* Reduce timer resolution

## [0.4.0-b.3] - 2021-08-27

* Add timer service

* Add helper time types Millis and Seconds

* Add sleep, interval, timeout helpers

* Use ntex-rt 0.3

* Use ntex-service 0.2

## [0.4.0-b.2] - 2021-08-14

* potential HTTP request smuggling vulnerabilities

## [0.4.0-b.1] - 2021-06-27

* use ntex-bytes instead of bytes

* drop direct tokio dependency

* rustls connector - fix rustls connect to work around a port in hostname (giving invalid DNS) #50

## [0.3.18] - 2021-06-03

* server: expose server status change notifications

## [0.3.17] - 2021-05-24

* framed: add read/write bytes pool

## [0.3.16] - 2021-05-17

* framed: process unhandled data on disconnect

* add "http-framework" feature

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
