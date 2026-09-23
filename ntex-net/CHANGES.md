# Changes

## [4.1.0] - 2026-09-23

* Treat peer half-close as read eof in the io-uring backend, as the polling
  backend does. `POLLRDHUP` terminated the connection, so a response to a
  peer that half-closed after its request was dropped and the peer saw a
  clean close. The disconnect poll is removed, disconnects are detected by
  reads and writes

* Reset read cancel state when a canceled `Recv` completes normally in the
  io-uring backend, the stale state disabled later read pauses

* Cancel in-flight operations of a stream before closing it in the io-uring
  backend. `Close` only removes the descriptor from the file table, pending
  operations kept the socket open, so a
  force-closed or dropped connection was neither reset nor closed until the
  peer went away

* Fix socket leak on runtime shutdown in the io-uring backend. Operations
  queued during the last turn are submitted and all in-flight operations are
  canceled before cleanup, then every socket still owned by the backend is
  closed, which also breaks the `IoContext` reference cycle

* Return the pages a failed write took to the write buffer in the polling and
  tokio backends, they were dropped and stayed counted as in-flight output

* Arm write interest from the out-of-band write path in the polling backend
  instead of leaving it to the write task, which retried the write only to
  have it block again

* Treat `EPOLLERR` as terminal in the polling backend, it is reported whether
  or not it was requested and re-arming on it makes no progress

* Fix polling reactor cleanup leaking sockets whose primary handle was dropped
  but whose secondary drop had not yet been processed

* Reset the connection on a force close instead of closing it gracefully, the
  receive queue drain and SHUT_RDWR turned a truncated response into a clean
  FIN that a peer could not tell apart from a complete one

* Drop poll interest before tearing a connection down in the polling backend,
  and drain its receive queue on the reactor thread instead of the blocking
  pool, the socket is still registered so the drain raced the reactor

* Read into the io context buffer in place in the tokio and polling backends,
  they read synchronously so they no longer need a detached buffer

* Discard the socket receive queue before closing a connection, closing a
  socket with unread input aborts it with an RST and loses the output that the
  graceful shutdown just drained. The io-uring backend leaves this to a recv
  that is already in flight rather than racing it

* Report the number of bytes written to the peer from every backend, so that
  output a completion based backend still owns is accounted for as outstanding

* Return output that did not reach the peer back to the write buffer in the
  io-uring backend, a partial send silently dropped the rest of the page

* Close both directions of the connection at the end of a graceful shutdown in
  the tokio and compio backends, previously they only shut down the write
  direction

* Fix compio backend hang, the io task could stop without reporting completion
  to the io context if the write buffer was empty

* Drop unreachable IoContext::shutdown() step from io task shutdown

* Support eager writes with the IOCP backend

* Update IoContext::take_read_buf()/release_read_buf() api usage

* Update IoContext::update_write_status() api usage

* Produce io::ErrorKind::WriteZero for backend impl

## [4.0.1] - 2026-09-18

* Api docs improvements

## [4.0.0] - 2026-09-14

* Migrate to ntex-service 5

## [3.15.0] - 2026-08-09

* Do not unwind reactor panics, forward handling to arbiter

## [3.14.1] - 2026-08-06

* IOCP driver cleanups

## [3.14.0] - 2026-08-03

* Add windows IOCP driver (neon-iocp)

## [3.13.1] - 2026-07-17

* io-ring driver could use invalid BytePage pointer #932

## [3.13.0] - 2026-06-21

* Cleanup io-uring requests before drop

* polling (neon) driver: do not panic with "called `Option::unwrap()` on a `None`
  value" when a filter emits a large (>= write_buf_threshold) write burst during
  read processing; the reentrant write is now deferred until the streams slab is
  released instead of re-taking it

## [3.12.0] - 2026-05-15

* Update ntex-io

* Fix readiness checks in tokio support

## [3.11.0] - 2026-05-08

* Simplify tokio integration impl

* Optimize neon(polling) impl

## [3.10.0] - 2026-05-03

* Enable vectored writes for tokio,compio,neon runtime

## [3.9.1] - 2026-04-07

* Update tokio compat impl

## [3.9.0] - 2026-04-02

* Update to ntex-error 2.0

## [3.8.0] - 2026-03-08

* Use ntex_error::Error for connect service

