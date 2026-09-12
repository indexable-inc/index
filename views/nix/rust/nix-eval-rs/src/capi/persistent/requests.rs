//! Bounded request-file protocol. A completed prefix never authorizes a batch.

use serde::{Deserialize, Deserializer};
use std::collections::HashSet;

mod ffi;
#[cfg(test)]
mod tests;

const MAX_FILE_BYTES: usize = 4 * 1024 * 1024;
const MAX_REQUESTS: usize = 256;
const MAX_ID_BYTES: usize = 128;
const MAX_INSTALLABLE_BYTES: usize = 64 * 1024;
const MAX_APPLY_BYTES: usize = 1024 * 1024;
const MAX_REPORT_BYTES: usize = 8 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestFile {
    version: u32,
    requests: Vec<Request>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    id: String,
    installable: String,
    // An explicit null means no --apply. Missing the field is a malformed request.
    #[serde(deserialize_with = "explicit_apply")]
    apply: Option<String>,
}

fn explicit_apply<'de, D: Deserializer<'de>>(input: D) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(input)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Ready,
    Pending,
    Failed,
}

struct Batch {
    requests: Vec<Request>,
    next: usize,
    phase: Phase,
    output_bytes: usize,
}

impl Batch {
    fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_FILE_BYTES {
            return Err("request file exceeds 4 MiB".to_owned());
        }
        let file: RequestFile = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
        if file.version != 1 || file.requests.is_empty() || file.requests.len() > MAX_REQUESTS {
            return Err("request file requires version 1 and 1..256 requests".to_owned());
        }
        let mut ids = HashSet::new();
        for request in &file.requests {
            if request.id.is_empty()
                || request.id.len() > MAX_ID_BYTES
                || !ids.insert(request.id.as_str())
            {
                return Err("request IDs must be unique and 1..128 UTF-8 bytes".to_owned());
            }
            if request.installable.is_empty()
                || request.installable.len() > MAX_INSTALLABLE_BYTES
                || request.installable.contains('\0')
            {
                return Err("installable must be 1..65536 UTF-8 bytes without NUL".to_owned());
            }
            if request
                .apply
                .as_ref()
                .is_some_and(|apply| apply.len() > MAX_APPLY_BYTES || apply.contains('\0'))
            {
                return Err("apply must be null or at most 1 MiB without NUL".to_owned());
            }
        }
        Ok(Self {
            requests: file.requests,
            next: 0,
            phase: Phase::Ready,
            output_bytes: 0,
        })
    }

    fn begin(&mut self) -> Result<Option<&Request>, String> {
        if self.phase != Phase::Ready {
            return Err("request batch is pending or failed".to_owned());
        }
        let request = self.requests.get(self.next);
        if request.is_some() {
            self.phase = Phase::Pending;
        }
        Ok(request)
    }

    fn complete(&mut self, success: bool, payload: &str) -> Result<String, String> {
        if self.phase != Phase::Pending {
            return Err("no pending request".to_owned());
        }
        // Any framing/budget error also terminates the batch. A caller cannot
        // catch it and silently continue with later requests.
        self.phase = Phase::Failed;
        if payload.len() > MAX_REPORT_BYTES {
            return Err("request report exceeds 8 MiB".to_owned());
        }
        let request = self.requests.get(self.next).ok_or("missing pending request")?;
        let mut report = if success {
            let value: serde_json::Value =
                serde_json::from_str(payload).map_err(|error| error.to_string())?;
            let serde_json::Value::Object(fields) = value else {
                return Err("completed request report is not an object".to_owned());
            };
            if fields.get("installable").and_then(serde_json::Value::as_str)
                != Some(request.installable.as_str())
                || !fields.contains_key("value")
                || ["version", "id", "status"].iter().any(|key| fields.contains_key(*key))
            {
                return Err("completed request report identity or shape differs".to_owned());
            }
            fields
        } else {
            let mut fields = serde_json::Map::new();
            fields.insert("installable".to_owned(), request.installable.clone().into());
            fields.insert("error".to_owned(), payload.into());
            fields
        };
        report.insert("version".to_owned(), 1.into());
        report.insert("id".to_owned(), request.id.clone().into());
        report.insert("status".to_owned(), if success { "ok" } else { "error" }.into());
        let mut encoded = serde_json::to_string(&report).map_err(|error| error.to_string())?;
        encoded.push('\n');
        if encoded.len() > MAX_REPORT_BYTES
            || encoded.len() > MAX_OUTPUT_BYTES.saturating_sub(self.output_bytes)
        {
            return Err("request-file output budget exceeded".to_owned());
        }
        self.output_bytes += encoded.len();
        self.next += 1;
        if success {
            self.phase = Phase::Ready;
        }
        Ok(encoded)
    }
}
