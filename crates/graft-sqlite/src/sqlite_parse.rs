//! `SQLite` File Format Parser - Complete Implementation
//!
//! Supports:
//! - Reading database header
//! - Parsing `sqlite_master` table
//! - Traversing B-tree to read table data
//! - Serializing/deserializing records

use std::collections::HashMap;

use graft::core::PageIdx;
use graft::volume_reader::{VolumeRead, VolumeReader};

/// `SQLite` database header
#[derive(Debug, Clone)]
pub struct DatabaseHeader {
    pub page_size: u32,
    pub reserved_space: u8,
    pub text_encoding: TextEncoding,
    pub num_pages: u32,
    pub schema_cookie: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TextEncoding {
    Utf8 = 1,
    Utf16le = 2,
    Utf16be = 3,
}

impl TextEncoding {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(TextEncoding::Utf8),
            2 => Some(TextEncoding::Utf16le),
            3 => Some(TextEncoding::Utf16be),
            _ => None,
        }
    }
}

/// B-tree page header
#[derive(Debug, Clone)]
pub struct BtreePageHeader {
    pub page_type: u8,
    pub first_freeblock: u16,
    pub num_cells: u16,
    pub cell_content_offset: u16,
    pub fragmented_free_bytes: u8,
    pub right_child_ptr: Option<u32>,
}

impl BtreePageHeader {
    pub fn is_leaf(&self) -> bool {
        self.page_type == 13 || self.page_type == 10
    }

    pub fn is_table(&self) -> bool {
        self.page_type == 5 || self.page_type == 13
    }

    pub fn header_size(&self) -> usize {
        if self.page_type == 2 || self.page_type == 5 {
            12 // Interior pages have right child pointer
        } else {
            8
        }
    }
}

/// Table leaf cell (contains rowid + payload)
#[derive(Debug, Clone)]
pub struct TableLeafCell {
    pub rowid: i64,
    pub payload: Vec<u8>,
}

/// Table interior cell (points to child page)
#[derive(Debug, Clone)]
pub struct TableInteriorCell {
    pub left_child: u32,
    pub rowid: i64,
}

/// Serialized record
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub values: Vec<Value>,
}

/// SQL value types
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    /// Convert to SQL string representation
    pub fn to_sql(&self) -> String {
        match self {
            Value::Null => "NULL".to_string(),
            Value::Integer(i) => i.to_string(),
            Value::Real(r) => format!("{r:.15}"),
            Value::Text(s) => {
                let escaped = s.replace('\'', "''");
                format!("'{escaped}'")
            }
            Value::Blob(b) => {
                let hex: String = b.iter().map(|b| format!("{b:02X}")).collect();
                format!("X'{hex}'")
            }
        }
    }
}

/// Table schema information
#[derive(Debug, Clone)]
pub struct TableSchema {
    pub name: String,
    pub sql: String,
    pub root_page: u32,
    pub columns: Vec<ColumnInfo>,
}

#[derive(Debug, Clone)]
pub struct ColumnInfo {
    pub name: String,
    pub ctype: String,
    pub not_null: bool,
    pub default_value: Option<Value>,
    pub pk: bool,
}

/// Table scanner for reading B-tree pages
pub struct TableScanner<'a> {
    reader: &'a VolumeReader,
    header: DatabaseHeader,
}

impl<'a> TableScanner<'a> {
    pub fn new(reader: &'a VolumeReader) -> Result<Self, ParseError> {
        let header_page = reader
            .read_page(PageIdx::FIRST)
            .map_err(|_| ParseError::ReadError)?;
        let header = Self::parse_header(header_page.as_ref())?;

        Ok(Self { reader, header })
    }

    pub fn get_header(&self) -> &DatabaseHeader {
        &self.header
    }

