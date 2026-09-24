//! Regression test for filter output produced while a read is processed.
//!
//! A filter may write while it processes input, a TLS server answering a
//! ClientHello for example. Once the output reaches the write buffer threshold
//! ntex-io sends it right away, from within the read. On the IOCP backend a
//! recv that completed immediately used to run the read filters while the
//! stream storage was taken, so that write panicked.

use std::{io::Read, io::Write, net, thread, time::Duration};

use ntex::codec::BytesCodec;
use ntex_io::{FilterBuf, FilterLayer};
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
