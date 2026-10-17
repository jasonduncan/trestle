//! `schools.mine`: the caller's own memberships, so a signed-in browser can
//! offer a school picker. It needs only an authenticated actor, and its
//! query is scoped to that actor: it can never list another person's
//! schools, and it grants nothing (the tenant header is still checked
//! against membership on every request).

use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use trestle_core::Problem;
use trestle_core::identity::ActorId;
use trestle_core::invocation::{FromInvocation, Invocation};
use trestle_core::operation::{
    ContextDescriptor, EffectsInfo, Mode, Operation, OperationInfo, Policy, RequestContext,
};
use trestle_core::pagination::{MAX_PAGE, Page};
use trestle_core::protocol::{Audience, Contract, ContractDoc, Field};
use uuid::Uuid;

use crate::AppState;

/// An authenticated caller with no tenant selected.
#[derive(Debug, Clone, Copy)]
pub struct ActorContext {
    pub actor: ActorId,
}

impl RequestContext for ActorContext {
    fn describe() -> ContextDescriptor {
        ContextDescriptor::authenticated()
    }
}

impl FromInvocation<AppState> for ActorContext {
    async fn resolve(invocation: &Invocation, _: &AppState) -> Result<Self, Problem> {
        Ok(ActorContext {
            actor: invocation.actor()?,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MySchoolsInput {}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct MembershipView {
    /// The school's ID (the value for `Trestle-Tenant`).
    pub school: Uuid,
    pub name: String,
    pub role: String,
}

impl Contract for MySchoolsInput {
    fn contract() -> &'static ContractDoc {
        static CONTRACT: LazyLock<ContractDoc> =
            LazyLock::new(|| ContractDoc::input("schools.mine.input", "1.0.0"));
        &CONTRACT
    }
}

impl Contract for MembershipView {
    fn contract() -> &'static ContractDoc {
        static CONTRACT: LazyLock<ContractDoc> = LazyLock::new(|| {
            ContractDoc::output("schools.membership", "1.0.0")
                .field(Field::uuid("school"))
                .field(Field::text("name").chars(1, 200))
                .field(Field::enumeration("role", &["owner", "teacher", "viewer"]))
        });
        &CONTRACT
    }
}

/// Every authenticated caller may list their own memberships.
pub struct OwnMemberships;

impl Policy<ActorContext> for OwnMemberships {
    fn id(&self) -> &'static str {
        "schools.own_memberships"
    }

    fn admit(&self, _ctx: &ActorContext) -> Result<(), Problem> {
        Ok(())
    }

    async fn recheck(&self, _conn: &mut PgConnection, _ctx: &ActorContext) -> Result<(), Problem> {
        Ok(())
    }
}

pub struct MySchools;

impl Operation for MySchools {
    type Context = ActorContext;
    type Input = MySchoolsInput;
    type Output = Page<MembershipView>;
    type Policy = OwnMemberships;

    const INFO: OperationInfo = OperationInfo {
        id: "schools.mine",
        version: "1.0.0",
        mode: Mode::Read,
        audiences: &[Audience::Product, Audience::Admin],
        summary: "The caller's schools and roles, by name; at most 100.",
    };

    const EFFECTS: Option<EffectsInfo> =
        Some(EffectsInfo::new().reads(&["schools.membership", "schools.school"]));

    fn policy(&self) -> &OwnMemberships {
        &OwnMemberships
    }

    async fn run(
        &self,
        conn: &mut PgConnection,
        ctx: &ActorContext,
        _input: MySchoolsInput,
    ) -> Result<Page<MembershipView>, Problem> {
        let items: Vec<MembershipView> = sqlx::query_as(
            "SELECT m.school_id AS school, s.name, m.role \
             FROM school_memberships m JOIN schools s ON s.id = m.school_id \
             WHERE m.actor_id = $1 ORDER BY s.name, m.school_id LIMIT $2",
        )
        .bind(ctx.actor.0)
        .bind(i64::from(MAX_PAGE))
        .fetch_all(conn)
        .await?;
        Ok(Page {
            items,
            next_cursor: None,
        })
    }
}
