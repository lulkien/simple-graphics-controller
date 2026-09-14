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
    ClientRequest, FRAME_HEADER_LEN, InputResource, Resource, ServerMessage, deserialize,
    parse_frame_header, serialize_framed,
};
use tokio::{
    io::AsyncWriteExt,
    net::{UnixListener, UnixStream, unix::SocketAddr as TokioSocketAddr},
};

use crate::{
    client_handler::handle_connection,
    resource_manager::ResourceRegistries,
    types::{AdvertisedResources, ClientId, ResourceRegistry},
    windowing::{ControlMessage, Policy, PolicyEngine, REVOKE_TIMEOUT},
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

    /// Is there really nothing to read? A short pause, then a peek: enough to
    /// catch a message that was queued right after the previous one.
    async fn no_message(&mut self) -> bool {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut byte = [0u8; 1];
        matches!(
            self.stream.try_read(&mut byte),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock
        )
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
/// Returns the engine and advertised list, so a test can drive what the input
/// reconciler would do (offer a resource, push a new list).
async fn spawn_server(
    name: &'static [u8],
    policy: Policy,
) -> (PolicyEngine, Arc<AdvertisedResources>) {
    static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);

    // A /dev/null fd stands in for the fbdev fd (hosts lack /dev/fb0).
    let resource_reg: ResourceRegistry = Arc::new(DashMap::new());
    let file = std::fs::File::open("/dev/null").expect("/dev/null");
    resource_reg.insert(Resource::Fbdev, file.into());
    // Same stand-in for input: a host has no /dev/input/event*, and a resource
    // offered at runtime is granted from the registry like any other.
    let keyboard = Resource::Input(InputResource::Keyboard(0));
    let fake_input = std::fs::File::open("/dev/null").expect("/dev/null");
    resource_reg.insert(keyboard, fake_input.into());
    #[cfg(feature = "drm")]
    let drm_registry = Arc::new(DashMap::new());
    // No DRM cards on test hosts; the lease registry stays empty.
    let registries = ResourceRegistries {
        fds: resource_reg,
        #[cfg(feature = "drm")]
        drm: drm_registry,
    };
    let advertised = Arc::new(AdvertisedResources::new(vec![Resource::Fbdev]));

    let engine = PolicyEngine::spawn(std::collections::HashMap::from([(Resource::Fbdev, policy)]));

    let addr = StdSocketAddr::from_abstract_name(name).expect("invalid abstract address");
    let std_listener = StdUnixListener::bind_addr(&addr).expect("bind");
    std_listener.set_nonblocking(true).expect("nonblocking");
    let listener = UnixListener::from_std(std_listener).expect("wrap listener");

    let accept_engine = engine.clone();
    let accept_advertised = advertised.clone();
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
            let engine = accept_engine.clone();
            let registries = registries.clone();
            let advertised = accept_advertised.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_connection(
                    stream,
                    client_id,
                    creds.pid(),
                    engine,
                    registries,
                    advertised,
                )
                .await
                {
                    eprintln!("handler error: {e:#}");
                }
            });
        }
    });

    (engine, advertised)
}

#[tokio::test]
async fn first_client_is_granted() {
    let _ = spawn_server(b"sgc-test-1", Policy::FairQueue).await;
    let mut a = TestClient::connect(b"sgc-test-1").await;
    assert_eq!(a.expect_advertise().await, vec![Resource::Fbdev]);
    a.send(&ClientRequest::Acquire {
        resource: Resource::Fbdev,
    })
    .await;
    let fds = a.expect_grant().await;
    assert_eq!(fds.len(), 1);
}

#[tokio::test]
async fn second_client_preempts_and_handoff_completes() {
    let _ = spawn_server(b"sgc-test-2", Policy::FairQueue).await;
    let mut a = TestClient::connect(b"sgc-test-2").await;
    let mut b = TestClient::connect(b"sgc-test-2").await;
    let _ = a.expect_advertise().await;
    let _ = b.expect_advertise().await;

    a.send(&ClientRequest::Acquire {
        resource: Resource::Fbdev,
    })
    .await;
    a.expect_grant().await;

    // B acquires -> queued (silence); A is told to leave.
    b.send(&ClientRequest::Acquire {
        resource: Resource::Fbdev,
    })
    .await;
    b.expect_silence(Duration::from_millis(200)).await;
    a.expect_revoke().await;

    // A's revoke-ack Release hands the resource to B.
    a.send(&ClientRequest::Release {
        resource: Resource::Fbdev,
    })
    .await;
    b.expect_grant().await;
}

