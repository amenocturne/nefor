use std::sync::Arc;

use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::net::TcpListener;
use tokio_rustls::{rustls, TlsAcceptor};

struct TestAuthority {
    ca_der: CertificateDer<'static>,
    server_der: CertificateDer<'static>,
    server_key: PrivateKeyDer<'static>,
}

fn authority(hostname: &str) -> TestAuthority {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let ca_key = KeyPair::generate().expect("CA key");
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = ca_params.self_signed(&ca_key).expect("CA certificate");

    let server_key = KeyPair::generate().expect("server key");
    let server = CertificateParams::new(vec![hostname.to_owned()])
        .expect("server params")
        .signed_by(&server_key, &ca, &ca_key)
        .expect("server certificate");
    TestAuthority {
        ca_der: CertificateDer::from(ca.der().to_vec()),
        server_der: CertificateDer::from(server.der().to_vec()),
        server_key: PrivatePkcs8KeyDer::from(server_key.serialize_der()).into(),
    }
}

async fn serve_once(authority: TestAuthority) -> std::net::SocketAddr {
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![authority.server_der], authority.server_key)
        .expect("server TLS config");
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        if let Ok(mut stream) = acceptor.accept(stream).await {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await;
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                .await;
        }
    });
    address
}

#[tokio::test]
async fn private_native_root_is_additive() {
    let authority = authority("localhost");
    let ca = authority.ca_der.clone();
    let address = serve_once(authority).await;
    let client = nefor_provider_http::add_native_roots(reqwest::Client::builder(), vec![ca])
        .expect("native root")
        .build()
        .expect("client");

    let response = client
        .get(format!("https://localhost:{}/", address.port()))
        .send()
        .await
        .expect("private CA is trusted");
    assert_eq!(response.text().await.expect("body"), "ok");
}

#[tokio::test]
async fn wrong_ca_still_fails() {
    let server_authority = authority("localhost");
    let address = serve_once(server_authority).await;
    let wrong_ca = authority("localhost").ca_der;
    let client = nefor_provider_http::add_native_roots(reqwest::Client::builder(), vec![wrong_ca])
        .expect("native root")
        .build()
        .expect("client");

    assert!(client
        .get(format!("https://localhost:{}/", address.port()))
        .send()
        .await
        .is_err());
}

#[tokio::test]
async fn wrong_hostname_still_fails() {
    let authority = authority("not-localhost.invalid");
    let ca = authority.ca_der.clone();
    let address = serve_once(authority).await;
    let client = nefor_provider_http::add_native_roots(reqwest::Client::builder(), vec![ca])
        .expect("native root")
        .build()
        .expect("client");

    assert!(client
        .get(format!("https://localhost:{}/", address.port()))
        .send()
        .await
        .is_err());
}