## [3.7.0] - 2026-02-16

* SharedCfg is not Copy

## [3.6.3] - 2026-02-12

* Fix compio io shutdown

## [3.6.2] - 2026-02-09

* Fix windows compilation

## [3.6.0] - 2026-02-01

* Update compio to 0.18

## [3.5.2] - 2026-01-29

* Fix socket close process for polling driver

## [3.5.1] - 2026-01-08

* Remove changes queue checks for io-uring driver

## [3.5.0] - 2026-01-03

* Refactor io driver

* Move tokio impl from ntex-tokio

* Move compio impl from ntex-compio

## [3.4.1] - 2025-12-18

* Cleanup and improvements for polling driver

## [3.4.0] - 2025-12-17

* Upgrade to ntex-service v4

## [3.3.1] - 2025-12-16

* Refactor neon and neon-uring drivers

## [3.3.0] - 2025-12-15

* Drop all sockets on driver drop for neon driver

## [3.2.0] - 2025-12-08

* Enable unix_connect for compio on windows

## [3.1.0] - 2025-12-05

* Remove useless service Resolver

## [3.0.0-pre.3] - 2025-12-01

* Export missing types

## [3.0.0-pre.2] - 2025-11-30

* Do not swallow error from rt-polling/rt-uring drivers

## [3.0.0-pre.0] - 2025-11-27

* New io configuration subsystem

## [2.9.0] - 2025-11-12

* Update to compio 0.16

## [2.8.1] - 2025-09-24

* Omitimize neon polling driver

## [2.8.0] - 2025-08-15

* Fix io disconnect handling for io-uring driver

## [2.7.0] - 2025-07-09

* Add timeout support to connector

## [2.6.0] - 2025-06-25

* Upgrade to ntex-compio 0.4

## [2.5.28] - 2025-06-21

* Check for potential date race

## [2.5.27] - 2025-06-11

* Use new io-uring opcode api

## [2.5.26] - 2025-06-09

* Use optimized io-uring submission api

* Use optimized io-uring opcodes

## [2.5.25] - 2025-05-29

* Use inline api for iour

## [2.5.22] - 2025-05-27

* Check io-uring compat

## [2.5.21] - 2025-05-26

* Upgrade to ntex-compio 0.3

## [2.5.20] - 2025-05-19

* iour: Handle POLLRDHUP event

* iour: Do not use zc send for small buffers

## [2.5.19] - 2025-05-15

* Handle uring Close op operation

## [2.5.18] - 2025-05-14

* iour: Use opcode::SendZc for send op

## [2.5.13] - 2025-04-08

* Cleanup io-urign driver

## [2.5.12] - 2025-04-07

* Fix leak in poll driver

## [2.5.11] - 2025-04-05

* Various improvements for polling driver

## [2.5.10] - 2025-03-28

* Better closed sockets handling

## [2.5.9] - 2025-03-27

* Handle closed sockets

## [2.5.8] - 2025-03-25

* Update neon runtime

## [2.5.7] - 2025-03-21

* Simplify neon poll impl

## [2.5.6] - 2025-03-20

* Redesign neon poll support

## [2.5.5] - 2025-03-17

* Add check for required io-uring opcodes

* Handle io-uring cancelation

## [2.5.4] - 2025-03-15

* Close FD in various case for poll driver

## [2.5.3] - 2025-03-14

* Fix operation cancelation handling for poll driver

## [2.5.2] - 2025-03-14

* Fix operation cancelation handling for io-uring driver

## [2.5.1] - 2025-03-14

* Fix socket connect for io-uring driver

## [2.5.0] - 2025-03-12

* Add neon runtime support

* Drop glommio support

* Drop async-std support

## [2.4.0] - 2024-09-25

* Update to glommio v0.9

## [2.3.0] - 2024-09-24

* Update to compio v0.12

## [2.1.0] - 2024-08-29

* Add `compio` runtime support

## [2.0.0] - 2024-05-28

* Use async fn for Service::ready() and Service::shutdown()

## [1.0.2] - 2024-03-30

* Fix glommio compat feature #327

## [1.0.1] - 2024-03-29

* Add Connect::map_addr() helper method

* Add `Address` support for ByteString

## [1.0.0] - 2024-03-25

* Move to separate crate