#[tokio::test]
async fn revoked_client_is_requeued_for_one_more_turn() {
    let _ = spawn_server(b"sgc-test-3", Policy::FairQueue).await;
    let mut a = TestClient::connect(b"sgc-test-3").await;
    let mut b = TestClient::connect(b"sgc-test-3").await;
    let _ = a.expect_advertise().await;
    let _ = b.expect_advertise().await;

    a.send(&ClientRequest::Acquire {
        resource: Resource::Fbdev,
    })
    .await;
    a.expect_grant().await;
    b.send(&ClientRequest::Acquire {
        resource: Resource::Fbdev,
    })
    .await;
    a.expect_revoke().await;
    a.send(&ClientRequest::Release {
        resource: Resource::Fbdev,
    })
    .await;
    b.expect_grant().await;

    // B releases voluntarily -> A (requeued) gets the unsolicited re-Grant.
    b.send(&ClientRequest::Release {
        resource: Resource::Fbdev,
    })
    .await;
    a.expect_grant().await;
}

#[tokio::test]
async fn first_owner_denies_new_acquires() {
    let _ = spawn_server(b"sgc-test-4", Policy::FirstOwner).await;
    let mut a = TestClient::connect(b"sgc-test-4").await;
    let mut b = TestClient::connect(b"sgc-test-4").await;
    let _ = a.expect_advertise().await;
    let _ = b.expect_advertise().await;

    a.send(&ClientRequest::Acquire {
        resource: Resource::Fbdev,
    })
    .await;
    a.expect_grant().await;
    b.send(&ClientRequest::Acquire {
        resource: Resource::Fbdev,
    })
    .await;
    let reason = b.expect_deny().await;
    assert!(reason.contains("owned"), "deny reason: {reason}");
}

#[tokio::test]
async fn queued_client_disconnect_does_not_wedge_queue() {
    let _ = spawn_server(b"sgc-test-6", Policy::FairQueue).await;
    let mut a = TestClient::connect(b"sgc-test-6").await;
    let mut b = TestClient::connect(b"sgc-test-6").await;
    let mut c = TestClient::connect(b"sgc-test-6").await;
    let _ = a.expect_advertise().await;
    let _ = b.expect_advertise().await;
    let _ = c.expect_advertise().await;

    a.send(&ClientRequest::Acquire {
        resource: Resource::Fbdev,
    })
    .await;
    a.expect_grant().await;

    // B queues, then gives up and disconnects.
    b.send(&ClientRequest::Acquire {
        resource: Resource::Fbdev,
    })
    .await;
    a.expect_revoke().await;
    drop(b);

    // A releases; the queue is empty, so C's fresh Acquire is granted.
    a.send(&ClientRequest::Release {
        resource: Resource::Fbdev,
    })
    .await;
    c.send(&ClientRequest::Acquire {
        resource: Resource::Fbdev,
    })
    .await;
    c.expect_grant().await;
}

#[tokio::test(start_paused = true)]
async fn silent_owner_is_force_reclaimed_after_timeout() {
    let _ = spawn_server(b"sgc-test-5", Policy::FairQueue).await;
    let mut a = TestClient::connect(b"sgc-test-5").await;
    let mut b = TestClient::connect(b"sgc-test-5").await;
    let _ = a.expect_advertise().await;
    let _ = b.expect_advertise().await;

    a.send(&ClientRequest::Acquire {
        resource: Resource::Fbdev,
    })
    .await;
    a.expect_grant().await;
    b.send(&ClientRequest::Acquire {
        resource: Resource::Fbdev,
    })
    .await;
    a.expect_revoke().await;

    // A never releases. Advance past the revoke deadline.
    tokio::time::advance(REVOKE_TIMEOUT + Duration::from_secs(1)).await;
    b.expect_grant().await;
}

/// A client's list is not frozen at connect time: the server pushes the current
/// one when a device appears or goes away, which is what makes a connection that
/// is already up able to see a device plugged in later.
#[tokio::test]
async fn a_runtime_device_change_is_pushed_to_connected_clients() {
    let keyboard = Resource::Input(InputResource::Keyboard(0));
    let (engine, advertised) = spawn_server(b"sgc-test-hotplug", Policy::FairQueue).await;
    let mut a = TestClient::connect(b"sgc-test-hotplug").await;
    assert_eq!(a.expect_advertise().await, vec![Resource::Fbdev]);

    // The display first: input belongs to the app on screen.
    a.send(&ClientRequest::Acquire {
        resource: Resource::Fbdev,
    })
    .await;
    a.expect_grant().await;

    // Exactly what the input reconciler does for a device that appears: offer it
    // to the engine first (so an Acquire is accepted), add it to the list, then
    // push the list.
    engine.offer(keyboard.clone(), Policy::FairQueue).await;
    advertised.insert(keyboard.clone());
    engine
        .broadcast(ControlMessage::Advertise {
            available_resources: advertised.snapshot(),
        })
        .await;

    // The already-connected client is told, without reconnecting...
    assert_eq!(
        a.expect_advertise().await,
        vec![Resource::Fbdev, keyboard.clone()]
    );
    // ...and what it was told about is acquirable straight away.
    a.send(&ClientRequest::Acquire {
        resource: keyboard.clone(),
    })
    .await;
    assert_eq!(a.expect_grant().await.len(), 1);

    // The same on the way out, and this is the point of suspension: a device
    // that goes away is NOT taken from its holder — the list it was told about
    // simply shrinks.
    engine.suspend(keyboard.clone()).await;
    advertised.remove(&keyboard);
    engine
        .broadcast(ControlMessage::Advertise {
            available_resources: advertised.snapshot(),
        })
        .await;
    assert_eq!(a.expect_advertise().await, vec![Resource::Fbdev]);
    assert!(
        a.no_message().await,
        "a suspended device must not be revoked"
    );

    // And when it comes back, the same client is handed a fresh fd over the
    // wire — it never lost the resource, so it has nothing to acquire.
    engine.resume(keyboard.clone()).await;
    advertised.insert(keyboard.clone());
    engine
        .broadcast(ControlMessage::Advertise {
            available_resources: advertised.snapshot(),
        })
        .await;
    assert_eq!(a.expect_grant().await.len(), 1);
    assert_eq!(
        a.expect_advertise().await,
        vec![Resource::Fbdev, keyboard.clone()]
    );
}

