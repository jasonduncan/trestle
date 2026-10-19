//! Product-specific export preview. This is fixture application code, not a
//! branch in the framework or renderer. Work payloads carry only a record ID.
use serde_json::{Value, json};
use std::sync::Arc;
use trestle::core::{
    invocation::Invocation,
    jobs::{self, JobsApp, NewJob},
    operation::*,
    registry::{App, AppBuilder, Http, SectionDefinition, ViewDefinition},
};
use trestle::intent::{Ctx, Permission, Resolved};
use trestle::protocol::{
    Audience, ContractDoc, Field, FormPresentation, interaction::PreconditionTarget,
};
use trestle::{Problem, State};

pub fn project(name: &str, source: &str) -> trestle::Project {
    let project = trestle::project().intent_str(name, source);
    if name == "support" {
        project
            .install_app(JobsApp)
            .intent_app(|resolved| Exports(resolved.clone()))
    } else {
        project
    }
}

struct Exports(Arc<Resolved>);
impl App<State> for Exports {
    fn id(&self) -> &str {
        "support_work"
    }
    fn register(&self, app: &mut AppBuilder<State>) {
        app.requires("support").requires("jobs");
        app.dyn_operation(
            Work::new(&self.0, Task::Start),
            Some(
                Http::post("/api/v1/cases/{case_id}/export")
                    .accepted()
                    .version_pinned()
                    .precondition_target(
                        PreconditionTarget {
                            operation: "support.case.get".into(),
                            representation: "support.case".into(),
                            parameter: "id".into(),
                            input_field: "case_id".into(),
                        },
                        "expected_version",
                    ),
            ),
        );
        app.dyn_operation(
            Work::new(&self.0, Task::Status),
            Some(Http::get("/api/v1/exports/{id}")),
        );
        app.dyn_operation(Work::new(&self.0, Task::Finish), None);
        app.dyn_operation(
            Work::new(&self.0, Task::Availability),
            Some(Http::get("/api/v1/cases/{id}/availability")),
        );
        app.form(
            FormPresentation::for_operation("support_work.export.form", "support_work.start")
                .title("Export case preview")
                .submit("Queue export"),
        );
        let permission = || Arc::new(Permission::new(&self.0, "support.read").unwrap());
        app.view_dyn(
            ViewDefinition::collection("support_work.queue", "support.case.list")
                .title("Support workspace")
                .resource("support.case")
                .label("subject")
                .columns(&["subject", "state"])
                .filters(&["state"])
                .record_view("support_work.case", "id"),
            permission(),
        );
        app.view_dyn(
            ViewDefinition::record("support_work.case", "support.case.get")
                .title("Case workspace")
                .resource("support.case")
                .label("subject")
                .columns(&["subject", "state"])
                .action("support.assign.form")
                .availability(
                    "support_work.availability",
                    [("id".into(), "/id".into())].into(),
                )
                .section(SectionDefinition {
                    id: "export".into(),
                    title: "Export preview".into(),
                    query: "support.case.get".into(),
                    collection: false,
                    identity: "id".into(),
                    label: "subject".into(),
                    columns: vec!["subject".into()],
                    input: [("id".into(), "/id".into())].into(),
                    component: Some("fixture.export_panel".into()),
                    required_component: true,
                }),
            permission(),
        );
        app.view_dyn(
            ViewDefinition::record("support_work.status", "support_work.status")
                .title("Export status")
                .label("state")
                .columns(&["state", "summary"]),
            permission(),
        );
    }
}

