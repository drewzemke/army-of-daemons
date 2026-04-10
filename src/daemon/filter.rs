use crate::config::FilterRule;
use serde_json::Value;

/// apply filter rules to a list of items, keeping only those that pass all rules
pub fn apply_rules(items: Vec<Value>, rules: &[FilterRule]) -> Vec<Value> {
    if rules.is_empty() {
        return items;
    }
    items.into_iter().filter(|item| passes_all(item, rules)).collect()
}

fn passes_all(item: &Value, rules: &[FilterRule]) -> bool {
    rules.iter().all(|rule| passes_rule(item, rule))
}

fn passes_rule(item: &Value, rule: &FilterRule) -> bool {
    let field_val = resolve_field(item, &rule.field);

    if rule.op == "exists" {
        return field_val.is_some();
    }

    let field_val = match field_val {
        Some(v) => v,
        None => return false,
    };

    let rule_val = match &rule.value {
        Some(v) => v,
        None => return false,
    };

    match rule.op.as_str() {
        "eq" => values_equal(field_val, rule_val),
        "neq" => !values_equal(field_val, rule_val),
        "gt" => numeric_cmp(field_val, rule_val).is_some_and(|o| o == std::cmp::Ordering::Greater),
        "gte" => numeric_cmp(field_val, rule_val).is_some_and(|o| o != std::cmp::Ordering::Less),
        "lt" => numeric_cmp(field_val, rule_val).is_some_and(|o| o == std::cmp::Ordering::Less),
        "lte" => numeric_cmp(field_val, rule_val).is_some_and(|o| o != std::cmp::Ordering::Greater),
        "contains" => {
            let haystack = json_to_string(field_val);
            let needle = toml_to_string(rule_val);
            haystack.contains(&needle)
        }
        "matches" => {
            let s = json_to_string(field_val);
            let pat = toml_to_string(rule_val);
            regex::Regex::new(&pat).is_ok_and(|re| re.is_match(&s))
        }
        _ => {
            tracing::warn!("unknown filter op: {}", rule.op);
            false
        }
    }
}

/// resolve a dot-notation field path into a JSON value
fn resolve_field<'a>(item: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = item;
    for part in path.split('.') {
        match current {
            Value::Object(map) => {
                current = map.get(part)?;
            }
            Value::Array(arr) => {
                let idx: usize = part.parse().ok()?;
                current = arr.get(idx)?;
            }
            _ => return None,
        }
    }
    Some(current)
}

fn json_to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

fn toml_to_f64(v: &toml::Value) -> Option<f64> {
    match v {
        toml::Value::Integer(n) => Some(*n as f64),
        toml::Value::Float(f) => Some(*f),
        toml::Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

fn json_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn toml_to_string(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(n) => n.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        other => other.to_string(),
    }
}

fn values_equal(json_val: &Value, toml_val: &toml::Value) -> bool {
    // try numeric comparison first
    if let (Some(a), Some(b)) = (json_to_f64(json_val), toml_to_f64(toml_val)) {
        return (a - b).abs() < f64::EPSILON;
    }
    // fall back to string comparison
    json_to_string(json_val) == toml_to_string(toml_val)
}

fn numeric_cmp(json_val: &Value, toml_val: &toml::Value) -> Option<std::cmp::Ordering> {
    let a = json_to_f64(json_val)?;
    let b = toml_to_f64(toml_val)?;
    a.partial_cmp(&b)
}