/// Input is owned by class: a client with no display may hold a device nobody
/// wants, but it can neither take one from anybody nor queue for one — and the
/// app on the display takes it whenever it asks.
#[tokio::test]
async fn input_without_a_display_is_low_priority() {
    let keyboard = Resource::Input(InputResource::Keyboard(0));
    let (engine, _advertised) = spawn_server(b"sgc-test-seat-1", Policy::FairQueue).await;
    let mut a = TestClient::connect(b"sgc-test-seat-1").await;
    let mut b = TestClient::connect(b"sgc-test-seat-1").await;
    let _ = a.expect_advertise().await;
    let _ = b.expect_advertise().await;
    engine.offer(keyboard.clone(), Policy::FairQueue).await;

    // A has no display and takes the free device.
    a.send(&ClientRequest::Acquire {
        resource: keyboard.clone(),
    })
    .await;
    assert_eq!(a.expect_grant().await.len(), 1);

    // B has no display either: the device is not free, and B does not get to
    // wait for it.
    b.send(&ClientRequest::Acquire {
        resource: keyboard.clone(),
    })
    .await;
    let reason = b.expect_deny().await;
    assert!(
        reason.contains("only the app on the display"),
        "deny reason: {reason}"
    );

    // B takes the display — so B is the app on screen — and takes the device
    // from A with it.
    b.send(&ClientRequest::Acquire {
        resource: Resource::Fbdev,
    })
    .await;
    b.expect_grant().await;
    b.send(&ClientRequest::Acquire {
        resource: keyboard.clone(),
    })
    .await;
    b.expect_silence(Duration::from_millis(200)).await;
    a.expect_revoke().await;
    a.send(&ClientRequest::Release {
        resource: keyboard.clone(),
    })
    .await;
    assert_eq!(b.expect_grant().await.len(), 1);
}

/// The seat's devices leave with the seat: the next app on screen can take them,
/// and the previous one is asked to let go when it hands the display over.
#[tokio::test]
async fn a_seat_change_releases_the_inputs() {
    let keyboard = Resource::Input(InputResource::Keyboard(0));
    let (engine, _advertised) = spawn_server(b"sgc-test-seat-2", Policy::FairQueue).await;
    let mut a = TestClient::connect(b"sgc-test-seat-2").await;
    let mut b = TestClient::connect(b"sgc-test-seat-2").await;
    let _ = a.expect_advertise().await;
    let _ = b.expect_advertise().await;
    engine.offer(keyboard.clone(), Policy::FairQueue).await;

    // A is the app on screen, display and device.
    a.send(&ClientRequest::Acquire {
        resource: Resource::Fbdev,
    })
    .await;
    a.expect_grant().await;
    a.send(&ClientRequest::Acquire {
        resource: keyboard.clone(),
    })
    .await;
    a.expect_grant().await;

    // B takes the screen: A is asked for the display, and once it hands it over
    // its input goes with the seat.
    b.send(&ClientRequest::Acquire {
        resource: Resource::Fbdev,
    })
    .await;
    b.expect_silence(Duration::from_millis(200)).await;
    a.expect_revoke().await;
    a.send(&ClientRequest::Release {
        resource: Resource::Fbdev,
    })
    .await;
    b.expect_grant().await;
    a.expect_revoke().await; // the keyboard, with the seat

    // B is the app on screen now and takes the device once A lets go.
    b.send(&ClientRequest::Acquire {
        resource: keyboard.clone(),
    })
    .await;
    b.expect_silence(Duration::from_millis(200)).await;
    a.send(&ClientRequest::Release {
        resource: keyboard.clone(),
    })
    .await;
    assert_eq!(b.expect_grant().await.len(), 1);
}
