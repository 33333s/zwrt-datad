//! Direct (WebRTC) data path, the device end of docs/P2P.md.
//!
//! NMS stays the rendezvous: this side joins the session's rendezvous socket,
//! answers the browser's offer and serves the `nms` data channel. The classic
//! tunnel is the fallback, so every failure here simply ends the direct path.
mod frame;
mod peer;
mod rendezvous;
mod service;
mod stun;

#[cfg(test)]
mod e2e_tests;

use crate::cloud::Config;
use serde::Deserialize;
use serde_json::Value;
use std::{net::SocketAddr, time::Duration};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time::Instant,
};

/// A direct session never outlives one hour.
const MAX_TTL: Duration = Duration::from_secs(3600);
/// The browser has to appear within this long after the device joined.
const FIRST_PEER_GRACE: Duration = Duration::from_secs(120);
/// How long the browser may stay away once it was there.
const PEER_DOWN_GRACE: Duration = Duration::from_secs(30);
const MAX_OFFER_BYTES: usize = 16 * 1024;

/// The `mesh` object NMS adds to `remote.open`.
#[derive(Clone, Debug, Deserialize)]
pub struct Params {
    pub v: u32,
    #[serde(default)]
    pub stun: Vec<String>,
    #[serde(default)]
    pub udp_ports: String,
    pub ttl_seconds: u64,
    pub rendezvous_url: String,
}

impl Params {
    /// UDP port range for ICE. Only unprivileged ports, never a huge span.
    pub fn ports(&self) -> Option<(u16, u16)> {
        let (low, high) = match self.udp_ports.split_once('-') {
            Some((low, high)) => (low.parse::<u16>().ok()?, high.parse::<u16>().ok()?),
            None => {
                let port = self.udp_ports.parse::<u16>().ok()?;
                (port, port)
            }
        };
        (low >= 1024 && low <= high && high - low < 1024).then_some((low, high))
    }

    /// STUN servers as `(host, port)`; at most four, every entry well-formed.
    pub fn stun_servers(&self) -> Option<Vec<(String, u16)>> {
        if self.stun.len() > 4 {
            return None;
        }
        self.stun
            .iter()
            .map(|value| stun::parse_server(value))
            .collect()
    }
}

async fn resolve(servers: &[(String, u16)]) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    for (host, port) in servers {
        let found = tokio::time::timeout(
            Duration::from_secs(2),
            tokio::net::lookup_host((host.as_str(), *port)),
        )
        .await;
        if let Ok(Ok(addresses)) = found
            && let Some(address) = addresses.into_iter().find(SocketAddr::is_ipv4)
        {
            out.push(address);
        }
    }
    out
}

