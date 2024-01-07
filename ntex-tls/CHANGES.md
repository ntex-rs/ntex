# Changes

## [1.0.0-b.0] - 2024-01-07

* Use "async fn" in trait for Service definition

## [0.3.3] - 2023-11-12

* Attempt to fix #190

## [0.3.2] - 2023-11-03

* Improve implementation

## [0.3.1] - 2023-09-11

* Add missing fmt::Debug impls

## [0.3.0] - 2023-06-22

* Release v0.3.0

## [0.3.0-beta.0] - 2023-06-16

* Migrate to ntex-service 1.2

## [0.2.4] - 2023-01-29

* Update buffer api

## [0.2.3] - 2023-01-25

* Fix double buf cleanup

## [0.2.2] - 2023-01-24

* Update ntex-io to 0.2.2

## [0.2.1] - 2023-01-23

* Update filter implementation

## [0.2.0] - 2023-01-04

* Release

## [0.2.0-beta.0] - 2022-12-28

* Migrate to ntex-service 1.0

## [0.1.7] - 2022-10-26

* Create the correct PskIdentity type on query #138

## [0.1.6] - 2022-10-14

* Allow extracting TLS SNI server name and TLS PSK identity #136

## [0.1.5] - 2022-02-19

* Fix rustls hangs during handshake #103

* Move HttpProtocol to ntex-io

## [0.1.4] - 2022-02-11

* Do not use SslRef::is_init_finished() method for openssl

## [0.1.3] - 2022-01-30

* Add PeerCert and PeerCertChain for rustls

* Update to ntex-io 0.1.7

## [0.1.2] - 2022-01-12

* Update Filter trait usage

## [0.1.1] - 2022-01-10

* Remove usage of ntex::io::Boxed types

## [0.1.0] - 2021-12-30

* Upgrade to ntex-io 0.1

## [0.1.0-b.5] - 2021-12-28

* Proper handling for openssl ZERO_RETURN error

## [0.1.0-b.5] - 2021-12-28

* Add query support for peer cert and peer cert chain

## [0.1.0-b.4] - 2021-12-27

* Upgrade no ntex 0.5-b.4

## [0.1.0-b.3] - 2021-12-23

* Add impl openssl::Acceptor::from(SslAcceptor)

* Add openssl::Acceptor::seal() helper

## [0.1.0-b.1] - 2021-12-20

* Update ntex-io

## [0.1.0-b.0] - 2021-12-19

* Initial impl
