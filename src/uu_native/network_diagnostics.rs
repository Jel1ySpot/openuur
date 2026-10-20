//! Address-free ICE path diagnostics.
//!
//! Logs only allowlisted enums (protocol, candidate type, route) and byte
//! rates. No IP address, candidate id, SDP or TURN credential is logged.
//! `direct` means "no relay candidate", not proof of a shared LAN.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Weak;
use std::time::{Duration, Instant};

use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::stats::StatsReportType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CandidateSummary {
    pub protocol: &'static str,
    pub kind: &'static str,
}

fn protocol(value: &str) -> &'static str {
    if value.eq_ignore_ascii_case("udp") {
        "udp"
    } else if value.eq_ignore_ascii_case("tcp") {
        "tcp"
    } else {
        "unknown"
    }
}

fn kind(value: &str) -> &'static str {
    match value {
        "host" => "host",
        "srflx" => "srflx",
        "prflx" => "prflx",
        "relay" => "relay",
        _ => "unknown",
    }
}

/// Parse only protocol/type from an SDP candidate line.
pub fn candidate_summary(sdp: &str) -> CandidateSummary {
    let fields: Vec<_> = sdp.split_whitespace().collect();
    CandidateSummary {
        protocol: fields.get(2).map_or("unknown", |value| protocol(value)),
        kind: if fields.get(6) == Some(&"typ") {
            fields.get(7).map_or("unknown", |value| kind(value))
        } else {
            "unknown"
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathSummary {
    pub local: CandidateSummary,
    pub remote: CandidateSummary,
    pub route: &'static str,
}

/// `webrtc` 0.10 keeps candidate-pair fields private; its Display form is
/// `(local) <protocol> <type> <endpoint> <-> (remote) ...`. Fail closed if the
/// representation changes.
fn selected_pair_summary(display: &str) -> Option<PathSummary> {
    let (local, remote) = display
        .strip_prefix("(local) ")?
        .split_once(" <-> (remote) ")?;
    fn side(value: &str) -> CandidateSummary {
        let mut fields = value.split_whitespace();
        CandidateSummary {
            protocol: protocol(fields.next().unwrap_or("")),
            kind: kind(fields.next().unwrap_or("")),
        }
    }
    let local = side(local);
    let remote = side(remote);
    let route = if local.kind == "relay" || remote.kind == "relay" {
        "relay"
    } else if local.kind == "unknown"
        || remote.kind == "unknown"
        || local.protocol == "unknown"
        || remote.protocol == "unknown"
    {
        "unknown"
    } else {
        "direct"
    };
    Some(PathSummary {
        local,
        remote,
        route,
    })
}

pub async fn selected_path(pc: &RTCPeerConnection) -> Option<PathSummary> {
    let pair = pc
        .dtls_transport()
        .ice_transport()
        .get_selected_candidate_pair()
        .await?;
    Some(
        selected_pair_summary(&pair.to_string()).unwrap_or(PathSummary {
            local: CandidateSummary {
                protocol: "unknown",
                kind: "unknown",
            },
            remote: CandidateSummary {
                protocol: "unknown",
                kind: "unknown",
            },
            route: "unknown",
        }),
    )
}

#[derive(Default)]
struct RateCounter {
    previous: Option<(Instant, u64, u64)>,
}

impl RateCounter {
    fn sample(&mut self, now: Instant, sent: u64, received: u64) -> Option<(u64, u64)> {
        let old = self.previous.replace((now, sent, received))?;
        let seconds = now.checked_duration_since(old.0)?.as_secs_f64();
        if seconds <= 0.0 || sent < old.1 || received < old.2 {
            return None;
        }
        Some((
            ((sent - old.1) as f64 * 8.0 / seconds) as u64,
            ((received - old.2) as f64 * 8.0 / seconds) as u64,
        ))
    }
}

static SESSION_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Spawn one diagnostics task per peer. Weak ownership makes the task exit
/// with the peer; reconnects start a new session id and reset counters.
pub fn start(peer: Weak<RTCPeerConnection>) {
    let session = SESSION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut rate = RateCounter::default();
        let mut last_path = None;
        let mut ticks = 0u64;
        loop {
            interval.tick().await;
            let Some(pc) = peer.upgrade() else { break };
            let state = pc.connection_state();
            if state == RTCPeerConnectionState::Closed {
                break;
            }
            if state != RTCPeerConnectionState::Connected {
                rate = RateCounter::default();
                last_path = None;
                continue;
            }
            let Some(path) = selected_path(&pc).await else {
                continue;
            };
            let report = pc.get_stats().await;
            let (mut sent, mut received) = (0u64, 0u64);
            for stats in report.reports.values() {
                // SCTPTransport repeats the same ICE totals; count Transport only.
                if let StatsReportType::Transport(stats) = stats {
                    sent = sent.saturating_add(stats.bytes_sent as u64);
                    received = received.saturating_add(stats.bytes_received as u64);
                }
            }
            let rates = rate.sample(Instant::now(), sent, received);
            ticks += 1;
            if last_path != Some(path) || ticks.is_multiple_of(6) {
                tracing::info!(
                    "ICE path: session={} route={} local={}/{} remote={}/{} tx_bps={:?} rx_bps={:?}",
                    session,
                    path.route,
                    path.local.protocol,
                    path.local.kind,
                    path.remote.protocol,
                    path.remote.kind,
                    rates.map(|value| value.0),
                    rates.map(|value| value.1)
                );
                last_path = Some(path);
            }
        }
    });
}

/// True when the selected pair has been gone for `grace`, signalling that a
/// network switch or interface loss needs an ICE restart.
pub fn transport_interrupted(
    pc: &RTCPeerConnection,
    disconnected_since: Option<Instant>,
    grace: Duration,
) -> bool {
    pc.connection_state() == RTCPeerConnectionState::Disconnected
        && disconnected_since.is_some_and(|since| since.elapsed() >= grace)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_candidates_without_leaking_addresses() {
        let summary = candidate_summary(
            "candidate:x 1 UDP 123 192.0.2.5 4444 typ relay raddr 10.0.0.1 rport 123 ufrag secret",
        );
        assert_eq!(
            summary,
            CandidateSummary {
                protocol: "udp",
                kind: "relay"
            }
        );
        assert_eq!(
            candidate_summary("malformed secret"),
            CandidateSummary {
                protocol: "unknown",
                kind: "unknown"
            }
        );
        assert!(selected_pair_summary("unexpected secret").is_none());
    }

    #[test]
    fn pair_display_maps_both_relay_sides() {
        use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
        use webrtc::ice_transport::ice_candidate_pair::RTCIceCandidatePair;
        use webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType;
        use webrtc::ice_transport::ice_protocol::RTCIceProtocol;
        for (local, remote, expected) in [
            (
                RTCIceCandidateType::Host,
                RTCIceCandidateType::Srflx,
                "direct",
            ),
            (
                RTCIceCandidateType::Relay,
                RTCIceCandidateType::Host,
                "relay",
            ),
            (
                RTCIceCandidateType::Host,
                RTCIceCandidateType::Relay,
                "relay",
            ),
            (
                RTCIceCandidateType::Unspecified,
                RTCIceCandidateType::Host,
                "unknown",
            ),
        ] {
            let candidate = |typ| RTCIceCandidate {
                typ,
                protocol: RTCIceProtocol::Udp,
                address: "192.0.2.42".into(),
                port: 1234,
                ..Default::default()
            };
            let summary = selected_pair_summary(
                &RTCIceCandidatePair::new(candidate(local), candidate(remote)).to_string(),
            )
            .unwrap();
            assert_eq!(summary.route, expected);
            assert!(!format!("{summary:?}").contains("192.0.2.42"));
        }
    }

    #[test]
    fn rates_reset_on_counter_restart() {
        let mut rates = RateCounter::default();
        let now = Instant::now();
        assert_eq!(rates.sample(now, 100, 200), None);
        assert_eq!(
            rates.sample(now + Duration::from_secs(2), 300, 300),
            Some((800, 400))
        );
        assert_eq!(rates.sample(now + Duration::from_secs(3), 0, 0), None);
    }
}
