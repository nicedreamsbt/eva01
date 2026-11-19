//! Routes from the local nicedreamswap daemon.
//!
//! `nicedreamswapd` keeps pool state live and answers over a Unix socket in about a millisecond
//! with a Jupiter-program route instruction, so a liquidation it can route needs no round trip to
//! a venue and spends none of a venue's rate limit. It is asked first; when it declines (the pair
//! is not covered, a pool is stale, the route cannot clear the floor) or does not answer, the
//! caller carries on to the venues as before.
//!
//! The daemon is off unless Eva01 is built with the `nds` feature (Unix only) and `NDS_SOCKET` is
//! set. Without either, [`NdsSource::from_env`] is `None` and nothing here is reached.
//!
//! - `NDS_SOCKET`: the daemon's socket path.
//! - `NDS_ENABLED`: `0` turns it off with the socket still set.
//! - `NDS_TIMEOUT_MS`: how long one call may take (default 25).

use std::time::Duration;

use anyhow::Result;
use solana_dex_superagg::{Aggregator, RouteRequest, SwapInstructions};
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;

/// What the daemon said to one request.
pub enum NdsAnswer {
    /// A route that clears the request's `min_out`, and a note for the liquidation's timeline.
    Routed(Box<SwapInstructions>, String),
    /// The daemon answered that it has no route, and why.
    Declined(String),
}

pub struct NdsSource {
    #[cfg(all(unix, feature = "nds"))]
    client: std::sync::Mutex<nds_client::Client>,
    #[cfg(all(unix, feature = "nds"))]
    next_id: std::sync::atomic::AtomicU64,
    timeout: Duration,
}

#[cfg(not(all(unix, feature = "nds")))]
impl NdsSource {
    pub fn from_env() -> Option<Self> {
        if std::env::var_os("NDS_SOCKET").is_some() {
            log::warn!("NDS_SOCKET is set but this build has no nicedreamswap client (build with --features nds on Unix); ignoring it");
        }
        None
    }

    pub fn route(&self, _request: &RouteRequest) -> Result<NdsAnswer> {
        let _ = self.timeout;
        anyhow::bail!("built without the nds feature")
    }
}

#[cfg(all(unix, feature = "nds"))]
impl NdsSource {
    pub fn from_env() -> Option<Self> {
        let path = std::env::var("NDS_SOCKET").ok().filter(|p| !p.trim().is_empty())?;
        if std::env::var("NDS_ENABLED").is_ok_and(|v| v.trim() == "0") {
            return None;
        }
        let timeout_ms = std::env::var("NDS_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|ms| *ms > 0)
            .unwrap_or(25);
        log::info!("NDS: asking nicedreamswap at {path} first (timeout {timeout_ms}ms)");
        Some(Self {
            client: std::sync::Mutex::new(nds_client::Client::new(path, "eva01")),
            next_id: std::sync::atomic::AtomicU64::new(1),
            timeout: Duration::from_millis(timeout_ms),
        })
    }

    /// Asks the daemon for `request`'s route. An `Err` is a failure to get an answer (socket
    /// missing, timeout, a broken connection or a reply that does not hold up); the caller falls
    /// back as it would for a decline.
    pub fn route(&self, request: &RouteRequest) -> Result<NdsAnswer> {
        use nds_client::proto::{NoRouteReason, Response};

        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let wire = nds_client::proto::RouteRequest {
            id,
            input_mint: request.input_mint.to_bytes(),
            output_mint: request.output_mint.to_bytes(),
            amount_in: request.amount,
            min_out: request.min_out,
            max_accounts: request.max_accounts.min(u16::MAX as usize) as u16,
            locked_accounts: request
                .transaction_accounts
                .iter()
                .map(|key| key.to_bytes())
                .collect(),
            allowed_programs: request
                .allowed_programs
                .iter()
                .flatten()
                .map(|key| key.to_bytes())
                .collect(),
            wallet: request.user.to_bytes(),
            deadline_ms: self.timeout.as_millis().min(u32::MAX as u128) as u32,
        };
        let response = self
            .client
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .route(&wire, self.timeout)?;
        match response {
            Response::Routed(routed) => {
                if routed.id != id {
                    anyhow::bail!("nds answered request {} to request {id}", routed.id);
                }
                // The instruction enforces this on chain; refuse a reply that says otherwise
                // rather than send a transaction that cannot land.
                if routed.min_out < request.min_out {
                    anyhow::bail!(
                        "nds route guarantees {} under the {} floor",
                        routed.min_out,
                        request.min_out
                    );
                }
                let instructions = routed
                    .instructions
                    .iter()
                    .map(|ix| Instruction {
                        program_id: Pubkey::new_from_array(ix.program),
                        accounts: ix
                            .accounts
                            .iter()
                            .map(|a| AccountMeta {
                                pubkey: Pubkey::new_from_array(a.pubkey),
                                is_signer: a.signer,
                                is_writable: a.writable,
                            })
                            .collect(),
                        data: ix.data.clone(),
                    })
                    .collect();
                let legs: Vec<String> = routed
                    .route
                    .iter()
                    .map(|leg| format!("{}:{}", leg.venue, Pubkey::new_from_array(leg.pool)))
                    .collect();
                let note = format!(
                    "nds routed in {}us via {} (state {}ms old, guaranteed out {})",
                    routed.compute_us,
                    legs.join(">"),
                    routed.state_age_ms,
                    routed.min_out
                );
                Ok(NdsAnswer::Routed(
                    Box::new(SwapInstructions {
                        // The instruction is Jupiter's own program's.
                        aggregator: Aggregator::Jupiter,
                        in_amount: request.amount,
                        min_out_amount: routed.min_out,
                        instructions,
                        address_lookup_tables: routed
                            .lookup_tables
                            .iter()
                            .map(|key| Pubkey::new_from_array(*key))
                            .collect(),
                    }),
                    note,
                ))
            }
            Response::NoRoute { reason, detail, .. } => {
                let reason = match reason {
                    NoRouteReason::Unsupported => "pair not covered",
                    NoRouteReason::TooStale => "pool state too stale",
                    NoRouteReason::NoLiquidity => "no liquidity",
                    NoRouteReason::Floor => "cannot clear the floor",
                    NoRouteReason::NoFit => "does not fit",
                    NoRouteReason::Deadline => "daemon past its deadline",
                    NoRouteReason::Internal => "daemon error",
                };
                Ok(NdsAnswer::Declined(format!("nds declined ({reason}: {detail})")))
            }
            other => anyhow::bail!("unexpected nds reply: {other:?}"),
        }
    }
}

#[cfg(all(test, unix, feature = "nds"))]
mod tests {
    use super::*;
    use nds_client::proto::{
        read_frame, write_frame, NoRouteReason, Request, Response, RouteLeg, Routed, WireAccount,
        WireInstruction, PROTOCOL_VERSION,
    };
    use std::os::unix::net::UnixListener;

