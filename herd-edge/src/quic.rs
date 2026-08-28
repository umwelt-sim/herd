//! The endpoint this edge listens on.
//!
//! umwelt takes a bound `quinn::Endpoint` and never touches certificates or the
//! crypto provider, so both of those decisions are made here. See
//! `docs/adr/0006`.

/// A listening endpoint with a certificate generated for this run.
///
/// Self-signed, which is right for a demo on one machine and wrong for
/// anything else. A deployment builds its endpoint from whatever its operator
/// actually trusts and hands that over instead.
pub fn endpoint(addr: &str, runtime: &tokio::runtime::Handle) -> quinn::Endpoint {
    herd_common::provider();
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])
        .unwrap_or_else(|e| {
            eprintln!("generating a certificate: {e}");
            std::process::exit(1);
        });
    let chain = vec![cert.cert.der().clone()];
    let key = quinn::rustls::pki_types::PrivateKeyDer::try_from(
        cert.signing_key.serialize_der(),
    )
    .expect("a key rcgen just produced");

    let mut tls = quinn::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .unwrap_or_else(|e| {
            eprintln!("server tls: {e}");
            std::process::exit(1);
        });
    tls.alpn_protocols = vec![herd_common::ALPN.to_vec()];
    let tls = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
        .expect("a TLS 1.3 config");
    let config = quinn::ServerConfig::with_crypto(std::sync::Arc::new(tls));

    let addr: std::net::SocketAddr = addr.parse().unwrap_or_else(|e| {
        eprintln!("--edge {addr:?}: {e}");
        std::process::exit(1);
    });
    // A quinn endpoint spawns its own driver, so it has to be built inside the
    // runtime that will carry it.
    let _guard = runtime.enter();
    quinn::Endpoint::server(config, addr).unwrap_or_else(|e| {
        eprintln!("binding {addr}: {e}");
        std::process::exit(1);
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// The demo TLS setup, which is easy to get wrong in a way that only shows
    /// up at connect time. No broker involved: this is the client link alone.
    #[test]
    fn a_game_endpoint_reaches_an_edge_endpoint() {
        let runtime = tokio::runtime::Runtime::new().expect("a runtime");
        let edge = endpoint("127.0.0.1:0", runtime.handle());
        let at = edge.local_addr().expect("bound");
        let game = herd_common::game_endpoint(runtime.handle());

        runtime.block_on(async move {
            let served = tokio::spawn(async move {
                let conn = edge.accept().await.expect("a connection").await.expect("shakes");
                let (mut send, mut recv) = conn.accept_bi().await.expect("a stream");
                let mut got = [0u8; 5];
                recv.read_exact(&mut got).await.expect("five bytes");
                send.write_all(&got).await.expect("writes them back");
                // Held until the far end has read: dropping the connection, or
                // the endpoint behind it, closes it under the reader.
                tokio::time::sleep(Duration::from_millis(200)).await;
                got
            });

            let conn =
                game.connect(at, "localhost").expect("configured").await.expect("connects");
            let (mut send, mut recv) = conn.open_bi().await.expect("a stream");
            send.write_all(b"herd!").await.expect("writes");
            let mut back = [0u8; 5];
            recv.read_exact(&mut back).await.expect("five bytes back");
            assert_eq!(&back, b"herd!");
            assert_eq!(&served.await.expect("the edge side finished"), b"herd!");
        });
    }

    /// The one number that decides whether a region's payloads can reach a
    /// client at all, and the reason herd does not use umwelt's default.
    #[test]
    fn a_packet_at_herd_s_budget_fits_a_datagram() {
        let runtime = tokio::runtime::Runtime::new().expect("a runtime");
        let edge = endpoint("127.0.0.1:0", runtime.handle());
        let at = edge.local_addr().expect("bound");
        let game = herd_common::game_endpoint(runtime.handle());

        runtime.block_on(async move {
            let listening = tokio::spawn(async move {
                let conn = edge.accept().await.expect("a connection").await.expect("shakes");
                conn.read_datagram().await.map(|d| d.len())
            });
            let conn =
                game.connect(at, "localhost").expect("configured").await.expect("connects");
            let full = herd_common::PAYLOAD_BYTES as usize + 5;
            let room = conn.max_datagram_size().expect("datagrams are enabled");
            assert!(
                full <= room,
                "a full packet plus its header is {full} bytes against {room} of room"
            );
            conn.send_datagram(vec![0u8; full].into()).expect("fits");
            assert_eq!(listening.await.expect("joined").expect("read"), full);

            // And umwelt's default does not, which is the whole reason
            // PAYLOAD_BYTES exists.
            let too_big = umwelt::ClientLimits::default().payload_bytes as usize + 5;
            assert!(too_big > room, "umwelt's default now fits; PAYLOAD_BYTES can go");
        });
    }
}
