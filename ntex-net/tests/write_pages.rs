//! Inline page storage must stay valid through vectored and partial writes.

use std::{io::Read, net, thread, time::Duration};

use ntex_bytes::Bytes;
use ntex_io::IoConfig;
use ntex_service::cfg::SharedCfg;

#[ntex::test]
async fn inline_pages_survive_writes() {
    const SMALL: usize = 7;
    const BATCH: usize = 256;

    for total in [SMALL, SMALL * 17, 256 * 1024] {
        let listener = net::TcpListener::bind("127.0.0.1:0").unwrap();
        socket2::SockRef::from(&listener)
            .set_recv_buffer_size(8 * 1024)
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let peer = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            thread::sleep(Duration::from_millis(100));
            let mut received = vec![0; total];
            socket.read_exact(&mut received).unwrap();
            received
        });

        let socket = net::TcpStream::connect(addr).unwrap();
        socket2::SockRef::from(&socket)
            .set_send_buffer_size(4096)
            .unwrap();
        let cfg = SharedCfg::new("TEST")
            .add(IoConfig::new().set_write_buf_threshold(64 * 1024))
            .build();
        let io = ntex_net::from_tcp_stream(socket, cfg).unwrap();
        let expected: Vec<u8> = (0..total)
            .map(|index| u8::try_from(index % 251).unwrap())
            .collect();

        ntex::time::timeout(Duration::from_secs(30), async {
            for batch in expected.chunks(SMALL * BATCH) {
                for chunk in batch.chunks(SMALL) {
                    let bytes = Bytes::copy_from_slice(chunk);
                    assert!(bytes.is_inline());
                    io.encode_bytes(bytes).unwrap();
                }
                io.flush(false).await.unwrap();
            }
            io.flush(true).await.unwrap();
            io.shutdown().await.unwrap();
        })
        .await
        .expect("writer did not finish");

        let received = peer.join().unwrap();
        assert_eq!(
            received.iter().zip(&expected).position(|(a, b)| a != b),
            None,
            "inline page data changed, total={total}"
        );
    }
}
