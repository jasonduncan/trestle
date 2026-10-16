//! Pilot's own commands, added to Trestle's built-in ones. `account` and
//! `mail` replace the built-ins: `account invite` can also grant a school
//! membership, and messages use pilot's product name.

use std::time::Duration;

use clap::{Subcommand, ValueEnum};
use trestle::{CommandContext, LinkArgs};
use trestle_core::identity::ActorId;
use trestle_identity::password;
use uuid::Uuid;

use crate::AppState;
use crate::mail::{MailArgs, PRODUCT};
use crate::provision;
use crate::schools::{Role, SchoolId};

#[derive(Subcommand)]
pub enum PilotCommand {
    /// Operator provisioning: schools and memberships (audited).
    Provision {
        #[command(subcommand)]
        action: ProvisionCommand,
    },
    /// Password accounts: invites, reset links, disable, enable, list.
    Account {
        #[command(subcommand)]
        action: AccountCommand,
    },
    /// Outgoing email.
    Mail {
        #[command(subcommand)]
        action: MailCommand,
    },
    /// Insert development schools and memberships (see `pilot::dev`).
    SeedDev,
}

#[derive(Subcommand)]
pub enum AccountCommand {
    /// Create an account and print a one-time invite link.
    Invite {
        #[arg(long)]
        email: String,
        /// Also make the account a member of this school.
        #[arg(long, requires = "role")]
        school: Option<Uuid>,
        #[arg(long, value_enum, requires = "school")]
        role: Option<RoleArg>,
        /// How long the link works.
        #[arg(long, default_value_t = 168, value_parser = clap::value_parser!(u32).range(1..=720))]
        valid_hours: u32,
        #[command(flatten)]
        link: LinkArgs,
    },
    /// Print a one-time password reset link. Earlier links stop working.
    ResetLink {
        #[arg(long)]
        email: String,
        #[arg(long, default_value_t = 24, value_parser = clap::value_parser!(u32).range(1..=720))]
        valid_hours: u32,
        #[command(flatten)]
        link: LinkArgs,
    },
    /// Stop an account signing in: ends its sessions and voids its links.
    Disable {
        #[arg(long)]
        email: String,
    },
    /// Allow a disabled account to sign in again.
    Enable {
        #[arg(long)]
        email: String,
    },
    /// Every account with its status.
    List,
}

#[derive(Subcommand)]
pub enum MailCommand {
    /// Send a test message with the configured backend.
    Test {
        #[arg(long)]
        to: String,
        #[command(flatten)]
        mail: MailArgs,
    },
}

#[derive(Subcommand)]
pub enum ProvisionCommand {
    /// Create a school (tenant); prints its ID.
    School {
        #[arg(long)]
        name: String,
    },
    /// Grant or change a member's role.
    Member {
        #[arg(long)]
        school: Uuid,
        #[arg(long)]
        actor: Uuid,
        #[arg(long, value_enum)]
        role: RoleArg,
    },
    /// Remove a member from a school.
    RemoveMember {
        #[arg(long)]
        school: Uuid,
        #[arg(long)]
        actor: Uuid,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum RoleArg {
    Owner,
    Teacher,
    Viewer,
}

impl From<RoleArg> for Role {
    fn from(role: RoleArg) -> Self {
        match role {
            RoleArg::Owner => Role::Owner,
            RoleArg::Teacher => Role::Teacher,
            RoleArg::Viewer => Role::Viewer,
        }
    }
}

pub async fn run(command: PilotCommand, ctx: CommandContext<AppState>) -> Result<(), String> {
    match command {
        PilotCommand::Account { action } => account(&ctx, action).await,
        PilotCommand::Mail {
            action: MailCommand::Test { to, mail },
        } => {
            let mailer = mail.mailer(ctx.env)?;
            mailer
                .send(&crate::mail::test_message(&to))
                .await
                .map_err(|e| e.to_string())?;
            println!("Sent a test message to {to} via {}.", mailer.describe());
            Ok(())
        }
        PilotCommand::Provision { action } => {
            let pool = ctx.connect().await?;
            let result = match action {
                ProvisionCommand::School { name } => provision::create_school(&pool, &name)
                    .await
                    .map(|id| println!("{id}")),
                ProvisionCommand::Member {
                    school,
                    actor,
                    role,
                } => {
                    provision::set_membership(&pool, SchoolId(school), ActorId(actor), role.into())
                        .await
                        .map(|()| {
                            println!("{actor} is now {} of {school}.", Role::from(role).as_str())
                        })
                }
                ProvisionCommand::RemoveMember { school, actor } => {
                    provision::remove_membership(&pool, SchoolId(school), ActorId(actor))
                        .await
                        .map(|removed| {
                            if removed {
                                println!("Removed {actor} from {school}.");
                            } else {
                                println!("{actor} was not a member of {school}.");
                            }
                        })
                }
            };
            result.map_err(|e| e.to_string())
        }
        PilotCommand::SeedDev => {
            if ctx.env.is_production() {
                return Err("seed-dev is not available in production".into());
            }
            let pool = ctx.connect().await?;
            crate::dev::seed(&pool).await.map_err(|e| e.to_string())?;
            println!("Seeded development schools and memberships.");
            Ok(())
        }
    }
}

async fn account(ctx: &CommandContext<AppState>, action: AccountCommand) -> Result<(), String> {
    let env = ctx.env;
    // Mail settings are checked before anything is created.
    let mailer = |link: &LinkArgs| link.send.then(|| link.mail.mailer(env)).transpose();
    let pool = ctx.connect().await?;
    let hours = |h: u32| Duration::from_secs(u64::from(h) * 3600);
    match action {
        AccountCommand::Invite {
            email,
            school,
            role,
            valid_hours,
            link,
        } => {
            let origin = link.origin()?;
            let mailer = mailer(&link)?;
            let issued = password::invite(&pool, &email, hours(valid_hours))
                .await
                .map_err(|e| e.to_string())?;
            if let (Some(school), Some(role)) = (school, role) {
                provision::set_membership(&pool, SchoolId(school), issued.actor, role.into())
                    .await
                    .map_err(|e| e.to_string())?;
                println!(
                    "{} is now {} of {school}.",
                    issued.email,
                    Role::from(role).as_str()
                );
            }
            trestle::deliver(PRODUCT, &issued, origin, mailer).await?;
        }
        AccountCommand::ResetLink {
            email,
            valid_hours,
            link,
        } => {
            let origin = link.origin()?;
            let mailer = mailer(&link)?;
            let issued = password::issue_reset(&pool, &email, hours(valid_hours))
                .await
                .map_err(|e| e.to_string())?;
            trestle::deliver(PRODUCT, &issued, origin, mailer).await?;
        }
        AccountCommand::Disable { email } => {
            let actor = password::set_disabled(&pool, &email, true)
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "Disabled {email} (actor {actor}); its sessions, links and API keys are void."
            );
        }
        AccountCommand::Enable { email } => {
            let actor = password::set_disabled(&pool, &email, false)
                .await
                .map_err(|e| e.to_string())?;
            println!("Enabled {email} (actor {actor}).");
        }
        AccountCommand::List => {
            for account in password::list(&pool).await.map_err(|e| e.to_string())? {
                println!(
                    "{:<9} {:<36} {} {}",
                    account.status, account.actor, account.created_at, account.email
                );
            }
        }
    }
    Ok(())
}
