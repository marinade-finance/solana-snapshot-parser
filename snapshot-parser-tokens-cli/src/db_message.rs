use rusqlite::ToSql;
use tokio::sync::oneshot;

pub enum DbMessage {
    Execute {
        query: String,
        params: Vec<Box<dyn ToSql + Send + Sync>>,
        response: oneshot::Sender<anyhow::Result<usize>>,
    },
    ExecuteSpecial {
        query: String,
        params: Vec<Box<dyn ToSql + Send + Sync>>,
        response: oneshot::Sender<anyhow::Result<usize>>,
    },
    Shutdown {
        response: oneshot::Sender<anyhow::Result<()>>,
    },
}

#[derive(Clone)]
pub enum OwnedSqlValue {
    Text(Option<String>),
    Integer(Option<i64>),
    UnsignedInteger(Option<u64>),
    UnsignedU16(Option<u16>),
    Boolean(Option<bool>),
    U8(Option<u8>),
}

impl ToSql for OwnedSqlValue {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        match self {
            OwnedSqlValue::Text(opt) => opt.to_sql(),
            OwnedSqlValue::Integer(opt) => opt.to_sql(),
            OwnedSqlValue::UnsignedInteger(opt) => opt.to_sql(),
            OwnedSqlValue::UnsignedU16(opt) => opt.to_sql(),
            OwnedSqlValue::Boolean(opt) => opt.to_sql(),
            OwnedSqlValue::U8(opt) => opt.to_sql(),
        }
    }
}

impl OwnedSqlValue {
    // Helper method to create a boxed value
    pub fn boxed<T: Into<OwnedSqlValue>>(value: T) -> Box<dyn ToSql + Send + Sync> {
        Box::new(value.into())
    }
}

impl From<String> for OwnedSqlValue {
    fn from(s: String) -> Self {
        OwnedSqlValue::Text(Some(s))
    }
}

impl From<&str> for OwnedSqlValue {
    fn from(s: &str) -> Self {
        OwnedSqlValue::Text(Some(s.to_string()))
    }
}

impl From<i64> for OwnedSqlValue {
    fn from(i: i64) -> Self {
        OwnedSqlValue::Integer(Some(i))
    }
}

impl From<u64> for OwnedSqlValue {
    fn from(i: u64) -> Self {
        OwnedSqlValue::UnsignedInteger(Some(i))
    }
}

impl From<u16> for OwnedSqlValue {
    fn from(i: u16) -> Self {
        OwnedSqlValue::UnsignedU16(Some(i))
    }
}

impl From<bool> for OwnedSqlValue {
    fn from(b: bool) -> Self {
        OwnedSqlValue::Boolean(Some(b))
    }
}

impl From<u8> for OwnedSqlValue {
    fn from(b: u8) -> Self {
        OwnedSqlValue::U8(Some(b))
    }
}

impl From<Option<String>> for OwnedSqlValue {
    fn from(s: Option<String>) -> Self {
        OwnedSqlValue::Text(s)
    }
}

impl From<Option<&str>> for OwnedSqlValue {
    fn from(s: Option<&str>) -> Self {
        OwnedSqlValue::Text(s.map_or_else(|| None, |s| Some(s.to_string())))
    }
}

impl From<Option<i64>> for OwnedSqlValue {
    fn from(i: Option<i64>) -> Self {
        OwnedSqlValue::Integer(i)
    }
}

impl From<Option<u64>> for OwnedSqlValue {
    fn from(i: Option<u64>) -> Self {
        OwnedSqlValue::UnsignedInteger(i)
    }
}

impl From<Option<u16>> for OwnedSqlValue {
    fn from(i: Option<u16>) -> Self {
        OwnedSqlValue::UnsignedU16(i)
    }
}

impl From<Option<bool>> for OwnedSqlValue {
    fn from(b: Option<bool>) -> Self {
        OwnedSqlValue::Boolean(b)
    }
}

impl From<Option<u8>> for OwnedSqlValue {
    fn from(b: Option<u8>) -> Self {
        OwnedSqlValue::U8(b)
    }
}

#[macro_export]
macro_rules! sql_params {
    ($($value:expr),* $(,)?) => {{
        vec![
            $(Box::new(Into::<OwnedSqlValue>::into($value)) as Box<dyn ToSql + Send + Sync>,)*
        ]
    }};
}