struct Peer {
    task: JoinHandle<()>,
    remote_ice: mpsc::Sender<Value>,
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Serves one direct session until it expires, is stopped or the browser is
/// gone for good. Nothing keeps running afterwards: the peer task, its UDP
/// socket and the rendezvous socket are all released before this returns.
pub async fn run(
    config: &Config,
    params: &Params,
    token: &str,
    port: u16,
    session_ttl: Duration,
    mut stop: watch::Receiver<bool>,
    loopback_only: bool,
) {
    let (Some(ports), Some(stun)) = (params.ports(), params.stun_servers()) else {
        return;
    };
    let deadline = Instant::now()
        + session_ttl
            .min(Duration::from_secs(params.ttl_seconds))
            .min(MAX_TTL);
    let (event_tx, mut events) = mpsc::channel(32);
    let (signal_tx, signal_rx) = mpsc::channel::<Value>(32);
    let (rendezvous_stop, rendezvous_stop_rx) = watch::channel(false);
    let rendezvous = tokio::spawn(rendezvous::run(
        config.clone(),
        params.rendezvous_url.clone(),
        token.to_owned(),
        event_tx,
        signal_rx,
        rendezvous_stop_rx,
    ));
    let mut peer: Option<Peer> = None;
    let mut ever_up = false;
    let mut give_up = Some(Instant::now() + FIRST_PEER_GRACE);
    loop {
        let give_up_at = give_up.unwrap_or(deadline);
        tokio::select! {
            _ = stop.changed() => break,
            _ = tokio::time::sleep_until(deadline) => break,
            _ = tokio::time::sleep_until(give_up_at), if give_up.is_some() => break,
            event = events.recv() => match event {
                None => break,
                Some(rendezvous::Event::PeerUp) => {
                    ever_up = true;
                    give_up = None;
                }
                Some(rendezvous::Event::PeerDown) => {
                    if ever_up {
                        give_up = Some(Instant::now() + PEER_DOWN_GRACE);
                    }
                }
                Some(rendezvous::Event::Signal(data)) => match data.get("type").and_then(Value::as_str) {
                    Some("offer") => {
                        let Some(sdp) = data.get("sdp").and_then(Value::as_str).filter(|s| s.len() <= MAX_OFFER_BYTES) else {
                            continue;
                        };
                        // A new offer means the browser started over.
                        drop(peer.take());
                        let (remote_ice, remote_rx) = mpsc::channel(64);
                        let addresses = resolve(&stun).await;
                        let task = tokio::spawn(peer::run(
                            sdp.to_owned(),
                            peer::Config { stun: addresses, ports, loopback_only },
                            peer::Link { signals: signal_tx.clone(), remote_ice: remote_rx, port },
                        ));
                        peer = Some(Peer { task, remote_ice });
                    }
                    Some("ice") => {
                        if let Some(peer) = &peer {
                            let _ = peer.remote_ice.try_send(data);
                        }
                    }
                    _ => {}
                },
            },
        }
    }
    drop(peer);
    rendezvous_stop.send_replace(true);
    if tokio::time::timeout(Duration::from_secs(2), rendezvous)
        .await
        .is_err()
    {
        // The task ends with `rendezvous_stop`; the timeout only bounds a stuck close.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(stun: &[&str], ports: &str) -> Params {
        Params {
            v: 1,
            stun: stun.iter().map(|s| (*s).to_owned()).collect(),
            udp_ports: ports.into(),
            ttl_seconds: 600,
            rendezvous_url: "wss://nms.example/api/remote/device/s?transport=mesh".into(),
        }
    }

    #[test]
    fn port_ranges_are_unprivileged_and_bounded() {
        assert_eq!(params(&[], "49160-49223").ports(), Some((49160, 49223)));
        assert_eq!(params(&[], "50000").ports(), Some((50000, 50000)));
        for bad in [
            "",
            "80-90",
            "1023-2000",
            "49223-49160",
            "1024-5000",
            "a-b",
            "49160-",
            "-49223",
            "70000-70001",
        ] {
            assert!(params(&[], bad).ports().is_none(), "{bad}");
        }
    }

    #[test]
    fn stun_lists_are_short_and_well_formed() {
        let ok = params(
            &[
                "stun:services.ericsfj.com:3478",
                "stun:stun.cloudflare.com:3478",
            ],
            "49160-49223",
        );
        assert_eq!(ok.stun_servers().unwrap().len(), 2);
        assert_eq!(params(&[], "49160-49223").stun_servers(), Some(vec![]));
        let five = ["stun:a.example:1"; 5];
        assert!(params(&five, "49160-49223").stun_servers().is_none());
        assert!(
            params(&["turn:a.example:3478"], "49160-49223")
                .stun_servers()
                .is_none()
        );
        assert!(
            params(
                &["stun:ok.example:3478", "stun:bad host:3478"],
                "49160-49223"
            )
            .stun_servers()
            .is_none()
        );
    }

    #[test]
    fn the_mesh_object_parses_and_tolerates_extra_fields() {
        let json = r#"{"v":1,"stun":["stun:a.example:3478"],"udp_ports":"49160-49223","ttl_seconds":3600,
            "rendezvous_url":"wss://n/api/remote/device/s?transport=mesh","only":true,"http":true}"#;
        let parsed: Params = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.ttl_seconds, 3600);
        assert!(serde_json::from_str::<Params>(r#"{"v":1}"#).is_err());
    }
}
