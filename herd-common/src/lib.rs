//! What every herd binary shares.
//!
//! Argument parsing, the world all three peers agree on, and the two endpoints
//! a deployment has to build for itself. Deliberately outside umwelt: this is
//! what a consumer writes, not what the library provides.

use std::str::FromStr;

use umwelt::WorldConfig;

/// Both ends reach each other through NATS rather than through each other, so
/// this is the only address either needs.
pub const DEFAULT_NATS: &str = "nats://127.0.0.1:4222";

/// Largest payload herd asks a region to build.
///
/// **Not umwelt's default, which is 1200 and does not fit.** A packet reaches a
/// client on a QUIC datagram with a five-byte header in front of it, and quinn
/// offers about 1162 bytes at the usual path MTU and refuses anything larger
/// rather than fragmenting. A test below pins that, because the failure is
/// silent from the region's side: it builds a packet nobody can carry, and the
/// only sign is the edge's `undeliverable` counter.
pub const PAYLOAD_BYTES: u16 = 1_100;

/// What every herd client declares. See [`PAYLOAD_BYTES`].
pub fn limits() -> umwelt::ClientLimits {
    umwelt::ClientLimits {
        payload_bytes: PAYLOAD_BYTES,
        ..umwelt::ClientLimits::default()
    }
}

/// `--name value`, anywhere in the arguments.
pub fn arg(name: &str) -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == format!("--{name}") {
            return args.next();
        }
    }
    None
}

pub fn arg_or<T: FromStr>(name: &str, fallback: T) -> T {
    match arg(name) {
        Some(raw) => raw.parse().unwrap_or_else(|_| {
            eprintln!("--{name}: cannot read {raw:?}");
            std::process::exit(2);
        }),
        None => fallback,
    }
}

/// The default world at a chosen tick rate. Wire precision is lossless here, so
/// a position an edge sends comes back exactly.
pub fn world(tick_hz: u32) -> WorldConfig {
    WorldConfig::builder()
        .region_size_m(4096)
        .vertical_extent_m(1024)
        .horizontal_view_radius_m(256)
        .max_horizontal_speed_m_per_sec(40)
        .tick_hz(tick_hz)
        .build()
        .unwrap_or_else(|e| {
            eprintln!("world config: {e}");
            std::process::exit(2);
        })
}

/// Connects to NATS, with credentials if a path was given.
///
/// Both binaries own their connection rather than handing an address to the
/// library, so this is where a deployment's choices land: a comma-separated
/// server list for a cluster, a `.creds` file, and whatever else
/// `ConnectOptions` offers.
pub async fn connect(
    url: &str,
    creds: Option<String>,
) -> Result<async_nats::Client, Box<dyn std::error::Error + Send + Sync>> {
    let options = match creds {
        Some(path) => async_nats::ConnectOptions::with_credentials_file(path).await?,
        None => async_nats::ConnectOptions::new(),
    };
    let servers: Vec<async_nats::ServerAddr> =
        url.split(',').map(|one| one.trim().parse()).collect::<Result<_, _>>()?;
    Ok(async_nats::connect_with_options(servers, options).await?)
}

// ---------------------------------------------------------------------------
// QUIC, for the edge-to-client link
// ---------------------------------------------------------------------------

/// Where a `herd-edge` listens for `herd-game`, and where `herd-game` looks.
pub const DEFAULT_EDGE: &str = "127.0.0.1:7777";

/// What both ends agree they are speaking, so a stray QUIC client is refused
/// before it sends anything. The one value about the client link both ends must
/// hold identically, which is why it is here and the endpoints are not: the
/// edge builds the one that listens, the game builds the one that connects.
pub const ALPN: &[u8] = b"umwelt-herd";

/// Installs the crypto provider, once per process.
///
/// The library never does this. Installing a process-global default is a
/// deployment's decision and a library has no business making it, which is why
/// `EdgeServer::new` takes an endpoint that is already bound.
pub fn provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = quinn::rustls::crypto::ring::default_provider().install_default();
    });
}

