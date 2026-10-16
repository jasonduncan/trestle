//! The SMTP backend against a small in-process SMTP server: plaintext to a
//! loopback catcher, STARTTLS that must not fall back to plaintext, and
//! implicit TLS with a pinned private CA (test certificates generated per
//! run).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use trestle_mail::{Email, MailSettings, Policy};

#[derive(Debug, Default, Clone)]
struct Transcript {
    auth: Option<String>,
    mail_from: Option<String>,
    rcpt: Vec<String>,
    data: Option<String>,
}

type Captured = Arc<Mutex<Vec<Transcript>>>;

/// Speaks just enough SMTP for one client session.
async fn converse<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    captured: Captured,
    offer_auth: bool,
) {
    let (read, mut write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read).lines();
    let mut transcript = Transcript::default();
    let _ = write.write_all(b"220 test.invalid ESMTP ready\r\n").await;
    while let Ok(Some(line)) = lines.next_line().await {
        let upper = line.to_ascii_uppercase();
        let reply: &[u8] = if upper.starts_with("EHLO") || upper.starts_with("HELO") {
            // STARTTLS is deliberately never offered.
            if offer_auth {
                b"250-test.invalid\r\n250 AUTH PLAIN LOGIN\r\n"
            } else {
                b"250 test.invalid\r\n"
            }
        } else if upper.starts_with("AUTH") {
            transcript.auth = Some(line.clone());
            b"235 2.7.0 Authentication successful\r\n"
        } else if upper.starts_with("MAIL FROM") {
            transcript.mail_from = Some(line.clone());
            b"250 OK\r\n"
        } else if upper.starts_with("RCPT TO") {
            transcript.rcpt.push(line.clone());
            b"250 OK\r\n"
        } else if upper == "DATA" {
            let _ = write
                .write_all(b"354 End data with <CR><LF>.<CR><LF>\r\n")
                .await;
            let mut data = String::new();
            while let Ok(Some(body)) = lines.next_line().await {
                if body == "." {
                    break;
                }
                data.push_str(&body);
                data.push('\n');
            }
            transcript.data = Some(data);
            b"250 OK queued\r\n"
        } else if upper == "QUIT" {
            let _ = write.write_all(b"221 Bye\r\n").await;
            break;
        } else {
            b"250 OK\r\n"
        };
        if write.write_all(reply).await.is_err() {
            break;
        }
    }
    captured.lock().unwrap().push(transcript);
}

async fn plain_server(offer_auth: bool) -> (u16, Captured) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let captured = Captured::default();
    let sink = captured.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(converse(stream, sink.clone(), offer_auth));
        }
    });
    (port, captured)
}

struct Pki {
    ca_pem: String,
    acceptor: TlsAcceptor,
}

/// A private CA and a certificate for `localhost` it signed.
fn pki() -> Pki {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let issuer = rcgen::Issuer::new(ca_params, ca_key);
    let leaf_key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec!["localhost".to_owned()])
        .unwrap()
        .signed_by(&leaf_key, &issuer)
        .unwrap();
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![leaf.der().clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(leaf_key.serialize_der().into()),
        )
        .unwrap();
    Pki {
        ca_pem: ca.pem(),
        acceptor: TlsAcceptor::from(Arc::new(config)),
    }
}

async fn tls_server(acceptor: TlsAcceptor) -> (u16, Captured) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let captured = Captured::default();
    let sink = captured.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            let sink = sink.clone();
            tokio::spawn(async move {
                if let Ok(tls) = acceptor.accept(stream).await {
                    converse(tls, sink, true).await;
                }
            });
        }
    });
    (port, captured)
}

fn email() -> Email {
    Email {
        to: "teacher@example.com".into(),
        subject: "Your invitation".into(),
        text: "Set your password:\nhttps://app.example.com/set-password#token=abc\n".into(),
    }
}

