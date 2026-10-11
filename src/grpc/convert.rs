//! Conversion utilities between database models and protobuf types
//!
//! Handles mapping of:
//! - Timestamps: chrono::DateTime <-> prost_types::Timestamp
//! - JSON values: serde_json::Value <-> google.protobuf.Struct
//! - Enums: JobStatus string <-> proto JobStatus

use chrono::{DateTime, TimeZone, Utc};
use prost_types::{value::Kind, ListValue, Struct, Timestamp, Value};
use std::collections::{BTreeMap, HashMap};

use crate::grpc::proto;
use crate::models::Job;

/// Convert chrono DateTime to protobuf Timestamp
pub fn datetime_to_timestamp(dt: DateTime<Utc>) -> Timestamp {
    Timestamp {
        seconds: dt.timestamp(),
        nanos: dt.timestamp_subsec_nanos() as i32,
    }
}

/// Convert optional chrono DateTime to optional protobuf Timestamp
pub fn datetime_to_timestamp_opt(dt: Option<DateTime<Utc>>) -> Option<Timestamp> {
    dt.map(datetime_to_timestamp)
}

/// First second a `google.protobuf.Timestamp` may hold (0001-01-01T00:00:00Z).
const MIN_TIMESTAMP_SECONDS: i64 = -62_135_596_800;

/// Last second a `google.protobuf.Timestamp` may hold (9999-12-31T23:59:59Z).
const MAX_TIMESTAMP_SECONDS: i64 = 253_402_300_799;

/// Convert protobuf Timestamp to chrono DateTime.
///
/// `None` for a value outside the Timestamp spec: seconds outside years
/// 1-9999 (chrono alone accepts years far past 9999) or nanos outside `0..1e9`.
pub fn timestamp_to_datetime(ts: Timestamp) -> Option<DateTime<Utc>> {
    if !(MIN_TIMESTAMP_SECONDS..=MAX_TIMESTAMP_SECONDS).contains(&ts.seconds)
        || !(0..1_000_000_000).contains(&ts.nanos)
    {
        return None;
    }
    Utc.timestamp_opt(ts.seconds, ts.nanos as u32).single()
}

/// Read an optional request Timestamp: absent, or the all-zero message some
/// clients send for "unset", is `Ok(None)`; a value outside the Timestamp
/// spec is `Err` so the caller can reject it instead of dropping it.
pub fn timestamp_to_datetime_opt(ts: Option<Timestamp>) -> Result<Option<DateTime<Utc>>, String> {
    match ts {
        None
        | Some(Timestamp {
            seconds: 0,
            nanos: 0,
        }) => Ok(None),
        Some(ts) => timestamp_to_datetime(ts).map(Some).ok_or_else(|| {
            format!(
                "Invalid timestamp (seconds={}, nanos={}): must be between \
                 0001-01-01T00:00:00Z and 9999-12-31T23:59:59Z with nanos in 0..999999999",
                ts.seconds, ts.nanos
            )
        }),
    }
}

/// Convert serde_json::Value to protobuf Struct
pub fn json_to_struct(value: &serde_json::Value) -> Option<Struct> {
    match value {
        serde_json::Value::Object(map) => {
            let fields: BTreeMap<String, Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), json_to_value(v)))
                .collect();
            Some(Struct { fields })
        }
        _ => None,
    }
}

/// Convert serde_json::Value to protobuf Value
pub fn json_to_value(value: &serde_json::Value) -> Value {
    let kind = match value {
        serde_json::Value::Null => Kind::NullValue(0),
        serde_json::Value::Bool(b) => Kind::BoolValue(*b),
        serde_json::Value::Number(n) => Kind::NumberValue(n.as_f64().unwrap_or(0.0)),
        serde_json::Value::String(s) => Kind::StringValue(s.clone()),
        serde_json::Value::Array(arr) => {
            let values: Vec<Value> = arr.iter().map(json_to_value).collect();
            Kind::ListValue(ListValue { values })
        }
        serde_json::Value::Object(map) => {
            let fields: BTreeMap<String, Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), json_to_value(v)))
                .collect();
            Kind::StructValue(Struct { fields })
        }
    };

    Value { kind: Some(kind) }
}

/// Convert protobuf Struct to serde_json::Value
pub fn struct_to_json(s: &Struct) -> serde_json::Value {
    let map: serde_json::Map<String, serde_json::Value> = s
        .fields
        .iter()
        .map(|(k, v)| (k.clone(), value_to_json(v)))
        .collect();
    serde_json::Value::Object(map)
}

