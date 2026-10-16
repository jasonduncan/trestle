//! Outgoing email for Trestle.
//!
//! A [`Mailer`] sends one plain-text [`Email`]. Backends are chosen by URL,
//! like Django's `EMAIL_BACKEND` settings:
//!
//! | URL | Backend |
//! |---|---|
//! | `smtps://user:pass@host[:465]` | SMTP over TLS from the first byte |
//! | `smtp://user:pass@host[:587]` | SMTP with STARTTLS **required** (never falls back to plaintext) |
//! | `smtp+insecure://localhost:1025` | plaintext SMTP to a loopback mail catcher; development only, no credentials |
//! | `file:///absolute/dir` | one `.eml` file per message; development only |
//!
//! `TRESTLE_EMAIL_CA_FILE` pins a private CA for the SMTP server: only
//! that CA is then trusted. Without it the system trust store is used.
//! Email services with their own APIs are future backends behind the same
//! trait.
//!
//! Messages are never logged: they carry sign-in links.

#![forbid(unsafe_code)]

use std::future::Future;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use lettre::message::header::ContentType;
use lettre::message::{Mailbox, Message};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Certificate, CertificateStore, Tls, TlsParameters};
use lettre::{Address, AsyncSmtpTransport, AsyncTransport, Tokio1Executor};
use url::Url;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One plain-text message to one recipient.
#[derive(Debug, Clone)]
pub struct Email {
    pub to: String,
    pub subject: String,
    pub text: String,
}

#[derive(Debug, thiserror::Error)]
pub enum MailError {
    #[error("{0:?} is not a valid email address")]
    Address(String),
    #[error("could not build the message: {0}")]
    Message(String),
    #[error("the mail server did not accept the message: {0}")]
    Transport(String),
    #[error("could not write the message: {0}")]
    Io(String),
}

/// Sends email. Dyn-compatible so the composition root can choose a
/// backend from configuration.
pub trait Mailer: Send + Sync + 'static {
    fn send<'a>(&'a self, email: &'a Email) -> BoxFuture<'a, Result<(), MailError>>;

    /// Where mail goes, without secrets, for logs and operator output.
    fn describe(&self) -> String;
}

/// How strict configuration checks are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Encrypted SMTP only.
    Production,
    /// Also plaintext SMTP to loopback and the file backend.
    Development,
}

#[derive(Debug, Clone)]
pub struct MailSettings {
    pub url: String,
    pub from: String,
    pub ca_file: Option<PathBuf>,
}

impl MailSettings {
    /// Every problem with these settings under `policy`, all at once.
    pub fn problems(&self, policy: Policy) -> Vec<String> {
        let mut problems = Vec::new();
        if let Err(e) = Mailbox::from_str(&self.from) {
            problems.push(format!(
                "TRESTLE_EMAIL_FROM {:?} is not a mailbox: {e}",
                self.from
            ));
        }
        match parse(&self.url, policy) {
            Ok(Target::File(_)) if self.ca_file.is_some() => {
                problems.push("TRESTLE_EMAIL_CA_FILE applies only to SMTP".into());
            }
            Ok(_) => {}
            Err(e) => problems.push(format!("TRESTLE_EMAIL_URL: {e}")),
        }
        problems
    }

    /// The configured mailer. Fails with the same problems as
    /// [`MailSettings::problems`], or if the CA file cannot be read.
    pub fn mailer(&self, policy: Policy) -> Result<Arc<dyn Mailer>, String> {
        let problems = self.problems(policy);
        if !problems.is_empty() {
            return Err(problems.join("; "));
        }
        let from = Mailbox::from_str(&self.from).map_err(|e| e.to_string())?;
        match parse(&self.url, policy)? {
            Target::File(dir) => Ok(Arc::new(FileMailer { dir, from })),
            Target::Smtp(smtp) => {
                let ca = match &self.ca_file {
                    Some(path) => Some(read_ca(path)?),
                    None => None,
                };
                Ok(Arc::new(SmtpMailer::new(smtp, from, ca)?))
            }
        }
    }
}

