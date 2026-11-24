//! Review F3 (docs/reviews/display.md). This file holds one test, because the
//! test lowers the descriptor limit of its whole process.
mod common;
use common::*;
use std::fs::File;
use std::os::unix::net::{UnixListener, UnixStream};
use vmlab_hv_display::{Capabilities as C, Message as M, protocol, receive, send};

/// A descriptor shortage at the moment a client connects is temporary. The
/// accept loop must still serve the next client when the shortage is over.
#[test]
fn display_survives_descriptor_exhaustion() {
    let (server, ..) = server();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("display.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let serving = server.clone();
    let _accept = std::thread::spawn(move || serving.serve(listener, false));

    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a valid rlimit for both calls.
    unsafe {
        assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit), 0);
        limit.rlim_cur = limit.rlim_cur.min(256);
        assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &limit), 0);
    }
    // Take every free descriptor, then give one back for the client side of
    // one connection. The accept of that connection has no descriptor left.
    let null = File::open("/dev/null").unwrap();
    let mut fillers = Vec::new();
    loop {
        match null.try_clone() {
            Ok(file) => fillers.push(file),
            Err(error) => {
                assert_eq!(error.raw_os_error(), Some(libc::EMFILE));
                break;
            }
        }
    }
    drop(fillers.pop());
    // The accept loop polls the listener, and the kernel reserves a
    // descriptor for each `accept` before it looks for a connection. A poll
    // at this moment holds the one free descriptor for an instant, and the
    // client's `socket` gets EMFILE. That is the same shortage: try again.
    let starved = loop {
        match UnixStream::connect(&path) {
            Ok(stream) => break stream,
            Err(error) => {
                assert_eq!(error.raw_os_error(), Some(libc::EMFILE));
                std::thread::yield_now();
            }
        }
    };
    while server.accept_failures() == 0 {
        std::thread::yield_now();
    }
    drop(fillers);
    drop(starved);

    let stream = UnixStream::connect(&path).expect("the listener is gone");
    send(
        &stream,
        &M::Hello {
            version: protocol::VERSION,
            capabilities: C(C::INPUT),
        },
        &[],
    )
    .unwrap();
    let reply = receive(&stream).expect("no Hello reply after the shortage");
    assert!(matches!(reply.message, M::Hello { .. }));
    server.shutdown();
}