    /// Parse database header
    fn parse_header(page: &[u8]) -> Result<DatabaseHeader, ParseError> {
        if page.len() < 100 {
            tracing::error!("Page too small: {} bytes", page.len());
            return Err(ParseError::InvalidHeader);
        }

        // Validate magic number
        let magic = &page[0..16];
        if magic != b"SQLite format 3\x00" {
            tracing::error!("Invalid magic: {:?}", magic);
            return Err(ParseError::InvalidMagic);
        }

        // Page size
        let page_size_raw = u16::from_be_bytes([page[16], page[17]]);
        let page_size = if page_size_raw == 1 {
            65536
        } else {
            page_size_raw as u32
        };

        // Reserved bytes at the end of each page.
        let reserved_space = page[20];

        // Text encoding - default to UTF-8 if invalid (0 means default/unspecified)
        let text_encoding = TextEncoding::from_u8(page[56]).unwrap_or(TextEncoding::Utf8);

        // Number of pages
        let num_pages = u32::from_be_bytes([page[28], page[29], page[30], page[31]]);

        // Schema cookie
        let schema_cookie = u32::from_be_bytes([page[40], page[41], page[42], page[43]]);

        tracing::debug!(
            "Parsed SQLite header: page_size={}, pages={}, encoding={:?}",
            page_size,
            num_pages,
            text_encoding
        );

        Ok(DatabaseHeader {
            page_size,
            reserved_space,
            text_encoding,
            num_pages,
            schema_cookie,
        })
    }

    /// Read B-tree page header
    fn read_btree_header(page: &[u8]) -> Result<BtreePageHeader, ParseError> {
        if page.len() < 8 {
            return Err(ParseError::InvalidPage);
        }

        let page_type = page[0];
        let first_freeblock = u16::from_be_bytes([page[1], page[2]]);
        let num_cells = u16::from_be_bytes([page[3], page[4]]);
        let cell_content_offset = u16::from_be_bytes([page[5], page[6]]);
        let fragmented_free_bytes = page[7];

        let right_child_ptr = if page_type == 2 || page_type == 5 {
            if page.len() < 12 {
                return Err(ParseError::InvalidPage);
            }
            Some(u32::from_be_bytes([page[8], page[9], page[10], page[11]]))
        } else {
            None
        };

        Ok(BtreePageHeader {
            page_type,
            first_freeblock,
            num_cells,
            cell_content_offset,
            fragmented_free_bytes,
            right_child_ptr,
        })
    }

    /// Scan all rows of a table
    pub fn scan_table(&self, root_page: u32) -> Result<Vec<TableLeafCell>, ParseError> {
        let mut rows = Vec::new();
        self.scan_table_page(root_page, &mut rows)?;
        Ok(rows)
    }

    /// Recursively scan B-tree pages
    fn scan_table_page(
        &self,
        page_num: u32,
        rows: &mut Vec<TableLeafCell>,
    ) -> Result<(), ParseError> {
        if page_num == 0 || page_num > self.header.num_pages {
            return Err(ParseError::InvalidPageNumber);
        }

        let page_idx = PageIdx::try_new(page_num).ok_or(ParseError::InvalidPageNumber)?;

        let full_page = self
            .reader
            .read_page(page_idx)
            .map_err(|_| ParseError::ReadError)?;
        let full_page = full_page.as_ref();

        // Page 1 has database header (100 bytes) before B-tree header
        let header_offset = if page_num == 1 { 100 } else { 0 };
        if full_page.len() < header_offset + 8 {
            return Err(ParseError::InvalidPage);
        }

        let header = Self::read_btree_header(&full_page[header_offset..])?;

        tracing::debug!(
            "Scanning page {}: type={}, cells={}, offset={}",
            page_num,
            header.page_type,
            header.num_cells,
            header_offset
        );

        match header.page_type {
            13 => {
                // Leaf table page
                self.read_leaf_cells(full_page, header_offset, &header, rows)?;
            }
            5 => {
                // Interior table page
                self.read_interior_cells(full_page, header_offset, &header, rows)?;
            }
            _ => {
                tracing::warn!(
                    "Skipping unknown page type {} on page {}",
                    header.page_type,
                    page_num
                );
            }
        }

        Ok(())
    }

    /// Read leaf page cells
    fn read_leaf_cells(
        &self,
        full_page: &[u8],
        header_offset: usize,
        header: &BtreePageHeader,
        rows: &mut Vec<TableLeafCell>,
    ) -> Result<(), ParseError> {
        let header_size = header.header_size();

        for i in 0..header.num_cells {
            let ptr_offset = header_offset + header_size + (i as usize * 2);
            if ptr_offset + 2 > full_page.len() {
                break;
            }

            // Cell pointer is relative to page start
            let cell_offset =
                u16::from_be_bytes([full_page[ptr_offset], full_page[ptr_offset + 1]]) as usize;

            tracing::debug!(
                "Leaf cell {}: ptr_offset={}, cell_offset={}",
                i,
                ptr_offset,
                cell_offset
            );

            let usable_end = self.usable_size().min(full_page.len());
            if cell_offset >= usable_end {
                return Err(ParseError::InvalidCell);
            }

            if let Some(cell) = self.parse_table_leaf_cell(&full_page[cell_offset..usable_end])? {
                rows.push(cell);
            }
        }

        Ok(())
    }