fn read_ca(path: &Path) -> Result<Certificate, String> {
    let pem = std::fs::read(path)
        .map_err(|e| format!("TRESTLE_EMAIL_CA_FILE {}: {e}", path.display()))?;
    Certificate::from_pem(&pem)
        .map_err(|e| format!("TRESTLE_EMAIL_CA_FILE {}: {e}", path.display()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Security {
    /// TLS from the first byte (port 465).
    Implicit,
    /// STARTTLS, required.
    StartTls,
    /// Plaintext, loopback only.
    None,
}

#[derive(Debug, Clone)]
struct Smtp {
    host: String,
    port: u16,
    security: Security,
    credentials: Option<(String, String)>,
}

enum Target {
    Smtp(Smtp),
    File(PathBuf),
}

fn loopback(url: &Url) -> bool {
    // Non-special schemes (smtp+insecure) keep hosts opaque, so an IP
    // address arrives as a domain string.
    match url.host() {
        Some(url::Host::Domain(d)) => {
            d == "localhost"
                || d.trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        }
        Some(url::Host::Ipv4(ip)) => IpAddr::V4(ip).is_loopback(),
        Some(url::Host::Ipv6(ip)) => IpAddr::V6(ip).is_loopback(),
        None => false,
    }
}

fn decode(value: &str) -> Result<String, String> {
    url::form_urlencoded::parse(format!("v={value}").as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .ok_or_else(|| "could not decode credentials".to_owned())
}

fn parse(raw: &str, policy: Policy) -> Result<Target, String> {
    let url = Url::parse(raw).map_err(|e| format!("not a URL: {e}"))?;
    if url.query().is_some() || url.fragment().is_some() {
        return Err("query strings and fragments are not supported".into());
    }
    let credentials = if url.username().is_empty() {
        None
    } else {
        let password = url
            .password()
            .ok_or("SMTP credentials need a password (user:password@host)")?;
        Some((decode(url.username())?, decode(password)?))
    };
    let smtp = |security: Security, default_port: u16| -> Result<Target, String> {
        let host = url
            .host_str()
            .filter(|h| !h.is_empty())
            .ok_or("the URL needs a host")?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        Ok(Target::Smtp(Smtp {
            host,
            port: url.port().unwrap_or(default_port),
            security,
            credentials: credentials.clone(),
        }))
    };
    match url.scheme() {
        "smtps" => smtp(Security::Implicit, 465),
        "smtp" => smtp(Security::StartTls, 587),
        "smtp+insecure" => {
            if policy == Policy::Production {
                return Err("plaintext SMTP is not allowed in production; use smtps:// or smtp:// (STARTTLS)".into());
            }
            if !loopback(&url) {
                return Err("plaintext SMTP is only for a loopback mail catcher".into());
            }
            if credentials.is_some() {
                return Err("never send SMTP credentials in plaintext".into());
            }
            smtp(Security::None, 25)
        }
        "file" => {
            if policy == Policy::Production {
                return Err("the file backend is for development; production needs SMTP".into());
            }
            let dir = url
                .to_file_path()
                .map_err(|_| "file:// needs an absolute path")?;
            Ok(Target::File(dir))
        }
        other => Err(format!(
            "unsupported scheme {other:?}; use smtps://, smtp:// or (development) smtp+insecure:// or file://"
        )),
    }
}

fn message(from: &Mailbox, email: &Email) -> Result<Message, MailError> {
    let to =
        Address::from_str(email.to.trim()).map_err(|_| MailError::Address(email.to.clone()))?;
    Message::builder()
        .from(from.clone())
        .to(Mailbox::new(None, to))
        .subject(email.subject.clone())
        .header(ContentType::TEXT_PLAIN)
        .body(email.text.clone())
        .map_err(|e| MailError::Message(e.to_string()))
}

/// SMTP through lettre, with rustls (aws-lc-rs).
pub struct SmtpMailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
    described: String,
}

impl SmtpMailer {
    fn new(smtp: Smtp, from: Mailbox, ca: Option<Certificate>) -> Result<Self, String> {
        let tls = |host: &str| -> Result<TlsParameters, String> {
            let mut builder = TlsParameters::builder(host.to_owned());
            if let Some(ca) = ca.clone() {
                builder = builder
                    .certificate_store(CertificateStore::None)
                    .add_root_certificate(ca);
            }
            builder
                .build_rustls()
                .map_err(|e| format!("TLS setup: {e}"))
        };
        let tls = match smtp.security {
            Security::Implicit => Tls::Wrapper(tls(&smtp.host)?),
            Security::StartTls => Tls::Required(tls(&smtp.host)?),
            Security::None => Tls::None,
        };
        let mut builder =
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(smtp.host.clone())
                .port(smtp.port)
                .tls(tls)
                .timeout(Some(Duration::from_secs(20)));
        if let Some((user, password)) = &smtp.credentials {
            builder = builder.credentials(Credentials::new(user.clone(), password.clone()));
        }
        let scheme = match smtp.security {
            Security::Implicit => "smtps",
            Security::StartTls => "smtp (STARTTLS)",
            Security::None => "smtp+insecure",
        };
        let who = smtp
            .credentials
            .as_ref()
            .map(|(user, _)| format!(" as {user}"))
            .unwrap_or_default();
        Ok(Self {
            transport: builder.build(),
            from,
            described: format!("{scheme} {}:{}{who}", smtp.host, smtp.port),
        })
    }
}

impl Mailer for SmtpMailer {
    fn send<'a>(&'a self, email: &'a Email) -> BoxFuture<'a, Result<(), MailError>> {
        Box::pin(async move {
            let message = message(&self.from, email)?;
            self.transport
                .send(message)
                .await
                .map(|_| ())
                .map_err(|e| MailError::Transport(e.to_string()))
        })
    }

    fn describe(&self) -> String {
        self.described.clone()
    }
}

/// Development backend: each message is written to `<dir>/<id>.eml`.
pub struct FileMailer {
    dir: PathBuf,
    from: Mailbox,
}

impl Mailer for FileMailer {
    fn send<'a>(&'a self, email: &'a Email) -> BoxFuture<'a, Result<(), MailError>> {
        Box::pin(async move {
            let message = message(&self.from, email)?;
            tokio::fs::create_dir_all(&self.dir)
                .await
                .map_err(|e| MailError::Io(e.to_string()))?;
            let path = self
                .dir
                .join(format!("{}.eml", uuid::Uuid::now_v7().simple()));
            tokio::fs::write(&path, message.formatted())
                .await
                .map_err(|e| MailError::Io(e.to_string()))
        })
    }

    fn describe(&self) -> String {
        format!("files in {}", self.dir.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(url: &str) -> MailSettings {
        MailSettings {
            url: url.into(),
            from: "Trestle Pilot <no-reply@example.com>".into(),
            ca_file: None,
        }
    }

    #[test]
    fn production_accepts_only_encrypted_smtp() {
        for ok in [
            "smtps://mailer:s3cret@smtp.example.com",
            "smtp://mailer:s3cret@smtp.example.com:2525",
            "smtps://relay.internal",
        ] {
            assert!(settings(ok).problems(Policy::Production).is_empty(), "{ok}");
        }
        for (bad, why) in [
            (
                "smtp+insecure://localhost:1025",
                "plaintext SMTP is not allowed in production",
            ),
            ("file:///tmp/mail", "the file backend is for development"),
            ("http://smtp.example.com", "unsupported scheme"),
            ("smtps://smtp.example.com?tls=none", "query strings"),
            ("smtps://mailer@smtp.example.com", "need a password"),
        ] {
            let problems = settings(bad).problems(Policy::Production).join("; ");
            assert!(problems.contains(why), "{bad}: {problems}");
        }
    }

    #[test]
    fn development_allows_a_loopback_catcher_without_credentials() {
        assert!(
            settings("smtp+insecure://localhost:1025")
                .problems(Policy::Development)
                .is_empty()
        );
        assert!(
            settings("smtp+insecure://127.0.0.1:1025")
                .problems(Policy::Development)
                .is_empty()
        );
        assert!(
            settings("file:///tmp/trestle-mail")
                .problems(Policy::Development)
                .is_empty()
        );
        let remote = settings("smtp+insecure://smtp.example.com").problems(Policy::Development);
        assert!(remote[0].contains("loopback"), "{remote:?}");
        let creds = settings("smtp+insecure://u:p@localhost:1025").problems(Policy::Development);
        assert!(
            creds[0].contains("never send SMTP credentials in plaintext"),
            "{creds:?}"
        );
    }

    #[test]
    fn the_sender_must_be_a_mailbox() {
        let mut bad = settings("smtps://smtp.example.com");
        bad.from = "not an address".into();
        assert!(bad.problems(Policy::Production)[0].contains("TRESTLE_EMAIL_FROM"));
    }

    #[test]
    fn credentials_are_percent_decoded_and_never_described() {
        let Target::Smtp(smtp) = parse(
            "smtps://mail%40example.com:p%40ss%2Fw0rd@smtp.example.com",
            Policy::Production,
        )
        .unwrap() else {
            panic!("smtp");
        };
        assert_eq!(
            smtp.credentials,
            Some(("mail@example.com".into(), "p@ss/w0rd".into()))
        );
        let mailer = settings("smtps://mailer:s3cret@smtp.example.com")
            .mailer(Policy::Production)
            .unwrap();
        assert_eq!(mailer.describe(), "smtps smtp.example.com:465 as mailer");
        assert!(!mailer.describe().contains("s3cret"));
    }

    #[test]
    fn recipients_cannot_inject_headers() {
        let from = Mailbox::from_str("a@example.com").unwrap();
        for to in [
            "b@example.com\r\nBcc: c@example.com",
            "b@example.com, c@example.com",
            "nobody",
        ] {
            let email = Email {
                to: to.into(),
                subject: "s".into(),
                text: "t".into(),
            };
            assert!(
                matches!(message(&from, &email), Err(MailError::Address(_))),
                "{to:?}"
            );
        }
    }
}
