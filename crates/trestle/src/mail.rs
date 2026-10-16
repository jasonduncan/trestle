//! Outgoing email configuration and the account messages the built-in
//! `account` commands send.

use std::path::PathBuf;
use std::sync::Arc;

use trestle_identity::password::{Issued, Purpose};
use trestle_mail::{Email, MailSettings, Mailer, Policy};

use crate::Environment;

/// `Debug` hides the URL, which may carry SMTP credentials.
#[derive(clap::Args, Clone, Default)]
pub struct MailArgs {
    /// smtps://user:pass@host (TLS), smtp://user:pass@host (STARTTLS
    /// required); in development also smtp+insecure://localhost:PORT or
    /// file:///dir. Percent-encode special characters in credentials.
    #[arg(long = "email-url", env = "TRESTLE_EMAIL_URL", hide_env_values = true)]
    pub url: Option<String>,
    /// Sender, e.g. "My App <no-reply@example.com>".
    #[arg(long = "email-from", env = "TRESTLE_EMAIL_FROM")]
    pub from: Option<String>,
    /// PEM CA certificate to trust (only it) for the SMTP server.
    #[arg(long = "email-ca-file", env = "TRESTLE_EMAIL_CA_FILE")]
    pub ca_file: Option<PathBuf>,
}

impl std::fmt::Debug for MailArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MailArgs")
            .field("url", &self.url.as_ref().map(|_| "<redacted>"))
            .field("from", &self.from)
            .field("ca_file", &self.ca_file)
            .finish()
    }
}

impl MailArgs {
    pub fn mailer(&self, env: Environment) -> Result<Arc<dyn Mailer>, String> {
        let url = self
            .url
            .clone()
            .ok_or("email is not configured: set TRESTLE_EMAIL_URL and TRESTLE_EMAIL_FROM")?;
        let from = self.from.clone().ok_or("TRESTLE_EMAIL_FROM is not set")?;
        let policy = if env.is_production() {
            Policy::Production
        } else {
            Policy::Development
        };
        MailSettings {
            url,
            from,
            ca_file: self.ca_file.clone(),
        }
        .mailer(policy)
    }
}

/// The invite or reset message for `issued`, naming `product`.
pub fn account_link(product: &str, issued: &Issued, link: &str) -> Email {
    let (subject, text) = match issued.purpose {
        Purpose::Invite => (
            format!("You're invited to {product}"),
            format!(
                "Hello,\n\n\
                 An administrator created a {product} account for {email}.\n\
                 Choose your password here. The link works once and expires {expires}.\n\n\
                 {link}\n\n\
                 If you weren't expecting this, you can ignore this email.\n",
                email = issued.email,
                expires = issued.expires_at,
            ),
        ),
        Purpose::Reset => (
            format!("Reset your {product} password"),
            format!(
                "Hello,\n\n\
                 An administrator issued a password reset for {email}.\n\
                 Choose a new password here. The link works once and expires {expires}.\n\
                 Setting it signs you out everywhere else.\n\n\
                 {link}\n\n\
                 If you didn't ask for this, tell your administrator.\n",
                email = issued.email,
                expires = issued.expires_at,
            ),
        ),
    };
    Email {
        to: issued.email.clone(),
        subject,
        text,
    }
}

/// `mail test`: proves the configuration end to end.
pub fn test_message(product: &str, to: &str) -> Email {
    Email {
        to: to.to_owned(),
        subject: format!("Test email from {product}"),
        text: format!(
            "This is a test message from `mail test`.\n\
             If you can read it, {product} can send email.\n"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_the_email_url() {
        let args = MailArgs {
            url: Some("smtps://user:hunter2-secret@mail.example.com".into()),
            from: Some("App <no-reply@example.com>".into()),
            ca_file: None,
        };
        let shown = format!("{args:?}");
        assert!(!shown.contains("hunter2-secret"), "{shown}");
        assert!(shown.contains("no-reply@example.com"), "{shown}");
    }
    use trestle_core::identity::ActorId;
    use uuid::Uuid;

    #[test]
    fn messages_name_the_product_and_carry_the_link() {
        let issued = Issued {
            email: "someone@example.com".into(),
            actor: ActorId(Uuid::nil()),
            purpose: Purpose::Reset,
            token: "t".repeat(43),
            expires_at: "2026-09-27T12:00:00Z".into(),
            created_account: false,
        };
        let link = issued.link("https://app.example.com");
        let email = account_link("Notes", &issued, &link);
        assert_eq!(email.subject, "Reset your Notes password");
        assert!(email.text.contains(&link));
    }

    #[test]
    fn sending_needs_configuration() {
        let err = MailArgs::default()
            .mailer(Environment::Development)
            .err()
            .unwrap();
        assert!(err.contains("TRESTLE_EMAIL_URL"));
    }
}