    /// Read interior page cells and recurse
    fn read_interior_cells(
        &self,
        full_page: &[u8],
        header_offset: usize,
        header: &BtreePageHeader,
        rows: &mut Vec<TableLeafCell>,
    ) -> Result<(), ParseError> {
        let header_size = 12; // Interior table header is 12 bytes

        // First read rightmost child page
        if let Some(right_child) = header.right_child_ptr {
            self.scan_table_page(right_child, rows)?;
        }

        // Then read each cell's child page
        for i in 0..header.num_cells {
            let ptr_offset = header_offset + header_size + (i as usize * 2);
            if ptr_offset + 2 > full_page.len() {
                break;
            }

            // Cell pointer is relative to page start
            let cell_offset =
                u16::from_be_bytes([full_page[ptr_offset], full_page[ptr_offset + 1]]) as usize;

            tracing::debug!(
                "Interior cell {}: ptr_offset={}, cell_offset={}",
                i,
                ptr_offset,
                cell_offset
            );

            if let Some(cell) = Self::parse_table_interior_cell(&full_page[cell_offset..])? {
                self.scan_table_page(cell.left_child, rows)?;
            }
        }

        Ok(())
    }

    /// Parse table leaf cell
    fn parse_table_leaf_cell(&self, data: &[u8]) -> Result<Option<TableLeafCell>, ParseError> {
        if data.is_empty() {
            return Ok(None);
        }

        let (payload_size, offset1) = read_varint(data);
        if payload_size < 0 || offset1 == 0 || offset1 >= data.len() {
            return Err(ParseError::InvalidCell);
        }
        let (rowid, offset2) = read_varint(&data[offset1..]);
        if offset2 == 0 {
            return Err(ParseError::InvalidCell);
        }

        let payload_start = offset1 + offset2;
        let payload_size = usize::try_from(payload_size).map_err(|_| ParseError::InvalidCell)?;
        let local_payload_size = self.local_table_leaf_payload_size(payload_size);
        let local_payload_end = payload_start
            .checked_add(local_payload_size)
            .ok_or(ParseError::InvalidCell)?;

        if local_payload_end > data.len() {
            return Err(ParseError::InvalidCell);
        }

        let mut payload = Vec::with_capacity(payload_size);
        payload.extend_from_slice(&data[payload_start..local_payload_end]);

        if payload.len() < payload_size {
            let overflow_page_ptr_end = local_payload_end
                .checked_add(4)
                .ok_or(ParseError::InvalidCell)?;
            if overflow_page_ptr_end > data.len() {
                return Err(ParseError::InvalidCell);
            }

            let first_overflow_page = u32::from_be_bytes([
                data[local_payload_end],
                data[local_payload_end + 1],
                data[local_payload_end + 2],
                data[local_payload_end + 3],
            ]);
            self.read_overflow_payload(
                first_overflow_page,
                payload_size - payload.len(),
                &mut payload,
            )?;
        }

        Ok(Some(TableLeafCell { rowid, payload }))
    }

    fn local_table_leaf_payload_size(&self, payload_size: usize) -> usize {
        let usable_size = self.usable_size();
        let max_local = usable_size.saturating_sub(35);
        if payload_size <= max_local {
            return payload_size;
        }

        let min_local = ((usable_size.saturating_sub(12) * 32) / 255).saturating_sub(23);
        let overflow_payload_size = usable_size.saturating_sub(4);
        if overflow_payload_size == 0 {
            return 0;
        }

        let mut local_payload_size =
            min_local + ((payload_size - min_local) % overflow_payload_size);
        if local_payload_size > max_local {
            local_payload_size = min_local;
        }
        local_payload_size
    }

