// QUIC interop spike: quinn client -> Network.framework server, pinning the cert's SHA-256.
use std::{sync::Arc, time::Instant};

use quinn::crypto::rustls::QuicClientConfig;
use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
    DigitallySignedStruct, SignatureScheme,
};
use sha2::{Digest, Sha256};

#[derive(Debug)]
struct Pinned(Vec<u8>, Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self, end: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let got = Sha256::digest(end.as_ref());
        if got.as_slice() == self.0.as_slice() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!("fingerprint mismatch: {}", hex::encode(got))))
        }
    }
    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.1.signature_verification_algorithms)
    }
    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.1.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.1.signature_verification_algorithms.supported_schemes()
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let addr = args.next().expect("addr").parse()?;
    let fp = hex::decode(args.next().expect("fingerprint"))?;

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned(fp, provider)))
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"sweeflow/0".to_vec()];

    let mut transport = quinn::TransportConfig::default();
    transport.datagram_receive_buffer_size(Some(8 << 20));
    let mut cfg = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    cfg.transport_config(Arc::new(transport));

    let mut ep = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    ep.set_default_client_config(cfg);

    let t0 = Instant::now();
    let conn = ep.connect(addr, "sweeflow")?.await?;
    println!("connected in {:?}, max datagram {:?}", t0.elapsed(), conn.max_datagram_size());

    let (mut tx, mut rx) = conn.open_bi().await?;
    let t1 = Instant::now();
    tx.write_all(b"hello from linux").await?;
    let mut buf = [0u8; 256];
    let n = rx.read(&mut buf).await?.unwrap_or(0);
    println!("stream rtt {:?}: {}", t1.elapsed(), String::from_utf8_lossy(&buf[..n]));

    conn.send_datagram(vec![7u8; 1000].into())?;
    let mut got = 0;
    let deadline = tokio::time::sleep(std::time::Duration::from_secs(3));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            d = conn.read_datagram() => { let d = d?; got += 1; if got == 1 || got == 100 { println!("datagram #{} len {} seq {}", got, d.len(), d[0]); } }
            _ = &mut deadline => break,
        }
    }
    println!("received {got}/100 datagrams; rtt estimate {:?}", conn.rtt());
    Ok(())
}
