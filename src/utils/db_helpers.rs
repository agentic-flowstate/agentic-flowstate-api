use aws_sdk_dynamodb::types::AttributeValue;
use std::collections::HashMap;
use uuid::Uuid;

// Attribute value constructors
pub fn string_attr(s: &str) -> AttributeValue {
    AttributeValue::S(s.to_string())
}

pub fn number_attr(n: i64) -> AttributeValue {
    AttributeValue::N(n.to_string())
}

pub fn list_attr(items: Vec<String>) -> AttributeValue {
    AttributeValue::L(items.into_iter().map(|s| string_attr(&s)).collect())
}

// Key constructors
pub fn epic_pk(epic_id: &str) -> AttributeValue {
    string_attr(&format!("EPIC#{}", epic_id))
}

pub fn slice_pk(slice_id: &str) -> AttributeValue {
    string_attr(&format!("SLICE#{}", slice_id))
}

pub fn ticket_pk(ticket_id: &str) -> AttributeValue {
    string_attr(&format!("TICKET#{}", ticket_id))
}

pub fn epic_meta_sk() -> AttributeValue {
    string_attr("META")
}

pub fn slice_meta_sk() -> AttributeValue {
    string_attr("META")
}

pub fn ticket_meta_sk() -> AttributeValue {
    string_attr("META")
}

pub fn epic_slice_sk(slice_id: &str) -> AttributeValue {
    string_attr(&format!("SLICE#{}", slice_id))
}

pub fn slice_ticket_sk(ticket_id: &str) -> AttributeValue {
    string_attr(&format!("TICKET#{}", ticket_id))
}

// ID generation
pub fn generate_id(prefix: &str) -> String {
    format!("{}-{}", prefix, Uuid::new_v4().to_string().split('-').next().unwrap())
}

// Item builder helper
pub struct ItemBuilder {
    item: HashMap<String, AttributeValue>,
}

impl ItemBuilder {
    pub fn new() -> Self {
        Self {
            item: HashMap::new(),
        }
    }

    pub fn add_string(&mut self, key: &str, value: &str) -> &mut Self {
        self.item.insert(key.to_string(), string_attr(value));
        self
    }

    pub fn add_number(&mut self, key: &str, value: i64) -> &mut Self {
        self.item.insert(key.to_string(), number_attr(value));
        self
    }

    pub fn add_optional_string(&mut self, key: &str, value: Option<&str>) -> &mut Self {
        if let Some(v) = value {
            self.add_string(key, v);
        }
        self
    }

    pub fn add_string_list(&mut self, key: &str, values: Vec<String>) -> &mut Self {
        if !values.is_empty() {
            self.item.insert(key.to_string(), list_attr(values));
        }
        self
    }

    pub fn build(self) -> HashMap<String, AttributeValue> {
        self.item
    }
}

// Extract helpers
pub fn extract_string(item: &HashMap<String, AttributeValue>, key: &str) -> Option<String> {
    item.get(key).and_then(|v| v.as_s().ok()).cloned()
}

pub fn extract_number(item: &HashMap<String, AttributeValue>, key: &str) -> Option<i64> {
    item.get(key)
        .and_then(|v| v.as_n().ok())
        .and_then(|n| n.parse().ok())
}

pub fn extract_string_list(item: &HashMap<String, AttributeValue>, key: &str) -> Option<Vec<String>> {
    item.get(key)
        .and_then(|v| v.as_l().ok())
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_s().ok().cloned())
                .collect()
        })
}