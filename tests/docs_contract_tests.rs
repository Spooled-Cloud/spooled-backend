//! Documentation contracts.
//!
//! Some statements in the guides, in `docs/openapi.yaml` and in the knowledge base are about
//! behaviour that is not fixed in the same change: a limit of the gRPC `Struct` fields, the status
//! a handler answers with, a table that exists but is not used, a cost that is measured and
//! deferred. Each test here reads the code (or the migration) that makes the statement true and
//! the document that makes it, so a change to one has to change the other.
//!
//! These tests need no Docker, database or network.

use serde_json::{json, Value};

use prost_types::value::Kind;
use spooled_backend::grpc::convert::{json_to_struct, struct_to_json, struct_to_json_opt};

const GRPC_GUIDE: &str = include_str!("../docs/guides/grpc-server.md");
const GRPC_DATAFLOW: &str = include_str!("../docs/ai/knowledge/dataflows/grpc-rest.md");
const OPERATIONS_GUIDE: &str = include_str!("../docs/guides/operations.md");
const ARCHITECTURE_GUIDE: &str = include_str!("../docs/guides/architecture.md");
const CHESTERTON_FENCES: &str = include_str!("../docs/ai/knowledge/risk/chesterton-fences.md");
const JOBS_DATAFLOW: &str = include_str!("../docs/ai/knowledge/dataflows/jobs-queue.md");
const FINDINGS_JSONL: &str = include_str!("../docs/ai/knowledge/bugs/findings.jsonl");
const FINDINGS_MD: &str = include_str!("../docs/ai/knowledge/bugs/findings.md");
const OPENAPI: &str = include_str!("../docs/openapi.yaml");
const PROTO: &str = include_str!("../proto/spooled.proto");
const SERVER_CONVERT: &str = include_str!("../src/grpc/convert.rs");
const EMAIL_LOGIN: &str = include_str!("../src/api/handlers/email_login.rs");
const ORGANIZATIONS: &str = include_str!("../src/api/handlers/organizations.rs");
const AUTH_DATAFLOW: &str = include_str!("../docs/ai/knowledge/dataflows/auth.md");
const CHANGELOG: &str = include_str!("../CHANGELOG.md");
const WORKFLOW_HANDLERS: &str = include_str!("../src/api/handlers/workflows.rs");
const SCHEDULER: &str = include_str!("../src/scheduler/mod.rs");
const QUEUE: &str = include_str!("../src/queue/mod.rs");
const INITIAL_SCHEMA: &str = include_str!("../migrations/20241209000001_initial_schema.sql");
const PARTITIONED_JOBS: &str = include_str!("../migrations/20241209000009_partitioned_jobs.sql");

/// Statuses `findings.jsonl` uses. A new one is added here and described in
/// `docs/ai/knowledge/MAINTENANCE.md` first.
const FINDING_STATUSES: [&str; 6] = [
    "open",
    "open (documented)",
    "deferred",
    "fixed",
    "mitigated",
    "closed",
];

fn findings() -> Vec<Value> {
    FINDINGS_JSONL
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("a findings.jsonl line is a JSON value"))
        .collect()
}

fn finding(id: &str) -> Value {
    findings()
        .into_iter()
        .find(|f| f["id"] == id)
        .unwrap_or_else(|| panic!("{id} is not in findings.jsonl"))
}

fn assert_mentions(what: &str, doc: &str, needles: &[&str]) {
    let missing: Vec<&&str> = needles.iter().filter(|n| !doc.contains(**n)).collect();
    assert!(missing.is_empty(), "{what} must mention {missing:?}");
}

fn assert_finding(id: &str, status: &str, note_mentions: &[&str]) {
    let f = finding(id);
    assert_eq!(f["status"], status, "{id} status in findings.jsonl");
    let note = f["note"].as_str().expect("note");
    assert_mentions(&format!("the {id} note"), note, note_mentions);
    assert!(
        FINDINGS_MD.contains(&format!("| {id} |")),
        "{id} has a row in findings.md"
    );
}

