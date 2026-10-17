//! `GET /api/v1/lessons/export.csv`: an ordinary Axum handler (§7.3, I13).
//!
//! Mounted as an unmanaged route with a named waiver, so it has no contract
//! or OpenAPI entry of its own. It resolves its context the same way as
//! managed routes (`Resolved`, ADR-013), and every row comes from
//! `lessons.list` through the executor, so scoping and policy are the
//! operation's.

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use http::header;
use trestle_core::Problem;
use trestle_core::invocation::Resolved;
use trestle_core::operation::execute;

use super::contracts::ListLessonsInput;
use super::operations::ListLessons;
use crate::AppState;
use crate::schools::TenantContext;

/// Bound on pages read for one download.
const MAX_PAGES: usize = 50;

pub(crate) async fn csv(
    State(state): State<AppState>,
    Resolved(ctx): Resolved<TenantContext>,
) -> Result<Response, Problem> {
    let mut out = String::from("id,title,grade_level,status,version,subtitle\n");
    let mut cursor = None;
    for _ in 0..MAX_PAGES {
        let input = ListLessonsInput {
            cursor: cursor.take(),
            limit: Some(100),
            ..Default::default()
        };
        let page = execute(&state.pool, &ListLessons, &ctx, input).await?;
        for lesson in &page.items {
            let row = [
                lesson.id.to_string(),
                lesson.title.clone(),
                lesson.grade_level.to_string(),
                lesson.status.as_str().to_owned(),
                lesson.version.clone(),
                lesson.subtitle.clone().unwrap_or_default(),
            ];
            out.push_str(&row.iter().map(|v| cell(v)).collect::<Vec<_>>().join(","));
            out.push('\n');
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"lessons.csv\"",
            ),
        ],
        out,
    )
        .into_response())
}

/// Quotes a CSV cell and neutralizes spreadsheet formulas (a leading `=`,
/// `+`, `-` or `@` gets an apostrophe).
fn cell(value: &str) -> String {
    let safe = if value.starts_with(['=', '+', '-', '@']) {
        format!("'{value}")
    } else {
        value.to_owned()
    };
    if safe.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", safe.replace('"', "\"\""))
    } else {
        safe
    }
}

#[cfg(test)]
mod tests {
    use super::cell;

    #[test]
    fn cells_are_quoted_and_formulas_neutralized() {
        assert_eq!(cell("Fractions"), "Fractions");
        assert_eq!(cell("a,b"), "\"a,b\"");
        assert_eq!(cell("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(cell("=HYPERLINK(\"x\")"), "\"'=HYPERLINK(\"\"x\"\")\"");
        assert_eq!(cell("-5"), "'-5");
    }
}