#[derive(Clone, Copy)]
enum Task {
    Start,
    Status,
    Finish,
    Availability,
}
struct Work {
    info: OperationDescriptor,
    permission: Permission,
    resolved: Arc<Resolved>,
    input: ContractDoc,
    output: ContractDoc,
    task: Task,
}
impl Work {
    fn new(resolved: &Arc<Resolved>, task: Task) -> Arc<Self> {
        let (id, mode, permission) = match task {
            Task::Start => ("support_work.start", Mode::Write, "support.write"),
            Task::Finish => ("support_work.finish", Mode::Write, "support.write"),
            Task::Status => ("support_work.status", Mode::Read, "support.read"),
            Task::Availability => ("support_work.availability", Mode::Read, "support.read"),
        };
        let input = if matches!(task, Task::Start) {
            ContractDoc::input(id, "1")
                .field(Field::uuid("case_id"))
                .field(Field::digits("expected_version"))
        } else {
            ContractDoc::input(id, "1").field(Field::uuid("id"))
        };
        let output = if matches!(task, Task::Availability) {
            trestle::protocol::view::availability_contract("support_work.availability.view", "1")
        } else {
            ContractDoc::output("support_work.status.view", "1")
                .field(Field::uuid("id"))
                .field(Field::enumeration(
                    "state",
                    &["queued", "succeeded", "failed"],
                ))
                .field(Field::text("summary").nullable())
        };
        let mut effects = Effects::new().reads("support.case").reads("support.export");
        if mode == Mode::Write {
            effects = effects.writes("support.export");
        }
        if matches!(task, Task::Start) {
            effects = effects.emits("support_work.finish");
        }
        Arc::new(Self {
            info: OperationDescriptor::new(id, "1", mode)
                .audiences(&[Audience::Product])
                .effects(effects),
            permission: Permission::new(resolved, permission).unwrap(),
            resolved: resolved.clone(),
            input,
            output,
            task,
        })
    }
}
impl DynOperation<State> for Work {
    fn info(&self) -> &OperationDescriptor {
        &self.info
    }
    fn policy_id(&self) -> &str {
        self.permission.id()
    }
    fn context(&self) -> ContextDescriptor {
        self.permission.context()
    }
    fn input(&self) -> &ContractDoc {
        &self.input
    }
    fn output(&self) -> &ContractDoc {
        &self.output
    }
    fn resolve_context<'a>(
        &'a self,
        invocation: &'a Invocation,
        state: &'a State,
    ) -> BoxFuture<'a, Result<DynContext, Problem>> {
        Box::pin(self.permission.resolve(invocation, state))
    }
    fn admit(&self, ctx: &DynContext) -> Result<(), Problem> {
        self.permission.admit(ctx)
    }
    fn recheck<'a>(
        &'a self,
        conn: &'a mut sqlx::PgConnection,
        ctx: &'a DynContext,
    ) -> BoxFuture<'a, Result<(), Problem>> {
        Box::pin(self.permission.recheck(conn, ctx))
    }
    fn run(
        self: Arc<Self>,
        conn: DynConn,
        ctx: DynContext,
        input: Value,
    ) -> BoxFuture<'static, (DynConn, Result<Value, Problem>)> {
        Box::pin(async move {
            let resolved = self.resolved.clone();
            let can_write = Permission::new(&resolved, "support.write")
                .unwrap()
                .admit(&ctx)
                .is_ok();
            let (mut conn, result) = Ctx::run(&resolved, conn, &ctx, async |data| {
                let mut invocation = Invocation::for_actor(data.actor());
                if let Some(tenant) = data.tenant() { invocation = invocation.with_tenant(tenant); }
                if matches!(self.task, Task::Availability) {
                    let id = input["id"].as_str().and_then(|v| v.parse().ok()).ok_or_else(Problem::not_found)?;
                    let record = data.get("support.case", id).await?;
                    let allowed = can_write && record.get::<String>("state")? == "open";
                    return Ok((json!({ "resource": "support.case", "record_key": id.to_string(), "etag": format!("\"support.case:{}\"", record.get::<i64>("version")?), "actions": [{ "id": "support.assign.form", "available": allowed, "reason": if allowed { None } else { Some("unavailable") } }] }), invocation));
                }
                let row = if matches!(self.task, Task::Start) {
                    let id = input["case_id"].as_str().and_then(|v| v.parse().ok()).ok_or_else(Problem::not_found)?;
                    let record = data.get("support.case", id).await?;
                    if record.get::<i64>("version")?.to_string() != input["expected_version"].as_str().unwrap_or_default() { return Err(Problem::precondition_failed()); }
                    data.create("support.export", json!({ "case_ref": id, "actor": data.actor().0, "snapshot": record.to_json().to_string() })).await?
                } else {
                    let id = input["id"].as_str().and_then(|v| v.parse().ok()).ok_or_else(Problem::not_found)?;
                    let row = data.get("support.export", id).await?;
                    if row.get::<uuid::Uuid>("actor")? != data.actor().0 { return Err(Problem::not_found()); }
                    if matches!(self.task, Task::Finish) && row.get::<String>("state")? != "succeeded" {
                        data.update("support.export", id, json!({ "state": "succeeded", "summary": row.get::<String>("snapshot")? })).await?
                    } else { row }
                };
                let value = row.to_json();
                Ok((json!({ "id": value["id"], "state": value["state"], "summary": value["summary"] }), invocation))
            }).await;
            let result = match result {
                Ok((output, invocation)) if matches!(self.task, Task::Start) => {
                    let job =
                        NewJob::operation("support_work.finish", json!({ "id": output["id"] }))
                            .on_behalf_of(&invocation)
                            .dedupe_key(format!("export:{}", output["id"]));
                    jobs::enqueue(conn.conn(), job).await.map(|_| output)
                }
                Ok((output, _)) => Ok(output),
                Err(problem) => Err(problem),
            };
            (conn, result)
        })
    }
}
