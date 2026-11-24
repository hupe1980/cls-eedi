//! Exchange a real MQTT 5 read and state across the gateway simulator’s two authenticated TLS connections.
#[path = "support/pki.rs"]
mod pki;

use cls_eedi::{
    engine::{Backend, BackendEvent, Clock, DeviceEvent, LocalDevice},
    transport::{
        mqtt::{Broker, Session, SessionEvent},
        Role, Routes, Target,
    },
};
use smgw_cls::transport::{SmgwProxy, TlsEndpoint};
use std::{
    error::Error,
    io,
    net::{Ipv4Addr, SocketAddr},
    path::Path,
    time::{Duration, Instant},
};
use tokio::{
    io::copy_bidirectional,
    net::{TcpListener, TcpStream},
    time::timeout,
};
use uuid::Uuid;

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;
const SETUP: Duration = Duration::from_secs(5);
const BUDGET: Duration = Duration::from_secs(15);
const ROUTE: &str = "cls.test";

async fn listener() -> io::Result<TcpListener> {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await
}

fn gateway(pki: &Path, wan: SocketAddr) -> Result<SmgwProxy> {
    Ok(SmgwProxy::new(
        TlsEndpoint::load(pki, "han", "smgw", "cls")?,
        TlsEndpoint::load(pki, "wan", "smgw", "aemt")?,
        ROUTE.into(),
        wan,
        "aemt.test".into(),
        SETUP,
        BUDGET,
    )?)
}

