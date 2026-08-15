pub use crate::cql_value::Value;
pub use crate::schema::ColumnType;
use crate::schema::{KeyspaceDef, TableDef, build_table_def, norm_id};
use crate::storage::{Storage, decode_key, decode_row_columns, encode_row_columns};
use cql3_parser::cassandra_ast::CassandraAST;
use cql3_parser::cassandra_statement::CassandraStatement;
use cql3_parser::common::{FQName, Operand, RelationOperator};
use cql3_parser::select::SelectElement;
use scylla_cql::frame::types::RawValue;
use std::collections::HashMap;

fn strip_quotes(s: &str) -> String {
    s.trim()
        .trim_start_matches('\'')
        .trim_end_matches('\'')
        .to_string()
}

#[derive(Debug)]
pub struct QueryError {
    pub code: i32,
    pub message: String,
}

fn syntax_err(msg: impl Into<String>) -> QueryError {
    QueryError {
        code: 0x2000,
        message: msg.into(),
    }
}

fn invalid_err(msg: impl Into<String>) -> QueryError {
    QueryError {
        code: 0x2200,
        message: msg.into(),
    }
}

fn already_exists_err(keyspace: &str, table: &str) -> QueryError {
    QueryError {
        code: 0x2400,
        message: format!("{keyspace}.{table} already exists"),
    }
}

fn internal_err(e: anyhow::Error) -> QueryError {
    QueryError {
        code: 0x0000,
        message: format!("internal error: {e}"),
    }
}

const SYSTEM_KEYSPACES: [&str; 2] = ["system", "system_schema"];

fn check_writable(keyspace: &str) -> Result<(), QueryError> {
    if SYSTEM_KEYSPACES.contains(&keyspace) {
        return Err(invalid_err(format!(
            "Modifications to keyspace '{keyspace}' are not allowed"
        )));
    }
    Ok(())
}

fn put_system_schema_row(
    storage: &Storage,
    table: &str,
    pk: &[&str],
    cols: &[(&str, Value)],
) -> Result<(), anyhow::Error> {
    let def = storage
        .get_table("system_schema", table)?
        .ok_or_else(|| anyhow::anyhow!("system_schema.{table} missing"))?;
    let mut pk_parts = Vec::new();
    for p in pk {
        pk_parts.push(p.as_bytes().to_vec());
    }
    let mut row = Vec::new();
    for (name, v) in cols {
        let idx = def
            .column_index(name)
            .ok_or_else(|| anyhow::anyhow!("column {name} missing"))?;
        row.push((idx as u16, v.clone()));
    }
    storage.put_row("system_schema", table, &pk_parts, &encode_row_columns(&row))
}

fn delete_system_schema_partition(
    storage: &Storage,
    table: &str,
    partition_key: &[Vec<u8>],
) -> Result<(), anyhow::Error> {
    for (key, _) in storage.scan_partition("system_schema", table, partition_key)? {
        storage.delete(&key)?;
    }
    Ok(())
}

#[derive(Debug)]
pub enum Response {
    Rows {
        keyspace: String,
        table: String,
        cols: Vec<(String, ColumnType)>,
        rows: Vec<Vec<Value>>,
    },
    Void,
    SetKeyspace(String),
    SchemaChange {
        change: &'static str,
        target: &'static str,
        keyspace: String,
        table: String,
    },
}

#[derive(Debug, Clone)]
pub struct PreparedInfo {
    pub keyspace: String,
    pub table: String,
    pub bind_specs: Vec<(String, ColumnType)>,
    pub pk_indexes: Vec<u16>,
    pub result_cols: Vec<(String, ColumnType)>,
}