    fn read_overflow_payload(
        &self,
        first_page_num: u32,
        mut remaining: usize,
        payload: &mut Vec<u8>,
    ) -> Result<(), ParseError> {
        let usable_size = self.usable_size();
        if usable_size <= 4 {
            return Err(ParseError::InvalidPage);
        }

        let mut page_num = first_page_num;
        while remaining > 0 {
            if page_num == 0 || page_num > self.header.num_pages {
                return Err(ParseError::InvalidPageNumber);
            }

            let page_idx = PageIdx::try_new(page_num).ok_or(ParseError::InvalidPageNumber)?;
            let full_page = self
                .reader
                .read_page(page_idx)
                .map_err(|_| ParseError::ReadError)?;
            let full_page = full_page.as_ref();
            let usable_end = usable_size.min(full_page.len());
            if usable_end <= 4 {
                return Err(ParseError::InvalidPage);
            }

            let next_page =
                u32::from_be_bytes([full_page[0], full_page[1], full_page[2], full_page[3]]);
            let available = usable_end - 4;
            let read_size = remaining.min(available);
            payload.extend_from_slice(&full_page[4..4 + read_size]);
            remaining -= read_size;

            if remaining > 0 && next_page == 0 {
                return Err(ParseError::InvalidPageNumber);
            }
            page_num = next_page;
        }

        Ok(())
    }

    fn usable_size(&self) -> usize {
        self.header
            .page_size
            .saturating_sub(u32::from(self.header.reserved_space)) as usize
    }

    /// Parse table interior cell
    fn parse_table_interior_cell(data: &[u8]) -> Result<Option<TableInteriorCell>, ParseError> {
        if data.len() < 4 {
            return Ok(None);
        }

        let left_child = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let (rowid, _) = read_varint(&data[4..]);

        Ok(Some(TableInteriorCell { left_child, rowid }))
    }

    /// Read `sqlite_master` table
    pub fn read_master_table(&self) -> Result<Vec<MasterEntry>, ParseError> {
        // sqlite_master is always on page 1
        let rows = self.scan_table(1)?;

        let mut entries = Vec::new();

        for row in rows {
            let record = Record::parse(&row.payload)?;
            if record.values.len() >= 5 {
                fn value_to_string(v: &Value) -> String {
                    match v {
                        Value::Text(s) => s.clone(),
                        Value::Blob(b) => String::from_utf8_lossy(b).to_string(),
                        Value::Integer(i) => i.to_string(),
                        Value::Real(r) => r.to_string(),
                        Value::Null => String::new(),
                    }
                }

                let entry = MasterEntry {
                    entry_type: value_to_string(&record.values[0]),
                    name: value_to_string(&record.values[1]),
                    table_name: value_to_string(&record.values[2]),
                    root_page: match &record.values[3] {
                        Value::Integer(i) => *i as u32,
                        _ => 0,
                    },
                    sql: value_to_string(&record.values[4]),
                };
                entries.push(entry);
            }
        }

        Ok(entries)
    }
}

/// `sqlite_master` entry
#[derive(Debug, Clone)]
pub struct MasterEntry {
    pub entry_type: String, // "table", "index", "view", "trigger"
    pub name: String,
    pub table_name: String,
    pub root_page: u32,
    pub sql: String,
}

impl MasterEntry {
    /// Parse column names from CREATE TABLE SQL.
    /// Handles CREATE TABLE name(col1 TYPE, col2 TYPE, ...) syntax.
    pub fn parse_columns(&self) -> Vec<ColumnInfo> {
        parse_create_table_columns(&self.sql)
    }
}

impl Record {
    /// Parse record (payload)
    pub fn parse(data: &[u8]) -> Result<Self, ParseError> {
        let (header_size, header_offset) = read_varint(data);
        // header_size includes the size varint itself, so header_end = header_size
        let header_end = header_size as usize;

        if header_end > data.len() {
            tracing::error!(
                "Invalid record: header_end={} > data_len={}",
                header_end,
                data.len()
            );
            return Err(ParseError::InvalidRecord);
        }

        // Parse type codes (start after the size varint)
        let mut types = Vec::new();
        let mut pos = header_offset;
        while pos < header_end {
            let (type_code, bytes_read) = read_varint(&data[pos..]);
            types.push(type_code);
            pos += bytes_read;
        }

        // Parse values (start at header_end)
        let mut values = Vec::new();
        pos = header_end;

        tracing::debug!(
            "Record::parse: header_size={}, types={:?}, data_len={}, pos after header={}",
            header_size,
            types,
            data.len(),
            pos
        );

        for type_code in types {
            let (value, bytes_read) = parse_value(data, pos, type_code)?;
            values.push(value);
            pos += bytes_read;
        }

        Ok(Record { values })
    }
}