fn settings(url: String, ca_file: Option<PathBuf>) -> MailSettings {
    MailSettings {
        url,
        from: "Trestle Pilot <no-reply@example.com>".into(),
        ca_file,
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("trestle-mail-{name}-{}", uuid_like()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn uuid_like() -> String {
    format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

#[tokio::test]
async fn plaintext_to_a_loopback_catcher_delivers() {
    let (port, captured) = plain_server(false).await;
    let mailer = settings(format!("smtp+insecure://127.0.0.1:{port}"), None)
        .mailer(Policy::Development)
        .unwrap();
    mailer.send(&email()).await.unwrap();
    let transcript = captured.lock().unwrap()[0].clone();
    assert_eq!(
        transcript.mail_from.as_deref(),
        Some("MAIL FROM:<no-reply@example.com>")
    );
    assert_eq!(transcript.rcpt, ["RCPT TO:<teacher@example.com>"]);
    let data = transcript.data.unwrap();
    assert!(data.contains("Subject: Your invitation"), "{data}");
    assert!(
        data.contains("From: \"Trestle Pilot\" <no-reply@example.com>"),
        "{data}"
    );
    assert!(data.contains("To: teacher@example.com"), "{data}");
    assert!(data.contains("Content-Type: text/plain"), "{data}");
    assert!(data.contains("set-password#token=abc"), "{data}");
    assert!(transcript.auth.is_none());
}

#[tokio::test]
async fn starttls_is_required_and_never_falls_back_to_plaintext() {
    // The server offers no STARTTLS; the client must refuse to continue.
    let (port, captured) = plain_server(true).await;
    let mailer = settings(format!("smtp://mailer:s3cret@127.0.0.1:{port}"), None)
        .mailer(Policy::Production)
        .unwrap();
    let err = mailer.send(&email()).await.unwrap_err();
    assert!(
        err.to_string().contains("mail server did not accept"),
        "{err}"
    );
    let transcripts = captured.lock().unwrap().clone();
    assert!(
        transcripts
            .iter()
            .all(|t| t.auth.is_none() && t.data.is_none()),
        "neither credentials nor the message crossed in plaintext: {transcripts:?}"
    );
}

#[tokio::test]
async fn implicit_tls_with_a_pinned_ca_delivers_and_authenticates() {
    let pki = pki();
    let (port, captured) = tls_server(pki.acceptor.clone()).await;
    let dir = scratch("ca");
    let ca_file = dir.join("ca.pem");
    std::fs::write(&ca_file, &pki.ca_pem).unwrap();
    let mailer = settings(
        format!("smtps://mailer:s3cret@localhost:{port}"),
        Some(ca_file),
    )
    .mailer(Policy::Production)
    .unwrap();
    mailer.send(&email()).await.unwrap();
    let transcript = captured.lock().unwrap()[0].clone();
    assert!(
        transcript.auth.unwrap().starts_with("AUTH PLAIN"),
        "credentials sent inside TLS"
    );
    assert!(
        transcript
            .data
            .unwrap()
            .contains("Subject: Your invitation")
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn an_untrusted_server_certificate_is_refused() {
    let pki = pki();
    let other = pki_ca_only();
    let (port, captured) = tls_server(pki.acceptor).await;
    let dir = scratch("other-ca");
    let ca_file = dir.join("ca.pem");
    std::fs::write(&ca_file, other).unwrap();
    let mailer = settings(
        format!("smtps://mailer:s3cret@localhost:{port}"),
        Some(ca_file),
    )
    .mailer(Policy::Production)
    .unwrap();
    assert!(mailer.send(&email()).await.is_err());
    assert!(
        captured.lock().unwrap().is_empty(),
        "no SMTP conversation took place"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// A CA that signed nothing the server presents.
fn pki_ca_only() -> String {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.self_signed(&key).unwrap().pem()
}

#[tokio::test]
async fn the_file_backend_writes_one_message_per_file() {
    let dir = scratch("files");
    let url = url::Url::from_file_path(&dir).unwrap().to_string();
    let mailer = settings(url, None).mailer(Policy::Development).unwrap();
    mailer.send(&email()).await.unwrap();
    mailer.send(&email()).await.unwrap();
    let files: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
    assert_eq!(files.len(), 2);
    let written = std::fs::read_to_string(files[0].as_ref().unwrap().path()).unwrap();
    assert!(written.contains("Subject: Your invitation"));
    assert!(written.contains("To: teacher@example.com"));
    std::fs::remove_dir_all(dir).unwrap();
}