pub fn prepared_info(
    storage: &Storage,
    session_ks: &Option<String>,
    stmt: &CassandraStatement,
) -> Result<PreparedInfo, QueryError> {
    match stmt {
        CassandraStatement::Insert(i) => {
            let (keyspace, table) = resolve_table(storage, session_ks, &i.table_name)?;
            let mut bind_specs = Vec::new();
            for col_id in &i.columns {
                let name = norm_id(col_id);
                let col = table
                    .column(&name)
                    .ok_or_else(|| invalid_err(format!("Undefined column name {name}")))?;
                bind_specs.push((name, col.col_type.clone()));
            }
            let mut pk_indexes = Vec::new();
            for pk in table
                .partition_keys
                .iter()
                .chain(table.clustering_keys.iter())
            {
                if let Some(pos) = i.columns.iter().position(|c| norm_id(c) == *pk) {
                    pk_indexes.push(pos as u16);
                }
            }
            Ok(PreparedInfo {
                keyspace,
                table: table.name,
                bind_specs,
                pk_indexes,
                result_cols: vec![],
            })
        }
        CassandraStatement::Update(u) => {
            let (keyspace, table) = resolve_table(storage, session_ks, &u.table_name)?;
            let mut bind_specs = Vec::new();
            for assignment in &u.assignments {
                let name = norm_id(&assignment.name.column);
                let col = table
                    .column(&name)
                    .ok_or_else(|| invalid_err(format!("Undefined column name {name}")))?;
                bind_specs.push((name, col.col_type.clone()));
            }
            collect_where_binds(&table, &u.where_clause, &mut bind_specs)?;
            Ok(PreparedInfo {
                keyspace,
                table: table.name,
                bind_specs,
                pk_indexes: vec![],
                result_cols: vec![],
            })
        }
        CassandraStatement::Select(s) => {
            let (keyspace, table) = resolve_table(storage, session_ks, &s.table_name)?;
            let mut bind_specs = Vec::new();
            collect_where_binds(&table, &s.where_clause, &mut bind_specs)?;
            let result_cols = select_columns(&table, &s.columns)?;
            Ok(PreparedInfo {
                keyspace,
                table: table.name,
                bind_specs,
                pk_indexes: vec![],
                result_cols,
            })
        }
        CassandraStatement::Delete(d) => {
            let (keyspace, table) = resolve_table(storage, session_ks, &d.table_name)?;
            let mut bind_specs = Vec::new();
            collect_where_binds(&table, &d.where_clause, &mut bind_specs)?;
            Ok(PreparedInfo {
                keyspace,
                table: table.name,
                bind_specs,
                pk_indexes: vec![],
                result_cols: vec![],
            })
        }
        other => Err(invalid_err(format!(
            "Statement cannot be prepared: {}",
            other.short_name()
        ))),
    }
}

