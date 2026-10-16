//! Email the pilot sends, and its configuration.
//!
//! Only operator commands send mail so far (`pilot account invite --send`,
//! `pilot account reset-link --send`, `pilot mail test`). The API server
//! sends none: self-service password reset would send from a request, and
//! reliable delivery from there needs the outbox deferred in ADR-009.

use trestle_identity::password::Issued;
use trestle_mail::Email;

pub const PRODUCT: &str = "Trestle Pilot";

pub use trestle::mail::MailArgs;

/// The invite or reset message for `issued`, linking to `link`.
pub fn account_link(issued: &Issued, link: &str) -> Email {
    trestle::mail::account_link(PRODUCT, issued, link)
}

/// `pilot mail test`: proves the configuration end to end.
pub fn test_message(to: &str) -> Email {
    trestle::mail::test_message(PRODUCT, to)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Environment;
    use trestle_core::identity::ActorId;
    use trestle_identity::password::Purpose;
    use uuid::Uuid;

    #[test]
    fn messages_name_the_account_and_carry_the_link() {
        let issued = Issued {
            email: "teacher@example.com".into(),
            actor: ActorId(Uuid::nil()),
            purpose: Purpose::Reset,
            token: "t".repeat(43),
            expires_at: "2026-09-27T12:00:00Z".into(),
            created_account: false,
        };
        let link = issued.link("https://app.example.com");
        let email = account_link(&issued, &link);
        assert_eq!(email.to, "teacher@example.com");
        assert_eq!(email.subject, "Reset your Trestle Pilot password");
        assert!(email.text.contains(&link));
        assert!(email.text.contains("2026-09-27T12:00:00Z"));
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
