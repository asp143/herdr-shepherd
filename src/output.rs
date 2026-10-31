use serde::Serialize;

use crate::error::ShepherdError;

pub const SCHEMA_VERSION: u32 = 1;

pub fn emit<T: Serialize>(json: bool, data: &T, human: impl FnOnce() -> String) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "schema_version": SCHEMA_VERSION,
                "ok": true,
                "data": data,
            })
        );
    } else {
        println!("{}", human());
    }
}

pub fn emit_error(json: bool, err: &ShepherdError) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "schema_version": SCHEMA_VERSION,
                "ok": false,
                "error": { "code": err.code(), "message": err.to_string() },
            })
        );
    } else {
        eprintln!("herdr-shepherd: {err}");
    }
}