/// A client endpoint that accepts whatever certificate the edge presents.
///
/// **For this demo only.** It turns off the check that says the edge is the
/// edge, which is exactly the check a deployment needs. A real client is built
/// against the roots its operator chose, and umwelt neither knows nor asks what
/// those are.
pub fn game_endpoint(runtime: &tokio::runtime::Handle) -> quinn::Endpoint {
    game_endpoint_with(runtime, None)
}

/// [`game_endpoint`], with the transport configured.
///
/// A test pins the MTU through this, so how large a datagram may be does not
/// depend on the host's loopback: quinn probes upward from 1200 by default, and
/// a loopback that allows 65536 answers very differently from one that does not.
pub fn game_endpoint_with(
    runtime: &tokio::runtime::Handle,
    transport: Option<std::sync::Arc<quinn::TransportConfig>>,
) -> quinn::Endpoint {
    provider();
    let mut tls = quinn::rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(std::sync::Arc::new(TrustAnything))
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let tls =
        quinn::crypto::rustls::QuicClientConfig::try_from(tls).expect("a TLS 1.3 config");

    let _guard = runtime.enter();
    let mut endpoint =
        quinn::Endpoint::client("0.0.0.0:0".parse().expect("a valid address"))
            .unwrap_or_else(|e| {
                eprintln!("binding a client socket: {e}");
                std::process::exit(1);
            });
    let mut client = quinn::ClientConfig::new(std::sync::Arc::new(tls));
    if let Some(transport) = transport {
        client.transport_config(transport);
    }
    endpoint.set_default_client_config(client);
    endpoint
}

/// Verifies nothing. See [`game_endpoint`].
#[derive(Debug)]
struct TrustAnything;

impl quinn::rustls::client::danger::ServerCertVerifier for TrustAnything {
    fn verify_server_cert(
        &self,
        _end_entity: &quinn::rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[quinn::rustls::pki_types::CertificateDer<'_>],
        _server_name: &quinn::rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: quinn::rustls::pki_types::UnixTime,
    ) -> Result<quinn::rustls::client::danger::ServerCertVerified, quinn::rustls::Error>
    {
        Ok(quinn::rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &quinn::rustls::pki_types::CertificateDer<'_>,
        _dss: &quinn::rustls::DigitallySignedStruct,
    ) -> Result<
        quinn::rustls::client::danger::HandshakeSignatureValid,
        quinn::rustls::Error,
    > {
        Ok(quinn::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &quinn::rustls::pki_types::CertificateDer<'_>,
        _dss: &quinn::rustls::DigitallySignedStruct,
    ) -> Result<
        quinn::rustls::client::danger::HandshakeSignatureValid,
        quinn::rustls::Error,
    > {
        Ok(quinn::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<quinn::rustls::SignatureScheme> {
        quinn::rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_world_is_valid_at_every_rate_a_peer_would_pick() {
        for hz in [10u32, 20, 50, 100] {
            let cfg = world(hz);
            assert_eq!(cfg.tick_hz(), hz);
            assert_eq!(cfg.region_size().floor_meters(), 4096);
            assert_eq!(cfg.horizontal_view_radius().floor_meters(), 256);
        }
    }

    #[test]
    fn all_three_peers_agree_on_the_world() {
        // A digest mismatch is how a region tells an edge they would decode
        // each other's packets into nonsense, so this is the check that the
        // three binaries cannot drift apart by building their own config.
        assert_eq!(world(20).protocol_hash(), world(20).protocol_hash());
        // Tick rate is carried beside the digest rather than inside it: it
        // changes how often a packet is built, not how one decodes.
        assert_eq!(world(20).protocol_hash(), world(50).protocol_hash());
    }

    #[test]
    fn every_client_declares_a_payload_that_fits_the_link() {
        // The number itself is pinned by a test in herd-edge, where the
        // datagram it has to fit is actually built.
        assert!(PAYLOAD_BYTES < umwelt::ClientLimits::default().payload_bytes);
        assert_eq!(limits().payload_bytes, PAYLOAD_BYTES);
    }
}
