//! Units: groups of lessons within a school. The second product resource,
//! added with resource conventions instead of hand-wired HTTP (§18 M4).

use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{FromRow, PgConnection};
use trestle_core::db::unique_violation;
use trestle_core::operation::{Mode, Operation, OperationInfo, Policy};
use trestle_core::pagination::{Page, decode_cursor, page_from, page_size};
use trestle_core::protocol::{Audience, Contract, ContractDoc, Field};
use trestle_core::query::{self, Col, Direction, Table};
use trestle_core::registry::{App, AppBuilder};
use trestle_core::{FieldError, Problem};
use trestle_historical::schema::{Column, Model};
use uuid::Uuid;

use crate::AppState;
use crate::audit::{self, AuditEvent};
use crate::schools::{self, TenantContext, can_write};

pub struct UnitsApp;

impl App<AppState> for UnitsApp {
    fn id(&self) -> &'static str {
        "units"
    }

    fn register(&self, app: &mut AppBuilder<AppState>) {
        app.requires("schools").requires("audit");
        app.schema(pilot_migrations::UNITS_0001).model(unit_model());
        app.resource("units.units", "/api/v1/units")
            .list(ListUnits)
            .create(CreateUnit)
            .retrieve(GetUnit);
    }
}

pub fn unit_model() -> Model {
    Model::new("units.unit")
        .table("units")
        .field(Column::uuid("id").primary_key())
        .field(Column::uuid("school_id").references("schools.school"))
        .field(Column::text("name").chars(1, 120))
        .field(Column::big_int("version").at_least(1).default_int(1))
        .field(Column::timestamptz("created_at").default_now())
        .unique("units_school_name_key", &["school_id", "name"])
}

// Contracts ----------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateUnitInput {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetUnitInput {
    pub id: Uuid,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListUnitsInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnitView {
    pub id: Uuid,
    pub name: String,
    pub version: String,
}

macro_rules! contract {
    ($ty:ty, $doc:expr) => {
        impl Contract for $ty {
            fn contract() -> &'static ContractDoc {
                static CONTRACT: LazyLock<ContractDoc> = LazyLock::new(|| $doc);
                &CONTRACT
            }
        }
    };
}

contract!(
    CreateUnitInput,
    ContractDoc::input("units.create.input", "1.0.0").field(Field::text("name").chars(1, 120))
);
contract!(
    GetUnitInput,
    ContractDoc::input("units.get.input", "1.0.0").field(Field::uuid("id"))
);
contract!(
    ListUnitsInput,
    ContractDoc::input("units.list.input", "1.0.0")
        .field(Field::text("cursor").chars(1, 1024).optional())
        .field(Field::integer("limit").range(1, 100).optional())
);
contract!(
    UnitView,
    ContractDoc::output("units.view", "1.0.0")
        .field(Field::uuid("id"))
        .field(Field::text("name").chars(1, 120))
        .field(Field::digits("version"))
);

// Persistence --------------------------------------------------------------

pub struct Units;

impl Table for Units {
    const MODEL: &'static str = "units.unit";
    const TABLE: &'static str = "units";
    const COLUMNS: &'static [&'static str] = &["id", "name", "version"];
    const PRIMARY_KEY: &'static str = "id";
}

pub const ID: Col<Units, Uuid> = Col::new("id");
pub const SCHOOL_ID: Col<Units, Uuid> = Col::new("school_id");
pub const NAME: Col<Units, String> = Col::new("name");

#[derive(Debug, FromRow)]
pub struct UnitRow {
    pub id: Uuid,
    pub name: String,
    pub version: i64,
}

impl From<UnitRow> for UnitView {
    fn from(row: UnitRow) -> Self {
        UnitView {
            id: row.id,
            name: row.name,
            version: row.version.to_string(),
        }
    }
}

/// Whether `unit` belongs to `school`. Used to revalidate relation choices.
pub async fn exists_in_school(
    conn: &mut PgConnection,
    school: schools::SchoolId,
    unit: Uuid,
) -> Result<bool, sqlx::Error> {
    Ok(query::select(SCHOOL_ID, school.0)
        .eq(ID, unit)
        .fetch_optional::<UnitRow>(conn)
        .await?
        .is_some())
}

// Policy -------------------------------------------------------------------

pub enum UnitPolicy {
    Read,
    Write,
}

impl Policy<TenantContext> for UnitPolicy {
    fn id(&self) -> &'static str {
        match self {
            UnitPolicy::Read => "units.read",
            UnitPolicy::Write => "units.write",
        }
    }

    fn admit(&self, ctx: &TenantContext) -> Result<(), Problem> {
        match self {
            UnitPolicy::Read => Ok(()),
            UnitPolicy::Write if can_write(ctx.role) => Ok(()),
            UnitPolicy::Write => Err(Problem::forbidden("permission_denied")),
        }
    }

    async fn recheck(&self, conn: &mut PgConnection, ctx: &TenantContext) -> Result<(), Problem> {
        match self {
            UnitPolicy::Read => Ok(()),
            UnitPolicy::Write => schools::recheck_role(conn, ctx, can_write).await,
        }
    }
}

