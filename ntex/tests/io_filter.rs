//! Io filter integration tests.
use std::io::{Read, Write};
use std::{cell::Cell, io, net, time::Duration};

use ntex::codec::BytesCodec;
use ntex::io::{FilterBuf, FilterLayer, Io};
use ntex::server::test_server;
use ntex::service::fn_service;
use ntex::util::Bytes;

/// Must be greater than or equal to `IoConfig::write_buf_threshold`
/// (8192 by default), so that the io stream initiates a direct
/// (in-place) write while the read buffer is being processed.
const BURST_SIZE: usize = 16 * 1024;

/// Filter that emits a large burst of write data while processing
/// incoming read data, similar to a TLS filter that writes handshake
/// or control data in response to incoming records.
#[derive(Debug, Default)]
struct BurstWriteFilter {
    sent: Cell<bool>,
}

impl FilterLayer for BurstWriteFilter {
    fn process_read_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
        // pass incoming data through
        let got_data = buf.with_read_buffers(|src, dst| {
            if let Some(src) = src.take() {
                dst.extend_from_slice(&src);
                !src.is_empty()
            } else {
                false
            }
        });

        // the first incoming data triggers a large write
        if got_data && !self.sent.get() {
            self.sent.set(true);
            buf.with_write_buffers(|_, dst| {
                dst.extend_from_slice(&[b'x'; BURST_SIZE]);
            });
        }
        Ok(())
    }

    fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
        // pass outgoing data through
        buf.with_write_buffers(|src, dst| {
            if !src.is_empty() {
                src.move_to(dst);
            }
        });
        Ok(())
    }
}

/// A filter that produces a write burst (>= `write_buf_threshold`)
/// during read-buffer processing must not break the io stream.
///
/// Regression test for the polling (neon) backend: the driver used to
/// panic on reentrant slab access ("called `Option::unwrap()` on a
/// `None` value") when a filter generated enough write data during
/// read processing to trigger a direct write.
#[ntex::test]
async fn test_filter_large_write_during_read_processing() {
    let srv = test_server(async || {
        fn_service(|io: Io| async move {
            let io = io.add_filter(BurstWriteFilter::default());
            // notify the client that the filter is installed
            io.send(Bytes::from_static(b"hi"), &BytesCodec)
                .await
                .unwrap();
            // echo service
            while let Ok(Some(msg)) = io.recv(&BytesCodec).await {
                io.send(msg, &BytesCodec).await.unwrap();
            }
            Ok::<_, io::Error>(())
        })
    });

    let mut client = net::TcpStream::connect(srv.addr()).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    // wait for the server-side filter to be installed
    let mut greeting = [0u8; 2];
    client.read_exact(&mut greeting).unwrap();
    assert_eq!(&greeting, b"hi");

    // this write triggers the filter burst during read processing
    // on the server side
    client.write_all(b"ping").unwrap();

    // burst must arrive first (it is written during read processing),
    // followed by the echoed message
    let mut data = vec![0u8; BURST_SIZE + 4];
    client
        .read_exact(&mut data)
        .expect("server did not send data, io driver is broken");

    assert!(data[..BURST_SIZE].iter().all(|b| *b == b'x'));
    assert_eq!(&data[BURST_SIZE..], b"ping");
}
