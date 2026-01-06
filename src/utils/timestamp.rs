use chrono::Utc;

pub fn current_timestamp() -> String {
    Utc::now().to_rfc3339()
}

pub fn current_timestamp_millis() -> i64 {
    Utc::now().timestamp_millis()
}