// Operations ---------------------------------------------------------------

const AUDIENCES: &[Audience] = &[Audience::Product, Audience::Admin];

pub struct ListUnits;

impl Operation for ListUnits {
    type Context = TenantContext;
    type Input = ListUnitsInput;
    type Output = Page<UnitView>;
    type Policy = UnitPolicy;

    const INFO: OperationInfo = OperationInfo {
        id: "units.list",
        version: "1.0.0",
        mode: Mode::Read,
        audiences: AUDIENCES,
        summary: "List the selected school's units by name.",
    };

    fn policy(&self) -> &UnitPolicy {
        &UnitPolicy::Read
    }

    async fn run(
        &self,
        conn: &mut PgConnection,
        ctx: &TenantContext,
        input: ListUnitsInput,
    ) -> Result<Page<UnitView>, Problem> {
        let limit = page_size(input.limit, 50);
        let shape = json!({ "op": Self::INFO.id, "limit": limit });
        let mut select = query::select(SCHOOL_ID, ctx.school.0);
        if let Some(cursor) = &input.cursor {
            let position = decode_cursor(&shape, cursor)?;
            let (name, id) = keyset(&position)?;
            select = select.after(NAME, name, ID, id, Direction::Asc);
        }
        let rows: Vec<UnitRow> = select
            .order_by(NAME, Direction::Asc)
            .limit(limit + 1)
            .fetch_all(conn)
            .await?;
        page_from(
            rows,
            limit,
            &shape,
            |r| json!([r.name, r.id]),
            |r| Ok::<_, Problem>(r.into()),
        )
    }
}

fn keyset(position: &Value) -> Result<(String, Uuid), Problem> {
    let invalid = || Problem::malformed("invalid_cursor", "The cursor is not valid.");
    let name = position[0].as_str().ok_or_else(invalid)?.to_owned();
    let id = position[1]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(invalid)?;
    Ok((name, id))
}

pub struct CreateUnit;

impl Operation for CreateUnit {
    type Context = TenantContext;
    type Input = CreateUnitInput;
    type Output = UnitView;
    type Policy = UnitPolicy;

    const INFO: OperationInfo = OperationInfo {
        id: "units.create",
        version: "1.0.0",
        mode: Mode::Write,
        audiences: AUDIENCES,
        summary: "Create a unit in the selected school.",
    };

    fn policy(&self) -> &UnitPolicy {
        &UnitPolicy::Write
    }

    async fn run(
        &self,
        conn: &mut PgConnection,
        ctx: &TenantContext,
        input: CreateUnitInput,
    ) -> Result<UnitView, Problem> {
        let row: UnitRow = sqlx::query_as(
            "INSERT INTO units (id, school_id, name) VALUES ($1, $2, $3) RETURNING id, name, version",
        )
        .bind(Uuid::now_v7())
        .bind(ctx.school.0)
        .bind(&input.name)
        .fetch_one(&mut *conn)
        .await
        .map_err(|err| match unique_violation(&err) {
            Some("units_school_name_key") => Problem::validation(vec![FieldError::new(
                "/name",
                "already_exists",
                "A unit with this name already exists in this school.",
            )]),
            _ => Problem::from(err),
        })?;
        audit::record(
            conn,
            AuditEvent {
                actor: ctx.actor,
                tenant: Some(ctx.school),
                operation: Self::INFO.id,
                operation_version: Self::INFO.version,
                target_type: "unit",
                target_id: row.id.to_string(),
            },
        )
        .await?;
        Ok(row.into())
    }
}

pub struct GetUnit;

impl Operation for GetUnit {
    type Context = TenantContext;
    type Input = GetUnitInput;
    type Output = UnitView;
    type Policy = UnitPolicy;

    const INFO: OperationInfo = OperationInfo {
        id: "units.get",
        version: "1.0.0",
        mode: Mode::Read,
        audiences: AUDIENCES,
        summary: "Read one unit in the selected school.",
    };

    fn policy(&self) -> &UnitPolicy {
        &UnitPolicy::Read
    }

    async fn run(
        &self,
        conn: &mut PgConnection,
        ctx: &TenantContext,
        input: GetUnitInput,
    ) -> Result<UnitView, Problem> {
        query::select(SCHOOL_ID, ctx.school.0)
            .eq(ID, input.id)
            .fetch_optional::<UnitRow>(conn)
            .await?
            .map(Into::into)
            .ok_or_else(Problem::not_found)
    }
}