/// Convert optional protobuf Struct to serde_json::Value
pub fn struct_to_json_opt(s: Option<&Struct>) -> serde_json::Value {
    match s {
        Some(s) => struct_to_json(s),
        None => serde_json::Value::Object(serde_json::Map::new()),
    }
}

/// Convert protobuf Value to serde_json::Value
pub fn value_to_json(v: &Value) -> serde_json::Value {
    match &v.kind {
        None => serde_json::Value::Null,
        Some(Kind::NullValue(_)) => serde_json::Value::Null,
        Some(Kind::BoolValue(b)) => serde_json::Value::Bool(*b),
        Some(Kind::NumberValue(n)) => number_to_json(*n),
        Some(Kind::StringValue(s)) => serde_json::Value::String(s.clone()),
        Some(Kind::ListValue(list)) => {
            let arr: Vec<serde_json::Value> = list.values.iter().map(value_to_json).collect();
            serde_json::Value::Array(arr)
        }
        Some(Kind::StructValue(s)) => struct_to_json(s),
    }
}

/// 2^53: above this an f64 no longer holds every integer exactly.
const MAX_SAFE_INTEGER_F64: f64 = 9_007_199_254_740_992.0;

/// A Struct number is always a double. Whole values that a double holds
/// exactly come back as JSON integers, so `{"count": 42}` round-trips as `42`
/// and not `42.0` (which typed decoders in Go, Python and PHP reject as an int).
fn number_to_json(n: f64) -> serde_json::Value {
    if n.fract() == 0.0 && n.abs() < MAX_SAFE_INTEGER_F64 {
        serde_json::Value::from(n as i64)
    } else {
        serde_json::json!(n)
    }
}

/// Convert string map to protobuf Struct (for tags)
pub fn map_to_struct(map: &HashMap<String, String>) -> Struct {
    let fields: BTreeMap<String, Value> = map
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                Value {
                    kind: Some(Kind::StringValue(v.clone())),
                },
            )
        })
        .collect();
    Struct { fields }
}

/// Convert protobuf Struct to string map
pub fn struct_to_map(s: &Struct) -> HashMap<String, String> {
    s.fields
        .iter()
        .filter_map(|(k, v)| match &v.kind {
            Some(Kind::StringValue(s)) => Some((k.clone(), s.clone())),
            _ => None,
        })
        .collect()
}

/// Convert database job status string to proto JobStatus
pub fn status_to_proto(status: &str) -> proto::JobStatus {
    match status.to_lowercase().as_str() {
        "pending" => proto::JobStatus::Pending,
        "scheduled" => proto::JobStatus::Scheduled,
        "processing" => proto::JobStatus::Processing,
        "completed" => proto::JobStatus::Completed,
        "failed" => proto::JobStatus::Failed,
        "deadletter" => proto::JobStatus::Deadletter,
        "cancelled" => proto::JobStatus::Cancelled,
        _ => proto::JobStatus::Unspecified,
    }
}

/// Convert proto JobStatus to database status string
pub fn proto_to_status(status: proto::JobStatus) -> &'static str {
    match status {
        proto::JobStatus::Pending => "pending",
        proto::JobStatus::Scheduled => "scheduled",
        proto::JobStatus::Processing => "processing",
        proto::JobStatus::Completed => "completed",
        proto::JobStatus::Failed => "failed",
        proto::JobStatus::Deadletter => "deadletter",
        proto::JobStatus::Cancelled => "cancelled",
        proto::JobStatus::Unspecified => "pending",
    }
}

/// Convert database Job model to proto Job message
pub fn job_to_proto(job: &Job) -> proto::Job {
    proto::Job {
        id: job.id.clone(),
        organization_id: job.organization_id.clone(),
        queue_name: job.queue_name.clone(),
        status: status_to_proto(&job.status).into(),
        payload: json_to_struct(&job.payload),
        result: job.result.as_ref().and_then(json_to_struct),
        retry_count: job.retry_count,
        max_retries: job.max_retries,
        last_error: job.last_error.clone().unwrap_or_default(),
        priority: job.priority,
        timeout_seconds: job.timeout_seconds,
        created_at: Some(datetime_to_timestamp(job.created_at)),
        scheduled_at: datetime_to_timestamp_opt(job.scheduled_at),
        started_at: datetime_to_timestamp_opt(job.started_at),
        completed_at: datetime_to_timestamp_opt(job.completed_at),
        lease_expires_at: datetime_to_timestamp_opt(job.lease_expires_at),
        assigned_worker_id: job.assigned_worker_id.clone().unwrap_or_default(),
        idempotency_key: job.idempotency_key.clone().unwrap_or_default(),
        lease_id: job.lease_id.clone().unwrap_or_default(),
    }
}

