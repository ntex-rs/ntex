# Changes

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
