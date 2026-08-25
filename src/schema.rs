use cql3_parser::common::{DataTypeName, Identifier};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColumnType {
    Int,
    BigInt,
    SmallInt,
    TinyInt,
    Float,
    Double,
    Boolean,
    Text,
    Blob,
    Timestamp,
    Uuid,
    Inet,
    Set(Box<ColumnType>),
    List(Box<ColumnType>),
    Map(Box<ColumnType>, Box<ColumnType>),
}

impl ColumnType {
    pub fn wire_id(&self) -> u16 {
        match self {
            ColumnType::Int => 0x0009,
            ColumnType::BigInt => 0x0002,
            ColumnType::SmallInt => 0x0013,
            ColumnType::TinyInt => 0x0014,
            ColumnType::Float => 0x0008,
            ColumnType::Double => 0x0007,
            ColumnType::Boolean => 0x0004,
            ColumnType::Text => 0x000D,
            ColumnType::Blob => 0x0003,
            ColumnType::Timestamp => 0x000B,
            ColumnType::Uuid => 0x000C,
            ColumnType::Inet => 0x0010,
            ColumnType::Set(_) => 0x0022,
            ColumnType::List(_) => 0x0020,
            ColumnType::Map(_, _) => 0x0021,
        }
    }

    pub fn write_wire_type(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&self.wire_id().to_be_bytes());
        match self {
            ColumnType::Set(elem) | ColumnType::List(elem) => {
                buf.extend_from_slice(&elem.wire_id().to_be_bytes());
            }
            ColumnType::Map(k, v) => {
                buf.extend_from_slice(&k.wire_id().to_be_bytes());
                buf.extend_from_slice(&v.wire_id().to_be_bytes());
            }
            _ => {}
        }
    }

    pub fn cql_string(&self) -> String {
        match self {
            ColumnType::Int => "int".to_string(),
            ColumnType::BigInt => "bigint".to_string(),
            ColumnType::SmallInt => "smallint".to_string(),
            ColumnType::TinyInt => "tinyint".to_string(),
            ColumnType::Float => "float".to_string(),
            ColumnType::Double => "double".to_string(),
            ColumnType::Boolean => "boolean".to_string(),
            ColumnType::Text => "text".to_string(),
            ColumnType::Blob => "blob".to_string(),
            ColumnType::Timestamp => "timestamp".to_string(),
            ColumnType::Uuid => "uuid".to_string(),
            ColumnType::Inet => "inet".to_string(),
            ColumnType::Set(e) => format!("set<{}>", e.cql_string()),
            ColumnType::List(e) => format!("list<{}>", e.cql_string()),
            ColumnType::Map(k, v) => format!("map<{}, {}>", k.cql_string(), v.cql_string()),
        }
    }

    pub fn from_data_type(name: &DataTypeName, definition: &[DataTypeName]) -> Option<ColumnType> {
        match name {
            DataTypeName::Int => Some(ColumnType::Int),
            DataTypeName::BigInt => Some(ColumnType::BigInt),
            DataTypeName::SmallInt => Some(ColumnType::SmallInt),
            DataTypeName::TinyInt => Some(ColumnType::TinyInt),
            DataTypeName::Float => Some(ColumnType::Float),
            DataTypeName::Double => Some(ColumnType::Double),
            DataTypeName::Boolean => Some(ColumnType::Boolean),
            DataTypeName::Text | DataTypeName::VarChar | DataTypeName::Ascii => {
                Some(ColumnType::Text)
            }
            DataTypeName::Blob => Some(ColumnType::Blob),
            DataTypeName::Timestamp => Some(ColumnType::Timestamp),
            DataTypeName::Uuid | DataTypeName::TimeUuid => Some(ColumnType::Uuid),
            DataTypeName::Inet => Some(ColumnType::Inet),
            DataTypeName::Set | DataTypeName::List | DataTypeName::Map | DataTypeName::Frozen => {
                let elem = |i: usize| ColumnType::from_data_type(definition.get(i)?, &[]);
                match name {
                    DataTypeName::Set => Some(ColumnType::Set(Box::new(elem(0)?))),
                    DataTypeName::List => Some(ColumnType::List(Box::new(elem(0)?))),
                    DataTypeName::Map => {
                        Some(ColumnType::Map(Box::new(elem(0)?), Box::new(elem(1)?)))
                    }
                    DataTypeName::Frozen => {
                        let inner = definition.first()?;
                        match inner {
                            DataTypeName::Set => Some(ColumnType::Set(Box::new(elem(1)?))),
                            DataTypeName::List => Some(ColumnType::List(Box::new(elem(1)?))),
                            DataTypeName::Map => {
                                Some(ColumnType::Map(Box::new(elem(1)?), Box::new(elem(2)?)))
                            }
                            other => ColumnType::from_data_type(other, &[]),
                        }
                    }
                    _ => unreachable!(),
                }
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnDef {
    pub name: String,
    pub col_type: ColumnType,
    pub is_partition_key: bool,
    pub is_clustering: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableDef {
    pub keyspace: String,
    pub name: String,
    pub columns: Vec<ColumnDef>,
    pub partition_keys: Vec<String>,
    pub clustering_keys: Vec<String>,
}

impl TableDef {
    pub fn column(&self, name: &str) -> Option<&ColumnDef> {
        self.columns.iter().find(|c| c.name == name)
    }

    pub fn is_partition(&self, name: &str) -> bool {
        self.partition_keys.iter().any(|p| p == name)
    }

    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyspaceDef {
    pub name: String,
    pub replication: Vec<(String, String)>,
}

pub fn norm_id(id: &Identifier) -> String {
    match id {
        Identifier::Quoted(s) => s.clone(),
        Identifier::Unquoted(s) => s.to_lowercase(),
    }
}

/// Case-sensitive for quoted identifiers, ASCII-case-insensitive for plain
/// ones — mirrors [`norm_id`] without allocating.
pub fn ident_eq(id: &Identifier, name: &str) -> bool {
    match id {
        Identifier::Quoted(s) => s == name,
        Identifier::Unquoted(s) => s.eq_ignore_ascii_case(name),
    }
}

pub fn build_table_def(
    keyspace: &str,
    table_name: &str,
    create: &cql3_parser::create_table::CreateTable,
) -> Option<TableDef> {
    let mut partition_keys: Vec<String> = Vec::new();
    let mut clustering_keys: Vec<String> = Vec::new();
    if let Some(key) = &create.key {
        partition_keys = key.partition.iter().map(norm_id).collect();
        clustering_keys = key.clustering.iter().map(norm_id).collect();
    }

    let mut columns = Vec::new();
    for cd in &create.columns {
        let col_type = ColumnType::from_data_type(&cd.data_type.name, &cd.data_type.definition)?;
        let name = norm_id(&cd.name);
        let is_pk = cd.primary_key || partition_keys.contains(&name);
        let is_ck = !is_pk && clustering_keys.contains(&name);
        columns.push(ColumnDef {
            name,
            col_type,
            is_partition_key: is_pk,
            is_clustering: is_ck,
        });
    }

    if partition_keys.is_empty() {
        partition_keys = columns
            .iter()
            .filter(|c| c.is_partition_key)
            .map(|c| c.name.clone())
            .collect();
    }

    Some(TableDef {
        keyspace: keyspace.to_string(),
        name: table_name.to_string(),
        columns,
        partition_keys,
        clustering_keys,
    })
}