/// Convert list of database Jobs to proto Jobs
pub fn jobs_to_proto(jobs: &[Job]) -> Vec<proto::Job> {
    jobs.iter().map(job_to_proto).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_datetime_to_timestamp() {
        let dt = Utc::now();
        let ts = datetime_to_timestamp(dt);
        let back = timestamp_to_datetime(ts).unwrap();

        // Allow 1 microsecond difference due to precision
        assert!((dt.timestamp_millis() - back.timestamp_millis()).abs() < 2);
    }

    #[test]
    fn test_json_to_struct_roundtrip() {
        let json = serde_json::json!({
            "name": "test",
            "count": 42,
            "active": true,
            "tags": ["a", "b"],
            "nested": {"key": "value"}
        });

        let proto_struct = json_to_struct(&json).unwrap();
        let back = struct_to_json(&proto_struct);

        assert_eq!(json["name"], back["name"]);
        assert_eq!(json["active"], back["active"]);
        assert_eq!(json["tags"], back["tags"]);
    }

    #[test]
    fn test_status_conversion() {
        assert_eq!(status_to_proto("pending"), proto::JobStatus::Pending);
        assert_eq!(status_to_proto("COMPLETED"), proto::JobStatus::Completed);
        assert_eq!(status_to_proto("invalid"), proto::JobStatus::Unspecified);

        assert_eq!(proto_to_status(proto::JobStatus::Processing), "processing");
        assert_eq!(proto_to_status(proto::JobStatus::Deadletter), "deadletter");
    }

    #[test]
    fn test_map_to_struct() {
        let mut map = HashMap::new();
        map.insert("key1".to_string(), "value1".to_string());
        map.insert("key2".to_string(), "value2".to_string());

        let s = map_to_struct(&map);
        let back = struct_to_map(&s);

        assert_eq!(map, back);
    }

    // BEGRPC-05: whole numbers survive a Struct round trip as JSON integers.
    #[test]
    fn test_struct_round_trip_keeps_integers() {
        let json = serde_json::json!({
            "count": 42,
            "negative": -7,
            "zero": 0,
            "ratio": 1.5,
            "below_2_53": 9_007_199_254_740_991_i64,
            "nested": {"qty": 3, "items": [1, 2.25, -4]}
        });

        let back = struct_to_json(&json_to_struct(&json).unwrap());

        assert_eq!(back, json);
        assert_eq!(back.to_string(), json.to_string());
        assert!(back["count"].is_i64(), "{}", back["count"]);
        assert!(back["ratio"].is_f64());
    }

    // BEGRPC-05: at 2^53 a double stops holding every integer, so the value
    // stays a float instead of claiming an exactness it does not have.
    #[test]
    fn test_number_to_json_boundary() {
        let two_53 = 9_007_199_254_740_992.0_f64;
        let number = |n: f64| value_to_json(&json_to_value(&serde_json::json!(n)));

        assert_eq!(
            number(two_53 - 1.0),
            serde_json::json!(9_007_199_254_740_991_i64)
        );
        assert_eq!(
            number(-(two_53 - 1.0)),
            serde_json::json!(-9_007_199_254_740_991_i64)
        );
        assert!(number(two_53).is_f64());
        assert!(number(-two_53).is_f64());
        assert!(number(1e300).is_f64());
        assert_eq!(number(-0.5), serde_json::json!(-0.5));
        let nan = Value {
            kind: Some(Kind::NumberValue(f64::NAN)),
        };
        assert_eq!(value_to_json(&nan), serde_json::Value::Null);
    }

    // BEGRPC-21: values outside the Timestamp spec do not convert.
    #[test]
    fn test_timestamp_to_datetime_rejects_out_of_spec_values() {
        let ts = |seconds: i64, nanos: i32| Timestamp { seconds, nanos };

        assert!(timestamp_to_datetime(ts(1_700_000_000, 1_000_000_000)).is_none());
        assert!(timestamp_to_datetime(ts(1_700_000_000, -1)).is_none());
        assert!(timestamp_to_datetime(ts(253_402_300_800, 0)).is_none());
        assert!(timestamp_to_datetime(ts(-62_135_596_801, 0)).is_none());

        assert!(timestamp_to_datetime(ts(253_402_300_799, 999_999_999)).is_some());
        assert!(timestamp_to_datetime(ts(-62_135_596_800, 0)).is_some());
        assert_eq!(
            timestamp_to_datetime(ts(1_700_000_000, 5))
                .unwrap()
                .timestamp(),
            1_700_000_000
        );
    }
}