/// Read variable-length integer (unsigned, as used in `SQLite` record headers).
/// Returns (value, `number_of_bytes_read`).
fn read_varint(data: &[u8]) -> (i64, usize) {
    let mut result: i64 = 0;
    let mut i = 0;

    while i < 9 && i < data.len() {
        let byte = data[i] as i64;
        result = (result << 7) | (byte & 0x7f);
        i += 1;

        if byte & 0x80 == 0 {
            break;
        }
    }

    (result, i)
}

/// Parse value based on type code
fn parse_value(data: &[u8], pos: usize, type_code: i64) -> Result<(Value, usize), ParseError> {
    tracing::trace!(
        "parse_value: pos={}, type_code={}, data_len={}",
        pos,
        type_code,
        data.len()
    );
    match type_code {
        0 => Ok((Value::Null, 0)),

        1..=6 => {
            // Integer (1-6 bytes, big-endian, signed)
            let len = type_code as usize;
            if pos + len > data.len() {
                tracing::error!(
                    "Integer overflow: pos={}, len={}, data_len={}",
                    pos,
                    len,
                    data.len()
                );
                return Err(ParseError::InvalidValue);
            }

            let mut val: i64 = 0;
            for i in 0..len {
                val = (val << 8) | (data[pos + i] as i64);
            }

            // Sign extension
            if len < 8 && (data[pos] & 0x80) != 0 {
                val -= 1 << (len * 8);
            }

            Ok((Value::Integer(val), len))
        }

        7 => {
            // Float (8 bytes, IEEE 754 big-endian)
            if pos + 8 > data.len() {
                tracing::error!(
                    "Float overflow: pos={}, len=8, data_len={}",
                    pos,
                    data.len()
                );
                return Err(ParseError::InvalidValue);
            }
            let bytes: [u8; 8] = data[pos..pos + 8].try_into().unwrap();
            let val = f64::from_be_bytes(bytes);
            Ok((Value::Real(val), 8))
        }

        8 => Ok((Value::Integer(0), 0)),
        9 => Ok((Value::Integer(1), 0)),

        n if n >= 12 && n % 2 == 0 => {
            // Blob: (n-12)/2 bytes
            let len = ((n - 12) / 2) as usize;
            if pos + len > data.len() {
                tracing::error!(
                    "Blob overflow: pos={}, len={}, data_len={}",
                    pos,
                    len,
                    data.len()
                );
                return Err(ParseError::InvalidValue);
            }
            let blob = data[pos..pos + len].to_vec();
            Ok((Value::Blob(blob), len))
        }

        n if n >= 13 && n % 2 == 1 => {
            // Text: (n-13)/2 bytes
            let len = ((n - 13) / 2) as usize;
            if pos + len > data.len() {
                tracing::error!(
                    "Text overflow: pos={}, len={}, data_len={}",
                    pos,
                    len,
                    data.len()
                );
                return Err(ParseError::InvalidValue);
            }
            // Assume UTF-8 (actual encoding depends on database encoding)
            let text = String::from_utf8_lossy(&data[pos..pos + len]).to_string();
            Ok((Value::Text(text), len))
        }

        _ => Err(ParseError::InvalidTypeCode),
    }
}

/// Parse all rows of a table, returns rowid -> Record mapping
pub fn read_all_rows(
    reader: &VolumeReader,
    root_page: u32,
) -> Result<HashMap<i64, Record>, ParseError> {
    let scanner = TableScanner::new(reader)?;
    let cells = scanner.scan_table(root_page)?;

    let mut rows = HashMap::new();
    for cell in cells {
        let record = Record::parse(&cell.payload)?;
        rows.insert(cell.rowid, record);
    }

    Ok(rows)
}