    fn source(path: &std::path::Path) -> NdsSource {
        NdsSource {
            client: std::sync::Mutex::new(nds_client::Client::new(path, "test")),
            next_id: std::sync::atomic::AtomicU64::new(1),
            timeout: Duration::from_millis(500),
        }
    }

    fn request(min_out: u64) -> RouteRequest {
        RouteRequest::new(
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            1_000,
            min_out,
            50,
        )
    }

    /// A daemon that answers one connection: hello, then `reply` to the first request.
    fn daemon(reply: impl FnOnce(&nds_client::proto::RouteRequest) -> Response + Send + 'static) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("nds-test-{}.sock", Pubkey::new_unique()));
        let listener = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _: Option<Request> = read_frame(&mut stream).unwrap();
            write_frame(
                &mut stream,
                &Response::Hello { version: PROTOCOL_VERSION, min_version: PROTOCOL_VERSION, build: "t".into(), snapshot_slot: 1 },
            )
            .unwrap();
            if let Some(Request::Route(r)) = read_frame(&mut stream).unwrap() {
                write_frame(&mut stream, &reply(&r)).unwrap();
            }
        });
        path
    }

    fn routed(id: u64, min_out: u64) -> Response {
        Response::Routed(Routed {
            id,
            expected_out: min_out + 10,
            min_out,
            instructions: vec![WireInstruction {
                program: [7; 32],
                accounts: vec![WireAccount { pubkey: [8; 32], signer: true, writable: false }],
                data: vec![1, 2, 3],
            }],
            lookup_tables: vec![[9; 32]],
            route: vec![RouteLeg { venue: "orca".into(), pool: [5; 32], input_mint: [1; 32], output_mint: [2; 32], percent: 100 }],
            state_age_ms: 4,
            compute_us: 300,
            slot: 1,
        })
    }

    #[test]
    fn a_route_comes_back_as_swap_instructions() {
        let path = daemon(|r| routed(r.id, r.min_out + 5));
        let request = request(100);
        let NdsAnswer::Routed(swap, note) = source(&path).route(&request).unwrap() else {
            panic!("declined")
        };
        assert_eq!(swap.min_out_amount, 105);
        assert_eq!(swap.in_amount, 1_000);
        assert_eq!(swap.instructions.len(), 1);
        assert_eq!(swap.instructions[0].program_id, Pubkey::new_from_array([7; 32]));
        assert!(swap.instructions[0].accounts[0].is_signer);
        assert_eq!(swap.address_lookup_tables, vec![Pubkey::new_from_array([9; 32])]);
        assert!(note.contains("orca"), "{note}");
    }

    #[test]
    fn a_route_under_the_floor_is_not_used() {
        let path = daemon(|r| routed(r.id, r.min_out - 1));
        assert!(source(&path).route(&request(100)).is_err());
    }

    #[test]
    fn a_decline_is_reported_as_one() {
        let path = daemon(|r| Response::NoRoute { id: r.id, reason: NoRouteReason::Unsupported, detail: "x".into() });
        match source(&path).route(&request(100)).unwrap() {
            NdsAnswer::Declined(note) => assert!(note.contains("pair not covered"), "{note}"),
            _ => panic!("routed"),
        }
    }

    #[test]
    fn a_missing_socket_is_a_failure_to_answer() {
        let path = std::env::temp_dir().join("nds-test-missing.sock");
        assert!(source(&path).route(&request(100)).is_err());
    }
}