/// Response codes documented for `method` of `path` under `paths:` of docs/openapi.yaml
/// (block style, two-space indentation, as scripts/check_openapi_drift.py reads it).
fn documented_response_codes(path: &str, method: &str) -> Vec<String> {
    let path_line = format!("  {path}:");
    let method_line = format!("    {method}:");
    let (mut in_path, mut in_method, mut in_responses) = (false, false, false);
    let mut codes = Vec::new();
    for line in OPENAPI.lines() {
        let text = line.trim_start();
        if text.is_empty() || text.starts_with('#') {
            continue;
        }
        let indent = line.len() - text.len();
        if indent <= 2 {
            in_path = line == path_line;
            (in_method, in_responses) = (false, false);
        } else if !in_path {
            continue;
        } else if indent == 4 {
            in_method = line == method_line;
            in_responses = false;
        } else if !in_method {
            continue;
        } else if indent == 6 {
            in_responses = line == "      responses:";
        } else if in_responses && indent == 8 {
            if let Some(code) = text.strip_suffix(':') {
                codes.push(code.trim_matches('\'').to_string());
            }
        }
    }
    codes
}

fn squash(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------------
// findings.jsonl is edited by hand: keep it readable by the next person.
// ---------------------------------------------------------------------------
#[test]
fn findings_jsonl_rows_are_well_formed() {
    let rows = findings();
    assert!(rows.len() > 150, "read {} findings", rows.len());
    let mut seen = std::collections::HashSet::new();
    for row in &rows {
        let id = row["id"].as_str().expect("id");
        assert!(seen.insert(id.to_string()), "{id} appears twice");
        for key in ["sev", "summary", "path", "status"] {
            assert!(
                row[key].as_str().is_some_and(|v| !v.is_empty()),
                "{id} has a non-empty {key}"
            );
        }
        assert!(
            ["P0", "P1", "P2", "P3"].contains(&row["sev"].as_str().unwrap()),
            "{id} severity {}",
            row["sev"]
        );
        let status = row["status"].as_str().unwrap();
        assert!(
            FINDING_STATUSES.contains(&status),
            "{id} has the unknown status {status:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// BEGRPC-05: gRPC payloads and results are google.protobuf.Struct.
// ---------------------------------------------------------------------------
#[test]
fn grpc_struct_limits_are_true_and_documented() {
    // The behaviour the guide describes.
    for value in [
        json!([1, 2]),
        json!("text"),
        json!(42),
        json!(true),
        json!(null),
    ] {
        assert!(
            json_to_struct(&value).is_none(),
            "{value} is not an object, so a Struct cannot carry it"
        );
    }
    let object = json!({"items": [1, "two", null], "nested": {"n": 7}});
    let back = struct_to_json(&json_to_struct(&object).expect("an object is a Struct"));
    assert_eq!(back, object, "an object arrives whole, with what is nested");

    // Delivery to a worker (`job_to_proto` -> `json_to_struct`): EVERY number is a double on the
    // wire, whole or not. A REST payload {"n": 42} reaches a Python, Go or PHP worker as 42.0,
    // and an integer of 2^53 or more can arrive rounded.
    let delivered = json_to_struct(&json!({"n": 42, "big": 9_007_199_254_740_993_i64, "f": 1.5}))
        .expect("an object is a Struct");
    let number = |name: &str| match delivered.fields[name].kind {
        Some(Kind::NumberValue(n)) => n,
        ref other => panic!("{name} is not a number on the wire: {other:?}"),
    };
    assert_eq!(
        number("n"),
        42.0,
        "a whole number below 2^53 is sent as a double"
    );
    assert_eq!(
        number("big"),
        9_007_199_254_740_992.0,
        "2^53 + 1 is rounded"
    );
    assert_eq!(number("f"), 1.5);
    assert!(
        SERVER_CONVERT.contains("serde_json::Value::Number(n) => Kind::NumberValue(")
            && SERVER_CONVERT.contains("payload: json_to_struct(&job.payload),")
            && SERVER_CONVERT.contains("result: job.result.as_ref().and_then(json_to_struct),"),
        "job_to_proto sends the payload and the result through json_to_struct"
    );

    // Enqueue / Complete to REST (`struct_to_json_opt`): the Struct a client sends is stored as
    // JSON, and a whole number below 2^53 becomes an integer there. This is the only direction
    // that converts to an integer; it is not what a worker receives.
    let below = struct_to_json(&json_to_struct(&json!({"n": 9_007_199_254_740_991_i64})).unwrap());
    assert!(
        below["n"].is_i64(),
        "a Struct number below 2^53 is stored as an integer: {below}"
    );
    let above = struct_to_json(&json_to_struct(&json!({"n": 9_007_199_254_740_993_i64})).unwrap());
    assert!(
        above["n"].is_f64(),
        "2^53 + 1 is stored as a float when it came over gRPC: {above}"
    );
    assert_eq!(above["n"].as_f64(), Some(9_007_199_254_740_992.0));

    // A job enqueued over gRPC without a payload is stored as `{}` and arrives as an empty
    // Struct, so an absent field on a delivered job means the value was not an object.
    assert_eq!(struct_to_json_opt(None), json!({}));
    assert!(json_to_struct(&json!({})).is_some());

    // The proto has the four Struct fields and no JSON string or bytes twin; adding one is the
    // fix for BEGRPC-05 and has to update the guide, the dataflow note and the finding.
    assert_eq!(
        PROTO.matches("google.protobuf.Struct").count(),
        4,
        "Job.payload, Job.result, EnqueueRequest.payload and CompleteRequest.result"
    );
    assert!(
        !PROTO.contains("payload_json") && !PROTO.contains("result_json"),
        "the proto gained a JSON field: update the gRPC guide, the dataflow note and BEGRPC-05"
    );

    // The statements. The guide is re-wrapped by hand now and then, so it is read with its
    // white space collapsed.
    let guide = squash(GRPC_GUIDE);
    assert_mentions(
        "docs/guides/grpc-server.md",
        &guide,
        &[
            "Payloads and results are `google.protobuf.Struct`",
            "`EnqueueRequest.payload`",
            "`CompleteRequest.result`",
            "field **absent**",
            "arrives as an empty Struct",
            "(`Dequeue`, `StreamJobs`, `ProcessJobs`,",
            // Delivery: every number is a double.
            "**Numbers are doubles.**",
            "the server sends every JSON number to a worker that way",
            "`{\"n\": 42}` arrives as `42.0` (a float) in Python, Go and PHP, and as `42` in JavaScript",
            "Do not test a received number with an integer type check",
            "converting it back to an integer in the worker is safe",
            "9007199254740993",
            "9007199254740992.0",
            "REST keeps every integer exactly, in both directions",
            // Enqueue / Complete to REST: the one direction that converts.
            "in a Struct that a client sends (an `Enqueue` payload, a `Complete` result) is stored as a JSON integer, so REST reads `42`, not `42.0`",
            "`bytes payload_json`",
        ],
    );
    // What the guide used to say, and what a worker does not get.
    for wrong in [
        "comes back as an integer",
        "Struct numbers that are whole and below 2^53 come back as integers",
        "it comes back as a float",
    ] {
        assert!(
            !guide.contains(wrong),
            "docs/guides/grpc-server.md says {wrong:?}, but a worker receives every number as a double"
        );
    }
    assert_mentions(
        "dataflows/grpc-rest.md",
        &squash(GRPC_DATAFLOW),
        &[
            "Only a JSON object is a Struct",
            "**open (documented)**",
            "sends EVERY number as `Kind::NumberValue(f64)`",
            "reaches a gRPC worker as `42.0` in Python",
        ],
    );
    assert_finding(
        "BEGRPC-05",
        "open (documented)",
        &["new proto fields", "use REST", "DOCUMENTED, not fixed"],
    );
}

// ---------------------------------------------------------------------------
// FESF-17 (backend half): website signup refuses the reserved slugs.
// ---------------------------------------------------------------------------
#[test]
fn signup_reserved_slugs_are_recorded() {
    // The code: complete_signup reads the same list POST /organizations does, through one helper
    // that also refuses before the token is consumed (the unit tests in email_login.rs pin the
    // behaviour and the order).
    assert_mentions(
        "src/api/handlers/email_login.rs",
        EMAIL_LOGIN,
        &[
            "fn normalize_signup_slug(raw: &str) -> AppResult<String> {",
            "crate::models::validate_slug_not_reserved(&slug)",
            "let slug = normalize_signup_slug(&request.slug)?;",
        ],
    );

    let fixed = finding("FESF-17");
    assert_eq!(fixed["fixed_in"], "0.1.123", "{fixed}");
    assert_finding(
        "FESF-17",
        "fixed",
        &[
            "`normalize_signup_slug`",
            "before the one-time token is consumed",
            "Deliberately NOT applied: `SLUG_REGEX`",
            "`check-slug`",
        ],
    );
    assert_mentions(
        "dataflows/auth.md",
        AUTH_DATAFLOW,
        &[
            "`normalize_signup_slug`",
            "`RESERVED_SLUGS`",
            "`SLUG_REGEX`",
        ],
    );
    assert_mentions("CHANGELOG.md", CHANGELOG, &["(FESF-17, backend half)"]);
}

// ---------------------------------------------------------------------------
// BEFLOW-23: POST /workflows answers 200.
// ---------------------------------------------------------------------------
#[test]
fn create_workflow_is_documented_with_the_status_the_handler_returns() {
    // The reader of the document finds what it is meant to find.
    assert!(documented_response_codes("/api/v1/jobs", "post").contains(&"201".to_string()));
    assert!(documented_response_codes("/api/v1/workflows", "get").contains(&"200".to_string()));

    let codes = documented_response_codes("/api/v1/workflows", "post");
    assert!(
        codes.contains(&"200".to_string()),
        "POST /api/v1/workflows documents {codes:?}: the handler answers 200"
    );
    assert!(
        !codes.contains(&"201".to_string()),
        "POST /api/v1/workflows documents 201 but the handler answers 200: {codes:?}"
    );

    // The handler answers a plain `Json` (200). The docker suites assert StatusCode::OK for it;
    // moving it to 201 means changing this test, the document and the SDK adapters together.
    assert_mentions(
        "src/api/handlers/workflows.rs (create)",
        WORKFLOW_HANDLERS,
        &[
            ") -> AppResult<Json<CreateWorkflowResponse>> {",
            "Ok(Json(CreateWorkflowResponse {",
        ],
    );

    assert_finding(
        "BEFLOW-23",
        "open",
        &["PARTIALLY FIXED (docs only", "`WorkflowDetailResponse`"],
    );
}

// ---------------------------------------------------------------------------
// BEJOB-17: jobs_partitioned is scaffolding; swapping it in breaks idempotency.
// ---------------------------------------------------------------------------
#[test]
fn partitioned_jobs_scaffolding_is_fenced_in_the_docs() {
    // The facts the warning rests on.
    let table_start = PARTITIONED_JOBS
        .find("CREATE TABLE IF NOT EXISTS jobs_partitioned (")
        .expect("jobs_partitioned is created by migration 20241209000009");
    let table_end = table_start
        + PARTITIONED_JOBS[table_start..]
            .find(") PARTITION BY RANGE")
            .expect("jobs_partitioned is partitioned by range");
    let table = &PARTITIONED_JOBS[table_start..table_end];
    for column in [
        "completion_webhook",
        "workflow_id",
        "workflow_step",
        "dependency_mode",
        "lease_duration_secs",
    ] {
        assert!(
            !table.contains(column),
            "jobs_partitioned gained {column}: revisit the warning in docs/guides/operations.md"
        );
    }
    assert!(
        squash(PARTITIONED_JOBS).contains(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_jobs_partitioned_idempotency \
             ON jobs_partitioned (organization_id, idempotency_key, created_at)"
        ),
        "the idempotency index of jobs_partitioned changed: revisit the warning"
    );
    assert!(
        squash(INITIAL_SCHEMA).contains(
            "CREATE UNIQUE INDEX idx_jobs_idempotency ON jobs(organization_id, idempotency_key) \
             WHERE idempotency_key IS NOT NULL;"
        ),
        "the idempotency index of jobs is the one the warning names"
    );
    assert!(
        QUEUE.contains("ON CONFLICT (organization_id, idempotency_key)"),
        "the enqueue statements infer the unique index from those two columns"
    );

    // The warning, wherever a reader looks for partitioning.
    assert_mentions(
        "docs/guides/operations.md",
        OPERATIONS_GUIDE,
        &[
            "the live `jobs` table is not partitioned",
            "`jobs_partitioned`",
            "**Idempotency breaks.**",
            "BEJOB-17",
        ],
    );
    assert_mentions(
        "docs/guides/architecture.md",
        ARCHITECTURE_GUIDE,
        &["`jobs_partitioned`", "would break idempotency"],
    );
    assert_mentions(
        "dataflows/jobs-queue.md",
        JOBS_DATAFLOW,
        &["`jobs_partitioned`", "BEJOB-17"],
    );
    assert_mentions(
        "risk/chesterton-fences.md",
        CHESTERTON_FENCES,
        &["33. **`jobs_partitioned` is dormant scaffolding"],
    );
    assert_finding(
        "BEJOB-17",
        "deferred",
        &["DEFERRED", "idempotency", "sqlx checksums"],
    );
}

// ---------------------------------------------------------------------------
// BEJOB-21: measured costs at scale, deferred.
// ---------------------------------------------------------------------------
#[test]
fn scale_costs_are_recorded_as_deferred() {
    // The code is still as the entry describes it; the day it changes, the entry changes.
    assert!(
        SCHEDULER
            .contains("AND (child.dependencies_met = FALSE OR child.dependencies_met IS NULL)"),
        "the dependency sweep changed: update BEJOB-21 and dataflows/jobs-queue.md"
    );
    assert!(
        QUEUE.contains("// Record history for each claimed job (batch operation"),
        "dequeue_batch changed how it records history: update BEJOB-21"
    );

    assert_finding(
        "BEJOB-21",
        "deferred",
        &[
            "DEFERRED",
            "320k",
            "47 ms",
            "about 130k jobs",
            "after the SDK wave",
        ],
    );
    assert_mentions(
        "dataflows/jobs-queue.md",
        JOBS_DATAFLOW,
        &["Known costs at scale (BEJOB-21, **deferred**"],
    );
}

// ---------------------------------------------------------------------------
// W9-R1, W9-R2: check-slug, generate-slug and signup agree on the slug (0.1.124).
// ---------------------------------------------------------------------------
#[test]
fn signup_slug_rules_agree_is_recorded() {
    // The code the entries describe: check-slug applies the signup length before its character
    // rules, generate-slug suffixes a reserved or short base, and both cut a suffixed slug to 50
    // (the unit tests in organizations.rs pin the behaviour).
    assert_mentions(
        "src/api/handlers/organizations.rs",
        ORGANIZATIONS,
        &[
            "pub(crate) const SLUG_MIN_LEN: usize = 3;",
            "pub(crate) const SLUG_MAX_LEN: usize = 50;",
            "validate_signup_slug_length(&slug).and_then(|()| validate_slug(&slug))",
            "base.len() < SLUG_MIN_LEN || crate::models::RESERVED_SLUGS.contains(&base)",
            "truncate_slug(base, SLUG_MAX_LEN - SLUG_SUFFIX_LEN)",
        ],
    );
    // Signup itself is unchanged: still 3-50 as sent.
    assert_mentions(
        "src/api/handlers/email_login.rs",
        EMAIL_LOGIN,
        &["#[validate(length(min = 3, max = 50, message = \"Slug must be 3-50 characters\"))]"],
    );

    for id in ["W9-R1", "W9-R2"] {
        let fixed = finding(id);
        assert_eq!(fixed["fixed_in"], "0.1.124", "{fixed}");
    }
    assert_finding(
        "W9-R1",
        "fixed",
        &[
            "`validate_signup_slug_length`",
            "Slug must be 3-50 characters",
            "Deliberately NOT changed",
        ],
    );
    assert_finding(
        "W9-R2",
        "fixed",
        &["`RESERVED_SLUGS`", "`base_needs_suffix`", "`org-`"],
    );
    assert_mentions(
        "dataflows/auth.md",
        AUTH_DATAFLOW,
        &[
            "W9-R1 fixed",
            "W9-R2 fixed",
            "`validate_signup_slug_length`",
        ],
    );
    assert_mentions(
        "docs/openapi.yaml",
        OPENAPI,
        &["Slug must be 3-50 characters"],
    );
    assert_mentions("CHANGELOG.md", CHANGELOG, &["(W9-R1)", "(W9-R2)"]);
}
