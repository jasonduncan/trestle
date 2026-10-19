//! Explicit membership discovery for the starter's workspace tenancy.
//! This read grants no authority to any operation in the selected workspace.
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use trestle::prelude::*;
use trestle::core::{identity::ActorId, invocation::{FromInvocation, Invocation}, operation::ContextDescriptor, registry::Http};
use trestle::protocol::{Audience, Contract, ContractDoc, Field};

pub struct Directory;
impl App for Directory {
    fn id(&self) -> &'static str { "directory" }
    fn register(&self, app: &mut AppBuilder) {
        app.requires("workspace");
        app.operation(Mine).http(Http::get("/api/v1/workspaces/mine"));
    }
}

struct Actor(ActorId);
impl RequestContext for Actor {
    fn describe() -> ContextDescriptor { ContextDescriptor::authenticated() }
}
impl FromInvocation<State> for Actor {
    async fn resolve(invocation: &Invocation, _: &State) -> Result<Self> { Ok(Self(invocation.actor()?)) }
}
struct SignedIn;
impl Policy<Actor> for SignedIn {
    fn id(&self) -> &'static str { "directory.signed_in" }
    fn admit(&self, _: &Actor) -> Result<()> { Ok(()) }
    async fn recheck(&self, _: &mut PgConnection, _: &Actor) -> Result<()> { Ok(()) }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
impl Contract for Empty {
    fn contract() -> &'static ContractDoc {
        static C: LazyLock<ContractDoc> = LazyLock::new(|| ContractDoc::input("directory.mine.input", "1"));
        &C
    }
}
#[derive(Serialize)]
struct Workspace { id: Id, name: String }
impl Contract for Workspace {
    fn contract() -> &'static ContractDoc {
        static C: LazyLock<ContractDoc> = LazyLock::new(|| ContractDoc::output("directory.workspace", "1").field(Field::uuid("id")).field(Field::text("name").chars(1, 200)));
        &C
    }
}
#[derive(Serialize)]
struct Workspaces { items: Vec<Workspace> }
impl Contract for Workspaces {
    fn contract() -> &'static ContractDoc {
        static C: LazyLock<ContractDoc> = LazyLock::new(|| ContractDoc::output("directory.workspaces", "1").field(Field::items("items", Workspace::contract(), 100)));
        &C
    }
}
struct Mine;
impl Operation for Mine {
    type Context = Actor;
    type Input = Empty;
    type Output = Workspaces;
    type Policy = SignedIn;
    const INFO: OperationInfo = OperationInfo { id: "directory.mine", version: "1", mode: Mode::Read, audiences: &[Audience::Admin], summary: "Workspaces in which the caller has a current membership." };
    fn policy(&self) -> &SignedIn { &SignedIn }
    async fn run(&self, conn: &mut PgConnection, actor: &Actor, _: Empty) -> Result<Workspaces> {
        // Membership is the scope of discovery; there is no selected tenant yet.
        let rows: Vec<(Id, String)> = sqlx::query_as("SELECT w.id, w.name FROM workspace_memberships m JOIN workspaces w ON w.id = m.workspace_id WHERE m.actor_id = $1 ORDER BY w.id LIMIT 101")
            .bind(actor.0.0).fetch_all(conn).await?;
        if rows.len() > 100 { return Err(Problem::conflict("workspace_directory_requires_paging")); }
        Ok(Workspaces { items: rows.into_iter().map(|(id, name)| Workspace { id, name }).collect() })
    }
}