/// Parse column definitions from a CREATE TABLE SQL statement.
/// Returns column names and their types (as strings).
pub fn parse_create_table_columns(sql: &str) -> Vec<ColumnInfo> {
    // Find the opening parenthesis after CREATE TABLE name
    let _sql_upper = sql.to_uppercase();
    let Some(open_paren) = sql.find('(') else {
        return vec![];
    };
    let Some(close_paren) = sql.rfind(')') else {
        return vec![];
    };

    let mut columns = Vec::new();
    let body = &sql[open_paren + 1..close_paren];

    // Split by commas, respecting nested parentheses
    let mut depth = 0;
    let mut current = String::new();
    for c in body.chars() {
        match c {
            '(' => {
                depth += 1;
                current.push(c);
            }
            ')' => {
                depth -= 1;
                current.push(c);
            }
            ',' if depth == 0 => {
                if let Some(col) = parse_one_column(current.trim()) {
                    columns.push(col);
                }
                current = String::new();
            }
            _ => current.push(c),
        }
    }
    if let Some(col) = parse_one_column(current.trim()) {
        columns.push(col);
    }

    columns
}

/// Parse a single column definition like "name TEXT" or "name TEXT NOT NULL"
fn parse_one_column(def: &str) -> Option<ColumnInfo> {
    let def = def.trim();
    if def.is_empty() {
        return None;
    }

    // Skip table-level constraints (PRIMARY KEY, UNIQUE, CHECK, FOREIGN KEY)
    let upper = def.to_uppercase();
    if upper.starts_with("PRIMARY")
        || upper.starts_with("UNIQUE")
        || upper.starts_with("CHECK")
        || upper.starts_with("FOREIGN")
        || upper.starts_with("CONSTRAINT")
    {
        return None;
    }

    // Extract the column name (first token, handling quotes and brackets)
    let (name, rest) = extract_identifier(def);
    let rest_upper = rest.to_uppercase();

    // Determine type from the next word
    let ctype = rest.split_whitespace().next().unwrap_or("").to_string();

    let not_null = rest_upper.contains("NOT NULL");
    let pk = rest_upper.contains("PRIMARY KEY");

    Some(ColumnInfo {
        name: name.to_string(),
        ctype,
        not_null,
        default_value: None,
        pk,
    })
}