fn collect_where_binds(
    table: &TableDef,
    where_clause: &[cql3_parser::common::RelationElement],
    out: &mut Vec<(String, ColumnType)>,
) -> Result<(), QueryError> {
    for rel in where_clause {
        let Operand::Column(col) = &rel.obj else {
            return Err(invalid_err(
                "Only column restrictions are supported in WHERE",
            ));
        };
        let name = norm_id(col);
        let col_type = table
            .column(&name)
            .ok_or_else(|| invalid_err(format!("Undefined column name {name}")))?
            .col_type
            .clone();
        match &rel.value {
            Operand::Param(_) => out.push((name, col_type)),
            Operand::Tuple(items) => {
                for item in items {
                    if let Operand::Param(_) = item {
                        out.push((name.clone(), col_type.clone()));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn select_columns(
    table: &TableDef,
    elems: &[SelectElement],
) -> Result<Vec<(String, ColumnType)>, QueryError> {
    let mut all_columns = false;
    let mut selected: Vec<(String, ColumnType)> = Vec::new();
    for elem in elems {
        match elem {
            SelectElement::Star => all_columns = true,
            SelectElement::Column(named) => {
                let name = norm_id(&named.name);
                let col = table
                    .column(&name)
                    .ok_or_else(|| invalid_err(format!("Undefined column name {name}")))?;
                selected.push((norm_id(named.alias_or_name()), col.col_type.clone()));
            }
            SelectElement::Function(_) => {
                return Err(invalid_err("Functions in SELECT are not supported"));
            }
        }
    }
    if all_columns {
        selected = table
            .columns
            .iter()
            .map(|c| (c.name.clone(), c.col_type.clone()))
            .collect();
    }
    Ok(selected)
}

pub fn parse_cached(
    storage: &Storage,
    query: &str,
) -> Result<std::sync::Arc<CassandraStatement>, QueryError> {
    if let Some(stmt) = storage.ast_cache_get(query) {
        return Ok(stmt);
    }
    let ast = CassandraAST::new(query);
    if ast.has_error() {
        return Err(syntax_err("line 1: syntax error in statement"));
    }
    let Some(stmt) = ast
        .statements
        .first()
        .filter(|s| !s.has_error)
        .map(|s| s.statement.clone())
    else {
        return Err(syntax_err("line 1: syntax error in statement"));
    };
    let stmt = std::sync::Arc::new(stmt);
    storage.ast_cache_put(query, stmt.clone());
    Ok(stmt)
}

pub fn execute(
    storage: &Storage,
    session_ks: &mut Option<String>,
    query: &str,
) -> Result<Response, QueryError> {
    let stmt = parse_cached(storage, query)?;
    execute_statement(storage, session_ks, &stmt, &mut Binds::none())
}

pub fn execute_with_values(
    storage: &Storage,
    session_ks: &mut Option<String>,
    query: &str,
    values: &[RawValue],
) -> Result<Response, QueryError> {
    let stmt = parse_cached(storage, query)?;
    execute_statement(storage, session_ks, &stmt, &mut Binds::new(values))
}

pub fn execute_prepared(
    storage: &Storage,
    session_ks: &mut Option<String>,
    stmt: &CassandraStatement,
    values: &[RawValue],
) -> Result<Response, QueryError> {
    execute_statement(storage, session_ks, stmt, &mut Binds::new(values))
}

struct Binds<'a> {
    values: &'a [RawValue<'a>],
    next: usize,
}

impl<'a> Binds<'a> {
    fn none() -> Self {
        Binds {
            values: &[],
            next: 0,
        }
    }

    fn new(values: &'a [RawValue<'a>]) -> Self {
        Binds { values, next: 0 }
    }

    fn take(&mut self, col_type: &ColumnType) -> Result<Value, QueryError> {
        let rv = self
            .values
            .get(self.next)
            .ok_or_else(|| invalid_err("Invalid amount of bind variables"))?;
        self.next += 1;
        match rv.as_value() {
            Some(raw) => Value::from_raw_bytes(raw, col_type)
                .map_err(|e| invalid_err(format!("Invalid bind value: {e}"))),
            None => Ok(Value::Null),
        }
    }
}

fn execute_statement(
    storage: &Storage,
    session_ks: &mut Option<String>,
    stmt: &CassandraStatement,
    binds: &mut Binds,
) -> Result<Response, QueryError> {
    match stmt {
        CassandraStatement::Use(id) => exec_use(storage, session_ks, id),
        CassandraStatement::CreateKeyspace(ks) => exec_create_keyspace(storage, ks),
        CassandraStatement::CreateTable(t) => exec_create_table(storage, session_ks, t),
        CassandraStatement::DropKeyspace(d) => exec_drop_keyspace(storage, d),
        CassandraStatement::DropTable(d) => exec_drop_table(storage, session_ks, d),
        CassandraStatement::Insert(i) => exec_insert(storage, session_ks, i, binds),
        CassandraStatement::Select(s) => exec_select(storage, session_ks, s, binds),
        CassandraStatement::Delete(d) => exec_delete(storage, session_ks, d, binds),
        CassandraStatement::Update(u) => exec_update(storage, session_ks, u, binds),
        CassandraStatement::Truncate(name) => exec_truncate(storage, session_ks, name),
        other => Err(invalid_err(format!(
            "Statement not supported: {}",
            other.short_name()
        ))),
    }
}

fn resolve_table(
    storage: &Storage,
    session_ks: &Option<String>,
    name: &FQName,
) -> Result<(String, TableDef), QueryError> {
    let keyspace = match &name.keyspace {
        Some(k) => norm_id(k),
        None => session_ks
            .clone()
            .ok_or_else(|| invalid_err("No keyspace specified; use USE <keyspace>"))?,
    };
    let table = norm_id(&name.name);
    let def = storage
        .get_table(&keyspace, &table)
        .map_err(internal_err)?
        .ok_or_else(|| invalid_err(format!("table {keyspace}.{table} does not exist")))?;
    Ok((keyspace, def))
}

fn exec_use(
    storage: &Storage,
    session_ks: &mut Option<String>,
    id: &cql3_parser::common::Identifier,
) -> Result<Response, QueryError> {
    let ks = norm_id(id);
    if storage.get_keyspace(&ks).map_err(internal_err)?.is_none() {
        return Err(invalid_err(format!("Keyspace '{ks}' does not exist")));
    }
    *session_ks = Some(ks.clone());
    Ok(Response::SetKeyspace(ks))
}

fn exec_create_keyspace(
    storage: &Storage,
    ks: &cql3_parser::create_keyspace::CreateKeyspace,
) -> Result<Response, QueryError> {
    let name = norm_id(&ks.name);
    if storage.get_keyspace(&name).map_err(internal_err)?.is_some() {
        if ks.if_not_exists {
            return Ok(Response::Void);
        }
        return Err(already_exists_err(&name, ""));
    }
    if SYSTEM_KEYSPACES.contains(&name.as_str()) {
        return Err(invalid_err(format!("Cannot create keyspace '{name}'")));
    }
    let def = KeyspaceDef {
        name: name.clone(),
        replication: ks.replication.clone(),
    };
    storage.put_keyspace(&def).map_err(internal_err)?;
    let replication = def
        .replication
        .iter()
        .map(|(k, v)| (Value::Text(strip_quotes(k)), Value::Text(strip_quotes(v))))
        .collect();
    put_system_schema_row(
        storage,
        "keyspaces",
        &[&name],
        &[
            ("durable_writes", Value::Boolean(true)),
            ("replication", Value::Map(replication)),
        ],
    )
    .map_err(internal_err)?;
    Ok(Response::SchemaChange {
        change: "CREATED",
        target: "KEYSPACE",
        keyspace: name,
        table: String::new(),
    })
}

fn exec_create_table(
    storage: &Storage,
    session_ks: &Option<String>,
    t: &cql3_parser::create_table::CreateTable,
) -> Result<Response, QueryError> {
    let keyspace = match &t.name.keyspace {
        Some(k) => norm_id(k),
        None => session_ks
            .clone()
            .ok_or_else(|| invalid_err("No keyspace specified; use USE <keyspace>"))?,
    };
    let table = norm_id(&t.name.name);
    if storage
        .get_table(&keyspace, &table)
        .map_err(internal_err)?
        .is_some()
    {
        if t.if_not_exists {
            return Ok(Response::Void);
        }
        return Err(already_exists_err(&keyspace, &table));
    }
    check_writable(&keyspace)?;
    let def = build_table_def(&keyspace, &table, t).ok_or_else(|| {
        invalid_err("Unsupported column type; supported types: int, bigint, smallint, tinyint, float, double, boolean, text, varchar, ascii, blob, timestamp, uuid, inet")
    })?;
    if def.partition_keys.is_empty() {
        return Err(invalid_err("Primary key is required"));
    }
    storage.put_table(&def).map_err(internal_err)?;
    mirror_table_metadata(storage, &def).map_err(internal_err)?;
    Ok(Response::SchemaChange {
        change: "CREATED",
        target: "TABLE",
        keyspace,
        table,
    })
}

fn mirror_table_metadata(storage: &Storage, def: &TableDef) -> Result<(), anyhow::Error> {
    put_system_schema_row(
        storage,
        "tables",
        &[&def.keyspace, &def.name],
        &[
            ("bloom_filter_fp_chance", Value::Double(0.01)),
            ("caching", Value::Map(vec![])),
            ("comment", Value::Text(String::new())),
            (
                "compaction",
                Value::Map(vec![(
                    Value::Text("class".to_string()),
                    Value::Text(
                        "org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy"
                            .to_string(),
                    ),
                )]),
            ),
            ("compression", Value::Map(vec![])),
            ("crc_check_chance", Value::Double(1.0)),
            ("dclocal_read_repair_chance", Value::Double(0.1)),
            ("default_time_to_live", Value::Int(0)),
            ("extensions", Value::Map(vec![])),
            (
                "flags",
                Value::Set(vec![Value::Text("compound".to_string())]),
            ),
            ("gc_grace_seconds", Value::Int(864000)),
            ("max_index_interval", Value::Int(2048)),
            ("memtable_flush_period_in_ms", Value::Int(0)),
            ("min_index_interval", Value::Int(128)),
            ("read_repair_chance", Value::Double(0.0)),
            ("speculative_retry", Value::Text("99PERCENTILE".to_string())),
        ],
    )?;
    for c in &def.columns {
        let (kind, position, clustering_order) = if c.is_partition_key {
            (
                "partition_key",
                def.partition_keys
                    .iter()
                    .position(|p| *p == c.name)
                    .unwrap() as i32,
                "NONE",
            )
        } else if c.is_clustering {
            (
                "clustering",
                def.clustering_keys
                    .iter()
                    .position(|p| *p == c.name)
                    .unwrap() as i32,
                "ASC",
            )
        } else {
            ("regular", -1, "NONE")
        };
        put_system_schema_row(
            storage,
            "columns",
            &[&def.keyspace, &def.name, &c.name],
            &[
                (
                    "clustering_order",
                    Value::Text(clustering_order.to_string()),
                ),
                ("column_name_bytes", Value::Blob(c.name.as_bytes().to_vec())),
                ("kind", Value::Text(kind.to_string())),
                ("position", Value::Int(position)),
                ("type", Value::Text(c.col_type.cql_string())),
            ],
        )?;
    }
    Ok(())
}

fn unmirror_table_metadata(
    storage: &Storage,
    keyspace: &str,
    table: &str,
) -> Result<(), anyhow::Error> {
    storage.delete_row(
        "system_schema",
        "tables",
        &[keyspace.as_bytes().to_vec(), table.as_bytes().to_vec()],
    )?;
    delete_system_schema_partition(
        storage,
        "columns",
        &[keyspace.as_bytes().to_vec(), table.as_bytes().to_vec()],
    )
}

fn exec_drop_keyspace(
    storage: &Storage,
    d: &cql3_parser::common_drop::CommonDrop,
) -> Result<Response, QueryError> {
    let name = norm_id(&d.name.name);
    if storage.get_keyspace(&name).map_err(internal_err)?.is_none() {
        if d.if_exists {
            return Ok(Response::Void);
        }
        return Err(invalid_err(format!(
            "Cannot drop non existing keyspace '{name}'"
        )));
    }
    check_writable(&name)?;
    storage.delete_keyspace(&name).map_err(internal_err)?;
    for table in ["keyspaces", "tables", "columns"] {
        delete_system_schema_partition(storage, table, &[name.as_bytes().to_vec()])
            .map_err(internal_err)?;
    }
    Ok(Response::SchemaChange {
        change: "DROPPED",
        target: "KEYSPACE",
        keyspace: name,
        table: String::new(),
    })
}

fn exec_drop_table(
    storage: &Storage,
    session_ks: &Option<String>,
    d: &cql3_parser::common_drop::CommonDrop,
) -> Result<Response, QueryError> {
    let keyspace = match &d.name.keyspace {
        Some(k) => norm_id(k),
        None => session_ks
            .clone()
            .ok_or_else(|| invalid_err("No keyspace specified; use USE <keyspace>"))?,
    };
    let table = norm_id(&d.name.name);
    if storage
        .get_table(&keyspace, &table)
        .map_err(internal_err)?
        .is_none()
    {
        if d.if_exists {
            return Ok(Response::Void);
        }
        return Err(invalid_err(format!(
            "Cannot drop non existing table '{keyspace}.{table}'"
        )));
    }
    check_writable(&keyspace)?;
    storage
        .delete_table(&keyspace, &table)
        .map_err(internal_err)?;
    unmirror_table_metadata(storage, &keyspace, &table).map_err(internal_err)?;
    Ok(Response::SchemaChange {
        change: "DROPPED",
        target: "TABLE",
        keyspace,
        table,
    })
}

fn exec_truncate(
    storage: &Storage,
    session_ks: &Option<String>,
    name: &FQName,
) -> Result<Response, QueryError> {
    let (keyspace, _) = resolve_table(storage, session_ks, name)?;
    check_writable(&keyspace)?;
    let table = norm_id(&name.name);
    storage
        .truncate_table(&keyspace, &table)
        .map_err(internal_err)?;
    Ok(Response::Void)
}

fn exec_insert(
    storage: &Storage,
    session_ks: &Option<String>,
    ins: &cql3_parser::insert::Insert,
    binds: &mut Binds,
) -> Result<Response, QueryError> {
    let (keyspace, table) = resolve_table(storage, session_ks, &ins.table_name)?;
    check_writable(&keyspace)?;
    let operands = match &ins.values {
        cql3_parser::insert::InsertValues::Values(v) => v,
        cql3_parser::insert::InsertValues::Json(_) => {
            return Err(invalid_err("INSERT JSON is not supported"));
        }
    };
    if ins.columns.len() != operands.len() {
        return Err(syntax_err("Wrong number of values provided"));
    }

    let mut values: HashMap<String, Value> = HashMap::new();
    for (col_id, operand) in ins.columns.iter().zip(operands.iter()) {
        let name = norm_id(col_id);
        let col = table
            .column(&name)
            .ok_or_else(|| invalid_err(format!("Undefined column name {name}")))?;
        let v = operand_to_value(operand, &col.col_type, binds)?;
        values.insert(name, v);
    }

    let mut key_parts = Vec::new();
    for pk in &table.partition_keys {
        let v = values
            .get(pk)
            .ok_or_else(|| invalid_err(format!("Some partition key parts are missing: {pk}")))?;
        if *v == Value::Null {
            return Err(invalid_err(format!(
                "Invalid null value for partition key part {pk}"
            )));
        }
        key_parts.push(v.raw_bytes());
    }
    for ck in &table.clustering_keys {
        let v = values
            .get(ck)
            .ok_or_else(|| invalid_err(format!("Some clustering key parts are missing: {ck}")))?;
        if *v == Value::Null {
            return Err(invalid_err(format!(
                "Invalid null value for clustering key part {ck}"
            )));
        }
        key_parts.push(v.raw_bytes());
    }

    let mut row_columns = Vec::new();
    for col in &table.columns {
        if col.is_partition_key || col.is_clustering {
            continue;
        }
        let idx = table
            .column_index(&col.name)
            .ok_or_else(|| internal_err(anyhow::anyhow!("column index missing")))?;
        let v = values.get(&col.name).cloned().unwrap_or(Value::Null);
        row_columns.push((idx as u16, v));
    }

    storage
        .put_row(
            &keyspace,
            &table.name,
            &key_parts,
            &encode_row_columns(&row_columns),
        )
        .map_err(internal_err)?;
    Ok(Response::Void)
}

fn operand_to_value(
    operand: &Operand,
    col_type: &ColumnType,
    binds: &mut Binds,
) -> Result<Value, QueryError> {
    match operand {
        Operand::Null => Ok(Value::Null),
        Operand::Const(text) => {
            Value::parse_literal(text, col_type).map_err(|e| invalid_err(e.to_string()))
        }
        Operand::Param(_) => binds.take(col_type),
        Operand::Set(items) => {
            let ColumnType::Set(elem_type) = col_type else {
                return Err(invalid_err("Collection type mismatch in values"));
            };
            let mut v = Vec::with_capacity(items.len());
            for it in items {
                v.push(operand_to_value(
                    &Operand::Const(it.clone()),
                    elem_type,
                    binds,
                )?);
            }
            Ok(Value::Set(v))
        }
        Operand::List(items) => {
            let ColumnType::List(elem_type) = col_type else {
                return Err(invalid_err("Collection type mismatch in values"));
            };
            let mut v = Vec::with_capacity(items.len());
            for it in items {
                v.push(operand_to_value(
                    &Operand::Const(it.clone()),
                    elem_type,
                    binds,
                )?);
            }
            Ok(Value::List(v))
        }
        Operand::Map(entries) => {
            let ColumnType::Map(k_type, v_type) = col_type else {
                return Err(invalid_err("Collection type mismatch in values"));
            };
            let mut v = Vec::with_capacity(entries.len());
            for (k, val) in entries {
                v.push((
                    operand_to_value(&Operand::Const(k.clone()), k_type, binds)?,
                    operand_to_value(&Operand::Const(val.clone()), v_type, binds)?,
                ));
            }
            Ok(Value::Map(v))
        }
        _ => Err(invalid_err(
            "Only literal values are supported (no bind markers)",
        )),
    }
}

enum Filter {
    Eq(String, Value),
    In(String, Vec<Value>),
}

fn parse_filters(
    table: &TableDef,
    where_clause: &[cql3_parser::common::RelationElement],
    binds: &mut Binds,
) -> Result<Vec<Filter>, QueryError> {
    let mut filters = Vec::new();
    for rel in where_clause {
        let Operand::Column(col) = &rel.obj else {
            return Err(invalid_err(
                "Only column restrictions are supported in WHERE",
            ));
        };
        let name = norm_id(col);
        if table.column(&name).is_none() {
            return Err(invalid_err(format!("Undefined column name {name}")));
        }
        match &rel.oper {
            RelationOperator::Equal => match &rel.value {
                Operand::Null => filters.push(Filter::Eq(name, Value::Null)),
                Operand::Const(text) => {
                    let col_type = table.column(&name).unwrap().col_type.clone();
                    let v = Value::parse_literal(text, &col_type)
                        .map_err(|e| invalid_err(e.to_string()))?;
                    filters.push(Filter::Eq(name, v));
                }
                Operand::Param(_) => {
                    let col_type = table.column(&name).unwrap().col_type.clone();
                    filters.push(Filter::Eq(name, binds.take(&col_type)?));
                }
                _ => return Err(invalid_err("Only literal values are supported in WHERE")),
            },
            RelationOperator::In => {
                let col_type = table.column(&name).unwrap().col_type.clone();
                let mut values = Vec::new();
                match &rel.value {
                    Operand::Tuple(items) => {
                        for item in items {
                            match item {
                                Operand::Const(text) => values.push(
                                    Value::parse_literal(text, &col_type)
                                        .map_err(|e| invalid_err(e.to_string()))?,
                                ),
                                Operand::Param(_) => values.push(binds.take(&col_type)?),
                                _ => {
                                    return Err(invalid_err(
                                        "Only literal values are supported in WHERE",
                                    ));
                                }
                            }
                        }
                    }
                    Operand::Const(text) => values.push(
                        Value::parse_literal(text, &col_type)
                            .map_err(|e| invalid_err(e.to_string()))?,
                    ),
                    Operand::Param(_) => values.push(binds.take(&col_type)?),
                    _ => return Err(invalid_err("Only literal values are supported in WHERE")),
                }
                filters.push(Filter::In(name, values));
            }
            _ => {
                return Err(invalid_err(
                    "Only '=' and 'IN' restrictions are supported in WHERE",
                ));
            }
        }
    }
    Ok(filters)
}

fn exec_select(
    storage: &Storage,
    session_ks: &Option<String>,
    sel: &cql3_parser::select::Select,
    binds: &mut Binds,
) -> Result<Response, QueryError> {
    let (keyspace, table) = resolve_table(storage, session_ks, &sel.table_name)?;

    let selected = select_columns(&table, &sel.columns)?;

    let filters = parse_filters(&table, &sel.where_clause, binds)?;

    let mut fetched: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut pk_restricted = true;
    for pk in &table.partition_keys {
        let restricted = filters.iter().any(|f| match f {
            Filter::Eq(name, _) | Filter::In(name, _) => name == pk,
        });
        if !restricted {
            pk_restricted = false;
            break;
        }
    }

    if pk_restricted {
        let mut partitions: Vec<Vec<Vec<u8>>> = vec![Vec::new()];
        for pk in &table.partition_keys {
            let values: Vec<Vec<u8>> = filters
                .iter()
                .filter_map(|f| match f {
                    Filter::Eq(name, v) if name == pk => Some(vec![v.raw_bytes()]),
                    Filter::In(name, vs) if name == pk => {
                        Some(vs.iter().map(|v| v.raw_bytes()).collect())
                    }
                    _ => None,
                })
                .flatten()
                .collect();
            let mut next = Vec::new();
            for combo in &partitions {
                for v in &values {
                    let mut c = combo.clone();
                    c.push(v.clone());
                    next.push(c);
                }
            }
            partitions = next;
        }
        for partition in partitions {
            fetched.extend(
                storage
                    .scan_partition(&keyspace, &table.name, &partition)
                    .map_err(internal_err)?,
            );
        }
    } else {
        fetched.extend(
            storage
                .scan_rows(&keyspace, &table.name)
                .map_err(internal_err)?,
        );
    }

    let mut out_rows: Vec<Vec<Value>> = Vec::new();
    for (key, value_bytes) in fetched {
        let key_parts = decode_key(&keyspace, &table.name, &key).map_err(internal_err)?;
        let mut row_columns = decode_row_columns(&value_bytes, &table).map_err(internal_err)?;

        let mut part_idx = 0;
        for col in &table.columns {
            if col.is_partition_key || col.is_clustering {
                if part_idx < key_parts.len() {
                    row_columns[table.column_index(&col.name).unwrap()] =
                        Value::from_raw_bytes(&key_parts[part_idx], &col.col_type)
                            .map_err(internal_err)?;
                }
                part_idx += 1;
            }
        }

        let mut passes = true;
        for f in &filters {
            let (name, expected) = match f {
                Filter::Eq(name, v) => (name, v),
                Filter::In(name, vs) => {
                    let idx = table.column_index(name).unwrap();
                    match vs.iter().find(|v| **v == row_columns[idx]) {
                        Some(v) => (name, v),
                        None => {
                            passes = false;
                            break;
                        }
                    }
                }
            };
            let idx = table.column_index(name).unwrap();
            if *expected != row_columns[idx] {
                passes = false;
                break;
            }
        }
        if !passes {
            continue;
        }

        let mut out = Vec::new();
        for (name, _) in &selected {
            let idx = table.column_index(name).unwrap();
            out.push(row_columns[idx].clone());
        }
        out_rows.push(out);

        if let Some(limit) = sel.limit
            && out_rows.len() as i32 >= limit
        {
            break;
        }
    }

    Ok(Response::Rows {
        keyspace,
        table: table.name,
        cols: selected,
        rows: out_rows,
    })
}

fn key_parts_from_filters(
    table: &TableDef,
    filters: &[Filter],
) -> Result<Vec<Vec<u8>>, QueryError> {
    let mut parts = Vec::new();
    for name in table
        .partition_keys
        .iter()
        .chain(table.clustering_keys.iter())
    {
        let v = filters
            .iter()
            .find_map(|f| match f {
                Filter::Eq(n, v) if n == name => Some(v),
                _ => None,
            })
            .ok_or_else(|| {
                invalid_err(format!("Restrictions are incomplete for key part {name}"))
            })?;
        if *v == Value::Null {
            return Err(invalid_err(format!(
                "Invalid null value for key part {name}"
            )));
        }
        parts.push(v.raw_bytes());
    }
    Ok(parts)
}

fn exec_delete(
    storage: &Storage,
    session_ks: &Option<String>,
    del: &cql3_parser::delete::Delete,
    binds: &mut Binds,
) -> Result<Response, QueryError> {
    let (keyspace, table) = resolve_table(storage, session_ks, &del.table_name)?;
    check_writable(&keyspace)?;
    if !del.columns.is_empty() {
        return Err(invalid_err("Column-level DELETE is not supported yet"));
    }
    let filters = parse_filters(&table, &del.where_clause, binds)?;
    let key_parts = key_parts_from_filters(&table, &filters)?;
    storage
        .delete_row(&keyspace, &table.name, &key_parts)
        .map_err(internal_err)?;
    Ok(Response::Void)
}

fn exec_update(
    storage: &Storage,
    session_ks: &Option<String>,
    upd: &cql3_parser::update::Update,
    binds: &mut Binds,
) -> Result<Response, QueryError> {
    let (keyspace, table) = resolve_table(storage, session_ks, &upd.table_name)?;
    check_writable(&keyspace)?;

    let mut new_values: Vec<(usize, Value)> = Vec::with_capacity(upd.assignments.len());
    for assignment in &upd.assignments {
        if assignment.operator.is_some() {
            return Err(invalid_err(
                "Increment/decrement assignments are not supported",
            ));
        }
        let name = norm_id(&assignment.name.column);
        let col = table
            .column(&name)
            .ok_or_else(|| invalid_err(format!("Undefined column name {name}")))?;
        let v = operand_to_value(&assignment.value, &col.col_type, binds)?;
        new_values.push((table.column_index(&name).unwrap(), v));
    }

    let filters = parse_filters(&table, &upd.where_clause, binds)?;
    let key_parts = key_parts_from_filters(&table, &filters)?;

    let existing = storage
        .get_row(&keyspace, &table.name, &key_parts)
        .map_err(internal_err)?;
    let mut row_columns: Vec<Value> = match existing {
        Some(bytes) => decode_row_columns(&bytes, &table).map_err(internal_err)?,
        None => vec![Value::Null; table.columns.len()],
    };
    for (idx, v) in new_values {
        row_columns[idx] = v;
    }

    let mut row_encoded = Vec::new();
    for (idx, col) in table.columns.iter().enumerate() {
        if col.is_partition_key || col.is_clustering {
            continue;
        }
        row_encoded.push((idx as u16, row_columns[idx].clone()));
    }
    storage
        .put_row(
            &keyspace,
            &table.name,
            &key_parts,
            &encode_row_columns(&row_encoded),
        )
        .map_err(internal_err)?;
    Ok(Response::Void)
}