#[tokio::test]
async fn unprovisioned_sni_rejected_before_wan_connect() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let pki = directory.path().join("pki");
    pki::generate(&pki)?;
    let han = listener().await?;
    let wan = listener().await?;
    let mut proxy = gateway(&pki, wan.local_addr()?)?;
    let cls = TlsEndpoint::load(&pki, "han", "cls", "smgw")?;
    let (client, relay) = tokio::join!(
        cls.connect(han.local_addr()?, "unknown.test", SETUP),
        async { proxy.relay(han.accept().await?.0).await }
    );
    let _client = client?;
    assert_eq!(relay.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    assert!(timeout(Duration::from_millis(100), wan.accept())
        .await
        .is_err());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable loopback broker specified by CLS_EEDI_BROKER_PORT"]
async fn backend_reads_device_through_authenticated_cls_channel() -> Result<()> {
    let broker_port: u16 = std::env::var("CLS_EEDI_BROKER_PORT")?.parse()?;
    let directory = tempfile::tempdir()?;
    let pki = directory.path().join("pki");
    pki::generate(&pki)?;
    let plain_cls = listener().await?;
    let han = listener().await?;
    let wan = listener().await?;
    let local_port = plain_cls.local_addr()?.port();
    let han_address = han.local_addr()?;
    let mut proxy = gateway(&pki, wan.local_addr()?)?;
    let cls = TlsEndpoint::load(&pki, "han", "cls", "smgw")?;
    let aemt = TlsEndpoint::load(&pki, "wan", "aemt", "smgw")?;
    // Session accepts a TCP port. Adapt it to the already-authenticated channel.
    let device_ingress = tokio::spawn(async move {
        let (mut client, _) = plain_cls.accept().await?;
        let mut secure = cls.connect(han_address, ROUTE, SETUP).await?;
        copy_bidirectional(&mut client, &mut secure).await
    });
    let relay = tokio::spawn(async move { proxy.relay(han.accept().await?.0).await });
    let broker_egress = tokio::spawn(async move {
        let mut secure = aemt.accept(wan.accept().await?.0, SETUP).await?;
        let mut broker = TcpStream::connect((Ipv4Addr::LOCALHOST, broker_port)).await?;
        // Exercise startup latency longer than the steady-state polling interval.
        tokio::time::sleep(Duration::from_millis(75)).await;
        copy_bidirectional(&mut secure, &mut broker).await
    });
    let cut = relay.abort_handle();
    let exercise = tokio::task::spawn_blocking(move || exchange(local_port, broker_port, cut));
    if let Err(error) = timeout(BUDGET, exercise).await?? {
        if device_ingress.is_finished() {
            eprintln!("local bridge: {:?}", device_ingress.await?);
        }
        if relay.is_finished() {
            eprintln!("gateway: {:?}", relay.await?);
        }
        if broker_egress.is_finished() {
            eprintln!("broker bridge: {:?}", broker_egress.await?);
        }
        return Err(error);
    }
    // The exchange intentionally cuts the authenticated relay after receiving the
    // correlated state. The broker will establishes downstream observation of that loss.
    assert!(relay.await.unwrap_err().is_cancelled());
    device_ingress.abort();
    broker_egress.abort();
    Ok(())
}

fn exchange(local_port: u16, broker_port: u16, cut: tokio::task::AbortHandle) -> Result<()> {
    let run = Uuid::new_v4();
    let routes = Routes::new(format!("smgw/{run}/down"), format!("smgw/{run}/up"))?;
    let session = |role, tag, port| -> Result<Session> {
        let broker = Broker::loopback(&format!("{tag}-{run}"), Ipv4Addr::LOCALHOST.into(), port)?;
        Ok(Session::new(broker, role, routes.clone(), tag)?)
    };
    let mut backend_link = session(Role::Backend, "backend", broker_port)?;
    let mut device_link = session(Role::LocalDevice, "device", local_port)?;
    let mut backend = Backend::new("local", "backend", 8);
    let mut device = LocalDevice::new("local", "device", false, 8);
    let now = Clock::new(1000, 0);
    let deadline = Instant::now() + SETUP;
    let mut sent = false;
    let mut online = false;
    let mut readback = false;
    let mut device_ready = false;
    let mut backend_ready = false;
    while !online || !readback {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "MQTT presence/read/state exchange",
            )
            .into());
        }
        match device_link
            .poll(Duration::from_millis(10))
            .map_err(|e| io::Error::other(format!("local MQTT: {e:?}")))?
        {
            Some(SessionEvent::Ready) => device_ready = true,
            Some(SessionEvent::Message {
                target: Target::Local,
                message,
            }) => {
                assert_eq!(message.id, "read-1");
                let DeviceEvent::Reply(state) =
                    device.receive("local", &message, "state-1", now)?
                else {
                    panic!("expected a state reply")
                };
                device_link.publish(&Target::Local, &state)?;
            }
            _ => {}
        }
        match backend_link
            .poll(Duration::from_millis(10))
            .map_err(|e| io::Error::other(format!("backend MQTT: {e:?}")))?
        {
            Some(SessionEvent::Ready) => backend_ready = true,
            Some(SessionEvent::Message {
                target: Target::Presence,
                message,
            }) => {
                let BackendEvent::Presence(status) =
                    backend.receive("local", &message, "unused", now)?
                else {
                    panic!("expected presence")
                };
                online = status.connected && status.timestamp.is_some();
            }
            Some(SessionEvent::Message {
                target: Target::Local,
                message,
            }) => {
                assert_eq!(message.relation.as_deref(), Some("read-1"));
                assert!(matches!(
                    backend.receive("local", &message, "unused", now)?,
                    BackendEvent::StateUpdated
                ));
                assert_eq!(backend.state().unwrap().timestamp, now.unix_seconds);
                readback = true;
            }
            _ => {}
        }
        if backend_ready && device_ready && !sent {
            backend_link.publish(&Target::Local, &backend.read("read-1", vec![])?)?;
            sent = true;
        }
    }
    // Keep both MQTT clients alive and sever only the simulated gateway session.
    cut.abort();
    loop {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "broker last will after channel loss",
            )
            .into());
        }
        if let Some(SessionEvent::Message {
            target: Target::Presence,
            message,
        }) = backend_link.poll(Duration::from_millis(10))?
        {
            if let BackendEvent::Presence(status) =
                backend.receive("local", &message, "unused", now)?
            {
                if !status.connected {
                    assert_eq!(status.timestamp, None);
                    break;
                }
            }
        }
    }
    Ok(())
}
