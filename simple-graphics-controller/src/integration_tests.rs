//! End-to-end tests: the real listener + policy engine, speaking the actual
//! wire protocol (framing + SCM_RIGHTS fd passing) over an abstract socket.
//! A `/dev/null` fd stands in for `/dev/fb0` since CI hosts lack fbdev.

use std::{
    io::ErrorKind,
    os::{
        fd::{FromRawFd, OwnedFd, RawFd},
        linux::net::SocketAddrExt,
        unix::net::{SocketAddr as StdSocketAddr, UnixListener as StdUnixListener},
    },
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use dashmap::DashMap;
use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
use sendfd::RecvWithFd;
use simple_graphics_protocol::{
    ClientRequest, FRAME_HEADER_LEN, Resource, DisplayResource, ServerMessage, deserialize,
    parse_frame_header, serialize_framed,
};
use tokio::{
    io::AsyncWriteExt,
    net::{UnixListener, UnixStream, unix::SocketAddr as TokioSocketAddr},
};

use crate::{
    client_handler::handle_connection,
    types::{ClientId, ResourceRegistry},
    windowing::{Policy, PolicyEngine, REVOKE_TIMEOUT},
};

/// One real protocol client: framed messages, fds collected via recvmsg.
struct TestClient {
    stream: UnixStream,
    fds: Vec<RawFd>,
}

impl TestClient {
    async fn connect(name: &'static [u8]) -> Self {
        let std_addr = StdSocketAddr::from_abstract_name(name).expect("invalid socket address");
        let tokio_addr: TokioSocketAddr = TokioSocketAddr::from(std_addr);
        let stream = UnixStream::connect_addr(&tokio_addr)
            .await
            .expect("failed to connect");
        Self {
            stream,
            fds: Vec::new(),
        }
    }

    async fn send(&mut self, msg: &ClientRequest) {
        let data = serialize_framed(msg).expect("serialize");
        self.stream.write_all(&data).await.expect("write");
    }

    /// Read one framed server message. Any SCM_RIGHTS fds are appended to
    /// `self.fds` (ownership transfers to this process).
    async fn recv(&mut self) -> ServerMessage {
        let mut msg_fds: Vec<RawFd> = Vec::new();
        let mut fd_buf = [0i32; 16];

        let mut header = [0u8; FRAME_HEADER_LEN];
        let mut got = 0;
        while got < FRAME_HEADER_LEN {
            self.stream.readable().await.expect("readable");
            match self.stream.recv_with_fd(&mut header[got..], &mut fd_buf) {
                Ok((0, _)) => panic!("server closed connection"),
                Ok((n, nfds)) => {
                    got += n;
                    msg_fds.extend_from_slice(&fd_buf[..nfds]);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => continue,
                Err(e) => panic!("recv failed: {e}"),
            }
        }

        let len = parse_frame_header(&header).expect("frame header");
        let mut payload = vec![0u8; len];
        let mut got = 0;
        while got < len {
            self.stream.readable().await.expect("readable");
            match self.stream.recv_with_fd(&mut payload[got..], &mut fd_buf) {
                Ok((0, _)) => panic!("server closed mid-frame"),
                Ok((n, nfds)) => {
                    got += n;
                    msg_fds.extend_from_slice(&fd_buf[..nfds]);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => continue,
                Err(e) => panic!("recv failed: {e}"),
            }
        }

        let msg = deserialize(&payload).expect("deserialize");
        self.fds.extend(msg_fds);
        msg
    }

    async fn expect_advertise(&mut self) -> Vec<Resource> {
        match self.recv().await {
            ServerMessage::Advertise {
                available_resources,
            } => available_resources,
            other => panic!("expected Advertise, got {other:?}"),
        }
    }

    /// Expect a Grant; returns the fds that arrived with it.
    async fn expect_grant(&mut self) -> Vec<RawFd> {
        let before = self.fds.len();
        let msg = self.recv().await;
        assert!(
            matches!(msg, ServerMessage::Grant { .. }),
            "expected Grant, got {msg:?}"
        );
        let fds = self.fds[before..].to_vec();
        assert!(!fds.is_empty(), "Grant must carry fds");
        fds
    }

    async fn expect_revoke(&mut self) {
        let msg = self.recv().await;
        assert!(
            matches!(msg, ServerMessage::Revoke { .. }),
            "expected Revoke, got {msg:?}"
        );
    }

    async fn expect_deny(&mut self) -> String {
        match self.recv().await {
            ServerMessage::Deny { reason } => reason,
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    /// Assert nothing arrives within `dur` (e.g. a queued client).
    async fn expect_silence(&mut self, dur: Duration) {
        match tokio::time::timeout(dur, self.recv()).await {
            Err(_) => {}
            Ok(msg) => panic!("expected silence, got {msg:?}"),
        }
    }
}

impl Drop for TestClient {
    fn drop(&mut self) {
        for &raw_fd in &self.fds {
            // Safety: received via SCM_RIGHTS, ownership is this process's.
            unsafe {
                drop(OwnedFd::from_raw_fd(raw_fd));
            }
        }
    }
}

/// Spawn the real server (listener + engine) on a test-only abstract socket.
async fn spawn_server(name: &'static [u8], policy: Policy) {
    static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);

    // A /dev/null fd stands in for the fbdev fd (hosts lack /dev/fb0).
    let resource_reg: ResourceRegistry = Arc::new(DashMap::new());
    let file = std::fs::File::open("/dev/null").expect("/dev/null");
    resource_reg.insert(Resource::Display(DisplayResource::Fbdev), file.into());
    let advertised = Arc::new(vec![Resource::Display(DisplayResource::Fbdev)]);

    let engine = PolicyEngine::spawn(std::collections::HashMap::from([(Resource::Display(DisplayResource::Fbdev), policy)]));

    let addr = StdSocketAddr::from_abstract_name(name).expect("invalid abstract address");
    let std_listener = StdUnixListener::bind_addr(&addr).expect("bind");
    std_listener.set_nonblocking(true).expect("nonblocking");
    let listener = UnixListener::from_std(std_listener).expect("wrap listener");

    tokio::spawn(async move {
        loop {
            let (stream, _addr) = match listener.accept().await {
                Ok(x) => x,
                Err(e) => {
                    eprintln!("accept error: {e}");
                    break;
                }
            };
            let creds = getsockopt(&stream, PeerCredentials).expect("peer credentials");
            let client_id = ClientId::new(NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed));
            let engine = engine.clone();
            let resource_reg = resource_reg.clone();
            let advertised = advertised.clone();
            tokio::spawn(async move {
                if let Err(e) =
                    handle_connection(stream, client_id, creds.pid(), engine, resource_reg, advertised).await
                {
                    eprintln!("handler error: {e:#}");
                }
            });
        }
    });
}

#[tokio::test]
async fn first_client_is_granted() {
    spawn_server(b"sgc-test-1", Policy::FairQueue).await;
    let mut a = TestClient::connect(b"sgc-test-1").await;
    assert_eq!(a.expect_advertise().await, vec![Resource::Display(DisplayResource::Fbdev)]);
    a.send(&ClientRequest::Acquire {
        resource: Resource::Display(DisplayResource::Fbdev),
    })
    .await;
    let fds = a.expect_grant().await;
    assert_eq!(fds.len(), 1);
}

#[tokio::test]
async fn second_client_preempts_and_handoff_completes() {
    spawn_server(b"sgc-test-2", Policy::FairQueue).await;
    let mut a = TestClient::connect(b"sgc-test-2").await;
    let mut b = TestClient::connect(b"sgc-test-2").await;
    let _ = a.expect_advertise().await;
    let _ = b.expect_advertise().await;

    a.send(&ClientRequest::Acquire {
        resource: Resource::Display(DisplayResource::Fbdev),
    })
    .await;
    a.expect_grant().await;

    // B acquires -> queued (silence); A is told to leave.
    b.send(&ClientRequest::Acquire {
        resource: Resource::Display(DisplayResource::Fbdev),
    })
    .await;
    b.expect_silence(Duration::from_millis(200)).await;
    a.expect_revoke().await;

    // A's revoke-ack Release hands the resource to B.
    a.send(&ClientRequest::Release {
        resource: Resource::Display(DisplayResource::Fbdev),
    })
    .await;
    b.expect_grant().await;
}

#[tokio::test]
async fn revoked_client_is_requeued_for_one_more_turn() {
    spawn_server(b"sgc-test-3", Policy::FairQueue).await;
    let mut a = TestClient::connect(b"sgc-test-3").await;
    let mut b = TestClient::connect(b"sgc-test-3").await;
    let _ = a.expect_advertise().await;
    let _ = b.expect_advertise().await;

    a.send(&ClientRequest::Acquire {
        resource: Resource::Display(DisplayResource::Fbdev),
    })
    .await;
    a.expect_grant().await;
    b.send(&ClientRequest::Acquire {
        resource: Resource::Display(DisplayResource::Fbdev),
    })
    .await;
    a.expect_revoke().await;
    a.send(&ClientRequest::Release {
        resource: Resource::Display(DisplayResource::Fbdev),
    })
    .await;
    b.expect_grant().await;

    // B releases voluntarily -> A (requeued) gets the unsolicited re-Grant.
    b.send(&ClientRequest::Release {
        resource: Resource::Display(DisplayResource::Fbdev),
    })
    .await;
    a.expect_grant().await;
}

#[tokio::test]
async fn first_owner_denies_new_acquires() {
    spawn_server(b"sgc-test-4", Policy::FirstOwner).await;
    let mut a = TestClient::connect(b"sgc-test-4").await;
    let mut b = TestClient::connect(b"sgc-test-4").await;
    let _ = a.expect_advertise().await;
    let _ = b.expect_advertise().await;

    a.send(&ClientRequest::Acquire {
        resource: Resource::Display(DisplayResource::Fbdev),
    })
    .await;
    a.expect_grant().await;
    b.send(&ClientRequest::Acquire {
        resource: Resource::Display(DisplayResource::Fbdev),
    })
    .await;
    let reason = b.expect_deny().await;
    assert!(reason.contains("owned"), "deny reason: {reason}");
}

#[tokio::test]
async fn queued_client_disconnect_does_not_wedge_queue() {
    spawn_server(b"sgc-test-6", Policy::FairQueue).await;
    let mut a = TestClient::connect(b"sgc-test-6").await;
    let mut b = TestClient::connect(b"sgc-test-6").await;
    let mut c = TestClient::connect(b"sgc-test-6").await;
    let _ = a.expect_advertise().await;
    let _ = b.expect_advertise().await;
    let _ = c.expect_advertise().await;

    a.send(&ClientRequest::Acquire {
        resource: Resource::Display(DisplayResource::Fbdev),
    })
    .await;
    a.expect_grant().await;

    // B queues, then gives up and disconnects.
    b.send(&ClientRequest::Acquire {
        resource: Resource::Display(DisplayResource::Fbdev),
    })
    .await;
    a.expect_revoke().await;
    drop(b);

    // A releases; the queue is empty, so C's fresh Acquire is granted.
    a.send(&ClientRequest::Release {
        resource: Resource::Display(DisplayResource::Fbdev),
    })
    .await;
    c.send(&ClientRequest::Acquire {
        resource: Resource::Display(DisplayResource::Fbdev),
    })
    .await;
    c.expect_grant().await;
}

#[tokio::test(start_paused = true)]
async fn silent_owner_is_force_reclaimed_after_timeout() {
    spawn_server(b"sgc-test-5", Policy::FairQueue).await;
    let mut a = TestClient::connect(b"sgc-test-5").await;
    let mut b = TestClient::connect(b"sgc-test-5").await;
    let _ = a.expect_advertise().await;
    let _ = b.expect_advertise().await;

    a.send(&ClientRequest::Acquire {
        resource: Resource::Display(DisplayResource::Fbdev),
    })
    .await;
    a.expect_grant().await;
    b.send(&ClientRequest::Acquire {
        resource: Resource::Display(DisplayResource::Fbdev),
    })
    .await;
    a.expect_revoke().await;

    // A never releases. Advance past the revoke deadline.
    tokio::time::advance(REVOKE_TIMEOUT + Duration::from_secs(1)).await;
    b.expect_grant().await;
}
