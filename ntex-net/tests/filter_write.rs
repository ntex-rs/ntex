//! Regression tests for filter output.
//!
//! A filter may write while it processes input, a TLS server answering a
//! ClientHello for example. Once the output reaches the write buffer threshold
//! ntex-io sends it right away, from within the read. On the IOCP backend a
//! recv that completed immediately used to run the read filters while the
//! stream storage was taken, so that write panicked.
//!
//! Output a filter produced before it failed is delivered before the
//! connection closes.

use std::{io::Read, io::Write, net, thread, time::Duration};

use ntex::codec::BytesCodec;
use ntex_io::{FilterBuf, FilterLayer, IoConfig};
use ntex_service::cfg::SharedCfg;

const REPLY: usize = 16 * 1024;

/// Passes input through and answers each input with a reply larger than the
/// default write buffer threshold.
#[derive(Debug)]
struct Reply;

impl FilterLayer for Reply {
    fn process_read_buf(&self, buf: &FilterBuf<'_>) -> std::io::Result<()> {
        let got = buf.with_read_buffers(|src, dst| {
            if let Some(s) = src.take() {
                dst.extend_from_slice(&s);
                true
            } else {
                false
            }
        });
        if got {
            buf.with_write_buffers(|_, dst| dst.extend_from_slice(&[7u8; REPLY]));
        }
        Ok(())
    }

    fn process_write_buf(&self, buf: &FilterBuf<'_>) -> std::io::Result<()> {
        buf.with_write_buffers(|src, dst| {
            while let Some(p) = src.take() {
                dst.append(p);
            }
        });
        Ok(())
    }
}

#[ntex::test]
async fn filter_writes_while_processing_immediate_read() {
    let lst = net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut peer = net::TcpStream::connect(lst.local_addr().unwrap()).unwrap();
    let (srv, _) = lst.accept().unwrap();
    peer.write_all(b"hello").unwrap();
    // queued before the first recv is issued, so that recv completes at once
    thread::sleep(Duration::from_millis(100));

    let io = ntex_net::from_tcp_stream(srv, SharedCfg::default())
        .unwrap()
        .add_filter(Reply);
    let reader = thread::spawn(move || {
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = vec![0u8; REPLY];
        peer.read_exact(&mut buf).map(|()| buf)
    });

    let msg = ntex::time::timeout(ntex::time::Seconds(5), io.recv(&BytesCodec))
        .await
        .expect("input not delivered")
        .unwrap()
        .unwrap();
    assert_eq!(&msg[..], b"hello");

    let reply = ntex::rt::spawn_blocking(move || reader.join().unwrap())
        .await
        .unwrap()
        .expect("reply not delivered");
    assert!(reply.iter().all(|b| *b == 7));
}

/// Moves output to the transport, then fails.
#[derive(Debug)]
struct FailAfterWrite;

impl FilterLayer for FailAfterWrite {
    fn process_read_buf(&self, _: &FilterBuf<'_>) -> std::io::Result<()> {
        Ok(())
    }

    fn process_write_buf(&self, buf: &FilterBuf<'_>) -> std::io::Result<()> {
        buf.with_write_buffers(|src, dst| {
            while let Some(p) = src.take() {
                dst.append(p);
            }
        });
        Err(std::io::Error::other("filter failed"))
    }
}

#[ntex::test]
async fn failed_filter_output_is_delivered() {
    // more than the socket buffers hold, the output is delivered over several
    // writes after the failure
    const SIZE: usize = 8 * 1024 * 1024;

    // threshold 1 writes from within `encode_slice`, the large one from the
    // write task
    for threshold in [1, 1 << 30] {
        let lst = net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut peer = net::TcpStream::connect(lst.local_addr().unwrap()).unwrap();
        let (srv, _) = lst.accept().unwrap();

        let cfg = SharedCfg::new("FAIL")
            .add(IoConfig::new().set_write_buf_threshold(threshold))
            .build();
        let io = ntex_net::from_tcp_stream(srv, cfg).unwrap();

        // the write path is running when the filter fails
        io.encode_slice(b"prime").unwrap();
        ntex::time::timeout(ntex::time::Seconds(5), io.flush(true))
            .await
            .unwrap()
            .unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        peer.read_exact(&mut [0u8; 5]).unwrap();

        let reader = thread::spawn(move || {
            let mut buf = Vec::new();
            peer.read_to_end(&mut buf).map(|_| buf)
        });

        let io = io.add_filter(FailAfterWrite);
        let _ = io.encode_slice(&vec![9u8; SIZE]);
        let err = ntex::time::timeout(ntex::time::Seconds(5), io.shutdown())
            .await
            .expect("connection not closed")
            .unwrap_err();
        assert_eq!(err.to_string(), "filter failed");

        let res = ntex::rt::spawn_blocking(move || reader.join().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            res.len(),
            SIZE,
            "threshold {threshold}: output of a failed filter is not delivered"
        );
        assert!(res.iter().all(|b| *b == 9));
        drop(io);
    }
}