/// Extract the first identifier (quoted or unquoted) from a definition string.
/// Returns (identifier, `rest_of_string`).
fn extract_identifier(s: &str) -> (&str, &str) {
    let s = s.trim();
    if s.is_empty() {
        return (s, "");
    }

    let first_char = s.chars().next().unwrap();
    match first_char {
        '"' => {
            // Double-quoted identifier: "col name"
            if let Some(end) = s[1..].find('"') {
                (&s[1..end + 1], s[end + 2..].trim())
            } else {
                (&s[1..], "")
            }
        }
        '[' => {
            // Bracket-quoted identifier: [col name]
            if let Some(end) = s[1..].find(']') {
                (&s[1..end + 1], s[end + 2..].trim())
            } else {
                (&s[1..], "")
            }
        }
        '`' => {
            // Backtick-quoted identifier: `col name`
            if let Some(end) = s[1..].find('`') {
                (&s[1..end + 1], s[end + 2..].trim())
            } else {
                (&s[1..], "")
            }
        }
        _ => {
            // Unquoted: take first whitespace-delimited token
            let end = s
                .find(|c: char| c.is_whitespace() || c == ',')
                .unwrap_or(s.len());
            (s[..end].trim(), s[end..].trim())
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("Invalid database header")]
    InvalidHeader,

    #[error("Invalid magic number")]
    InvalidMagic,

    #[error("Invalid text encoding")]
    InvalidEncoding,

    #[error("Invalid page")]
    InvalidPage,

    #[error("Invalid page number")]
    InvalidPageNumber,

    #[error("Invalid cell")]
    InvalidCell,

    #[error("Invalid record")]
    InvalidRecord,

    #[error("Invalid value")]
    InvalidValue,

    #[error("Invalid type code")]
    InvalidTypeCode,

    #[error("Read error")]
    ReadError,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_column_parsing_simple() {
        let sql = "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)";
        let cols = parse_create_table_columns(sql);
        assert_eq!(cols.len(), 3);
        assert_eq!(cols[0].name, "id");
        assert_eq!(cols[1].name, "name");
        assert_eq!(cols[2].name, "age");
        assert!(cols[0].pk);
    }

    #[test]
    fn test_column_parsing_with_constraints() {
        let sql = "CREATE TABLE t (a TEXT NOT NULL, b INTEGER DEFAULT 0, c REAL, PRIMARY KEY(a))";
        let cols = parse_create_table_columns(sql);
        assert_eq!(cols.len(), 3);
        assert_eq!(cols[0].name, "a");
        assert!(cols[0].not_null);
        assert_eq!(cols[1].name, "b");
        assert_eq!(cols[2].name, "c");
    }

    #[test]
    fn test_column_parsing_quoted_names() {
        let sql = r#"CREATE TABLE "my table" ("col one" TEXT, "col two" INTEGER)"#;
        let cols = parse_create_table_columns(sql);
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0].name, "col one");
        assert_eq!(cols[1].name, "col two");
    }

    #[test]
    fn test_column_parsing_empty() {
        let sql = "CREATE TABLE empty ()";
        let cols = parse_create_table_columns(sql);
        assert_eq!(cols.len(), 0);
    }

    #[test]
    fn test_column_parsing_no_parens() {
        let cols = parse_create_table_columns("CREATE TABLE t");
        assert_eq!(cols.len(), 0);
    }

    #[test]
    fn test_varint_small() {
        // 0x01 = 1
        assert_eq!(read_varint(&[0x01]), (1, 1));
        // 0x81 0x01 = 129 (in unsigned varint) but in SQLite varint this is signed
        let (_val, len) = read_varint(&[0x81, 0x01]);
        assert_eq!(len, 2);
        // 0x7f = 127
        assert_eq!(read_varint(&[0x7f]), (127, 1));
    }

    #[test]
    fn test_varint_null() {
        // Type code 0 = NULL
        assert_eq!(read_varint(&[0x00]), (0, 1));
    }

    #[test]
    fn test_record_parse_simple() {
        // Payload with header_size=3, type=0x0f (text 1 byte, odd), type=0x01 (int 1 byte)
        // header: 0x03 0x0f 0x01
        // data: 'a' + 0x42
        let data = vec![0x03, 0x0f, 0x01, b'a', 0x42];
        let record = Record::parse(&data).unwrap();
        assert_eq!(record.values.len(), 2);
        assert_eq!(record.values[0], Value::Text("a".to_string()));
        assert_eq!(record.values[1], Value::Integer(0x42));
    }

    #[test]
    fn test_record_parse_null() {
        // header=2, type=0x00 (NULL)
        let data = vec![0x02, 0x00];
        let record = Record::parse(&data).unwrap();
        assert_eq!(record.values.len(), 1);
        assert_eq!(record.values[0], Value::Null);
    }

    #[test]
    fn test_record_parse_integer_8() {
        // header=2, type=0x08 (integer 0), type=0x09 (integer 1)
        let data = vec![0x03, 0x08, 0x09];
        let record = Record::parse(&data).unwrap();
        assert_eq!(record.values.len(), 2);
        assert_eq!(record.values[0], Value::Integer(0));
        assert_eq!(record.values[1], Value::Integer(1));
    }

    #[test]
    fn test_value_to_sql() {
        assert_eq!(Value::Null.to_sql(), "NULL");
        assert_eq!(Value::Integer(42).to_sql(), "42");
        assert_eq!(Value::Text("hello".into()).to_sql(), "'hello'");
        assert_eq!(Value::Text("it's".into()).to_sql(), "'it''s'");
        assert_eq!(Value::Real(3.14).to_sql().len() > 0, true);
        assert_eq!(Value::Blob(vec![0xFF, 0x00]).to_sql(), "X'FF00'");
    }

    #[test]
    fn test_master_entry_parse_columns() {
        let entry = MasterEntry {
            entry_type: "table".into(),
            name: "users".into(),
            table_name: "users".into(),
            root_page: 2,
            sql: "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, email TEXT)".into(),
        };
        let cols = entry.parse_columns();
        assert_eq!(cols.len(), 3);
        assert_eq!(cols[0].name, "id");
        assert_eq!(cols[1].name, "name");
        assert_eq!(cols[2].name, "email");
    }

    #[test]
    fn test_database_header_validation() {
        // Invalid magic
        let page = vec![0u8; 100];
        let result = TableScanner::parse_header(&page);
        assert!(result.is_err());
    }
}
