//! The numeric-index subset of SurrealDB 3.2.4 query results used by Facial.
use std::collections::HashMap;
use surrealdb::types::{SurrealValue, Value};

pub struct Response {
    pub(super) results: Vec<Option<Result<Value, String>>>,
}
pub trait TakeResult: Sized {
    fn from_result(value: Value) -> Result<Self, String>;
    fn missing_result() -> Result<Self, String> {
        Self::from_result(Value::None)
    }
}
impl<T: SurrealValue> TakeResult for Vec<T> {
    fn missing_result() -> Result<Self, String> {
        Ok(Vec::new())
    }
    fn from_result(value: Value) -> Result<Self, String> {
        let values = match value {
            Value::Array(values) => values.into_vec(),
            value => vec![value],
        };
        values
            .into_iter()
            .map(|value| T::from_value(value).map_err(|e| e.to_string()))
            .collect()
    }
}
impl<T: SurrealValue> TakeResult for Option<T> {
    fn from_result(value: Value) -> Result<Self, String> {
        let value = match value {
            Value::Array(values) => {
                let mut values = values.into_vec();
                if values.len() > 1 {
                    return Err("query contains multiple results for Option".into());
                }
                values.pop().unwrap_or(Value::None)
            }
            value => value,
        };
        if matches!(value, Value::None) {
            Ok(None)
        } else {
            T::from_value(value).map(Some).map_err(|e| e.to_string())
        }
    }
}
impl TakeResult for Value {
    fn from_result(value: Value) -> Result<Self, String> {
        Ok(value)
    }
}
impl TakeResult for serde_json::Value {
    fn from_result(value: Value) -> Result<Self, String> {
        Self::from_value(value).map_err(|e| e.to_string())
    }
}
impl Response {
    pub fn take<T: TakeResult>(&mut self, index: usize) -> Result<T, String> {
        let Some(value) = self.results.get_mut(index).and_then(Option::take) else {
            return T::missing_result();
        };
        let value = value?;
        T::from_result(value)
    }
    pub fn num_statements(&self) -> usize {
        self.results
            .iter()
            .filter(|result| result.is_some())
            .count()
    }
    pub fn check(self) -> Result<Self, String> {
        if let Some(error) = self
            .results
            .iter()
            .flatten()
            .find_map(|result| result.as_ref().err())
        {
            return Err(error.clone());
        }
        Ok(self)
    }
    pub fn take_errors(&mut self) -> HashMap<usize, String> {
        self.results
            .iter_mut()
            .enumerate()
            .filter_map(|(index, result)| {
                if matches!(result, Some(Err(_))) {
                    match result.take() {
                        Some(Err(error)) => Some((index, error)),
                        _ => None,
                    }
                } else {
                    None
                }
            })
            .collect()
    }
}
