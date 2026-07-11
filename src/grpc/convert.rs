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

/// Convert protobuf Timestamp to chrono DateTime
pub fn timestamp_to_datetime(ts: Timestamp) -> Option<DateTime<Utc>> {
    Utc.timestamp_opt(ts.seconds, ts.nanos as u32).single()
}

/// Convert optional protobuf Timestamp to optional chrono DateTime
pub fn timestamp_to_datetime_opt(ts: Option<Timestamp>) -> Option<DateTime<Utc>> {
    ts.and_then(timestamp_to_datetime)
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
        Some(Kind::NumberValue(n)) => {
            serde_json::json!(n)
        }
        Some(Kind::StringValue(s)) => serde_json::Value::String(s.clone()),
        Some(Kind::ListValue(list)) => {
            let arr: Vec<serde_json::Value> = list.values.iter().map(value_to_json).collect();
            serde_json::Value::Array(arr)
        }
        Some(Kind::StructValue(s)) => struct_to_json(s),
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
}
