//! Lists: `{items, next_cursor}` pages and opaque cursors (design doc §7.1).
//!
//! A cursor is `base64url(payload).base64url(hmac)` where the payload holds
//! the keyset position and a digest of the query shape (filters, ordering,
//! limit). Cursors are integrity-protected, bounded in size, and refused
//! when reused with a different query. They carry no authority: the list
//! operation re-scopes and re-authorizes every request.

use std::any::TypeId;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use trestle_protocol::{Contract, ContractDoc, Field};

use crate::Problem;

/// Largest page any list returns.
pub const MAX_PAGE: u32 = 100;
const MAX_CURSOR_BYTES: usize = 1024;

/// One page of `T`. Its contract, `<item contract>.page`, is derived once
/// per item type.
#[derive(Debug, Clone, Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

impl<T: Contract + 'static> Contract for Page<T> {
    fn contract() -> &'static ContractDoc {
        static PAGES: OnceLock<Mutex<HashMap<TypeId, &'static ContractDoc>>> = OnceLock::new();
        let mut pages = PAGES
            .get_or_init(Default::default)
            .lock()
            .expect("page registry");
        pages.entry(TypeId::of::<T>()).or_insert_with(|| {
            let item = T::contract();
            Box::leak(Box::new(
                ContractDoc::output(&format!("{}.page", item.id), &item.version)
                    .field(Field::items("items", item, MAX_PAGE))
                    .field(Field::text("next_cursor").nullable()),
            ))
        })
    }
}

type HmacSha256 = Hmac<Sha256>;

static KEY: OnceLock<Vec<u8>> = OnceLock::new();

/// Installs the cursor signing key (`TRESTLE_CURSOR_KEY`). Without one, a
/// random per-process key is used: cursors then expire on restart.
pub fn install_key(key: &[u8]) -> Result<(), &'static str> {
    if key.len() < 32 {
        return Err("the cursor key must be at least 32 bytes");
    }
    KEY.set(key.to_vec())
        .map_err(|_| "the cursor key is already installed")
}

pub fn key_installed() -> bool {
    KEY.get().is_some()
}

fn key() -> &'static [u8] {
    KEY.get_or_init(|| {
        let mut bytes = Vec::with_capacity(32);
        for _ in 0..2 {
            bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
        }
        bytes
    })
}

fn shape_digest(shape: &Value) -> String {
    let hash = Sha256::digest(
        serde_json::to_vec(&trestle_protocol::canonicalize(shape)).unwrap_or_default(),
    );
    hash.iter().take(12).map(|b| format!("{b:02x}")).collect()
}

fn sign(payload: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key()).expect("any key length works");
    mac.update(payload);
    mac.finalize().into_bytes().to_vec()
}

/// Encodes `position` for the list query described by `shape`.
pub fn encode_cursor(shape: &Value, position: &Value) -> String {
    let payload =
        serde_json::to_vec(&json!({ "q": shape_digest(shape), "p": position })).unwrap_or_default();
    format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(&payload),
        URL_SAFE_NO_PAD.encode(sign(&payload))
    )
}

/// Decodes a cursor produced for the same `shape`. 400 otherwise.
pub fn decode_cursor(shape: &Value, cursor: &str) -> Result<Value, Problem> {
    let invalid = || {
        Problem::malformed(
            "invalid_cursor",
            "The cursor is not valid. Start again from the first page.",
        )
    };
    if cursor.len() > MAX_CURSOR_BYTES {
        return Err(invalid());
    }
    let (payload, signature) = cursor.split_once('.').ok_or_else(invalid)?;
    let payload = URL_SAFE_NO_PAD.decode(payload).map_err(|_| invalid())?;
    let signature = URL_SAFE_NO_PAD.decode(signature).map_err(|_| invalid())?;
    let mut mac = HmacSha256::new_from_slice(key()).expect("any key length works");
    mac.update(&payload);
    mac.verify_slice(&signature).map_err(|_| invalid())?;
    let decoded: Value = serde_json::from_slice(&payload).map_err(|_| invalid())?;
    if decoded["q"] != shape_digest(shape) {
        return Err(Problem::malformed(
            "cursor_mismatch",
            "This cursor belongs to a different filter or ordering. Start again from the first page.",
        ));
    }
    Ok(decoded["p"].clone())
}

/// Builds a page from up to `limit + 1` rows fetched in keyset order: the
/// extra row only signals that another page exists, and the cursor points
/// at the last row returned.
pub fn page_from<R, T, E>(
    mut rows: Vec<R>,
    limit: i64,
    shape: &Value,
    position: impl Fn(&R) -> Value,
    view: impl Fn(R) -> Result<T, E>,
) -> Result<Page<T>, E> {
    let more = rows.len() as i64 > limit;
    rows.truncate(limit.max(0) as usize);
    let next_cursor = if more {
        rows.last()
            .map(|last| encode_cursor(shape, &position(last)))
    } else {
        None
    };
    Ok(Page {
        items: rows.into_iter().map(view).collect::<Result<_, _>>()?,
        next_cursor,
    })
}

/// Page size from an optional request value.
pub fn page_size(requested: Option<i64>, default: u32) -> i64 {
    requested
        .unwrap_or(i64::from(default))
        .clamp(1, i64::from(MAX_PAGE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_round_trip_and_resist_tampering_and_reuse() {
        let shape = json!({"grade": 6, "order": "title"});
        let cursor = encode_cursor(&shape, &json!(["Area", "01a0"]));
        assert_eq!(
            decode_cursor(&shape, &cursor).unwrap(),
            json!(["Area", "01a0"])
        );

        let other = json!({"grade": 7, "order": "title"});
        assert_eq!(
            decode_cursor(&other, &cursor).unwrap_err().code(),
            "cursor_mismatch"
        );

        let (payload, signature) = cursor.split_once('.').unwrap();
        let forged_payload = URL_SAFE_NO_PAD.encode(br#"{"q":"x","p":["Zzz","ffff"]}"#);
        for forged in [
            format!("{forged_payload}.{signature}"),
            format!("{payload}.{}", URL_SAFE_NO_PAD.encode([0u8; 32])),
            "not-a-cursor".to_owned(),
            "x".repeat(2000),
        ] {
            assert_eq!(
                decode_cursor(&shape, &forged).unwrap_err().code(),
                "invalid_cursor"
            );
        }
    }

    #[test]
    fn page_contract_is_derived_per_item_type() {
        struct Item;
        impl Contract for Item {
            fn contract() -> &'static ContractDoc {
                static C: OnceLock<ContractDoc> = OnceLock::new();
                C.get_or_init(|| ContractDoc::output("a.item", "2").field(Field::uuid("id")))
            }
        }
        let page = Page::<Item>::contract();
        assert_eq!(page.id, "a.item.page");
        assert_eq!(page.version, "2");
        assert!(std::ptr::eq(page, Page::<Item>::contract()));
        assert!(page.problems().is_empty());
        assert_eq!(page_size(Some(1000), 20), 100);
        assert_eq!(page_size(None, 20), 20);
    }
}
