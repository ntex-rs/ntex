//! OpenSSL allocates through the Rust global allocator after `use_global_allocator()`.
//!
//! The allocator must be installed before OpenSSL allocates anything, so this
//! binary contains a single test.
#![cfg(feature = "openssl")]
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use ntex::codec::BytesCodec;
use ntex_bytes::Bytes;
use ntex_io::{Io, testing::IoTest};
use ntex_service::{Pipeline, cfg::SharedCfg};
use ntex_tls::openssl::{SslAcceptor, connect, use_global_allocator};
use ntex_util::future::join;
use tls_openssl::{pkey::PKey, ssl, x509::X509};

const CERT: &[u8] = include_bytes!("../examples/cert.pem");
const KEY: &[u8] = include_bytes!("../examples/key.pem");

thread_local! {
    static LAST: Cell<usize> = const { Cell::new(0) };
}

/// Records the size of the last allocation on the current thread
struct Recording;

unsafe impl GlobalAlloc for Recording {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = LAST.try_with(|last| last.set(layout.size()));
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Recording = Recording;

#[ntex::test]
async fn openssl_uses_global_allocator() {
    assert!(use_global_allocator());

    // the block carries a 16 byte header
    LAST.set(0);
    let p = unsafe { openssl_sys::CRYPTO_malloc(1000, c"test".as_ptr(), 0) };
    assert!(!p.is_null());
    assert_eq!(LAST.get(), 1016);
    unsafe { openssl_sys::CRYPTO_free(p, c"test".as_ptr(), 0) };

    let mut acceptor = ssl::SslAcceptor::mozilla_intermediate(ssl::SslMethod::tls()).unwrap();
    acceptor
        .set_private_key(&PKey::private_key_from_pem(KEY).unwrap())
        .unwrap();
    acceptor
        .set_certificate(&X509::from_pem(CERT).unwrap())
        .unwrap();
    let acceptor = Pipeline::new((), SslAcceptor::new(acceptor.build()));
    let mut connector = ssl::SslConnector::builder(ssl::SslMethod::tls()).unwrap();
    connector.set_verify(ssl::SslVerifyMode::NONE);
    let ssl = connector
        .build()
        .configure()
        .unwrap()
        .into_ssl("localhost")
        .unwrap();

    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1 << 20);
    server.remote_buffer_cap(1 << 20);
    let (client, server) = join(
        connect(Io::new(client, SharedCfg::new("CLI")), ssl),
        acceptor.call(Io::new(server, SharedCfg::new("SRV"))),
    )
    .await;
    let (client, server) = (client.unwrap(), server.unwrap());

    // the connection allocates and releases its record buffers
    let data = Bytes::from(vec![b'a'; 256 * 1024]);
    for (tx, rx) in [(&client, &server), (&server, &client)] {
        tx.send(data.clone(), &BytesCodec).await.unwrap();
        let mut received = 0;
        while received < data.len() {
            received += rx.recv(&BytesCodec).await.unwrap().unwrap().len();
        }
    }

    let (res, ()) = join(client.shutdown(), async {
        assert!(server.recv(&BytesCodec).await.unwrap().is_none());
    })
    .await;
    res.unwrap();
}
