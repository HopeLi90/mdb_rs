//! 字段定义：对应 ArcObjects 的 `IField / IFields / esriFieldType`。

use std::collections::HashMap;

use crate::error::{PgdbError, Result};

/// 字段类型（对应 `esriFieldType` 的子集 + Access 自有类型）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FieldType {
    /// 短整型 esriFieldTypeSmallInteger
    SmallInteger,
    /// 长整型 esriFieldTypeInteger
    Integer,
    /// 单精度 esriFieldTypeSingle
    Single,
    /// 双精度 esriFieldTypeDouble
    Double,
    /// 文本 esriFieldTypeString
    String,
    /// 日期 esriFieldTypeDate
    Date,
    /// 二进制/OLE 对象 esriFieldTypeBlob
    Blob,
    /// 全局标识符 esriFieldTypeGUID
    Guid,
    /// 对象 ID esriFieldTypeOID
    Oid,
    /// 几何字段 esriFieldTypeGeometry
    Geometry,
    /// 栅格 esriFieldTypeRaster
    Raster,
    /// XML
    Xml,
}

impl FieldType {
    /// 由 Jet/Access 的 ODBC SQL 类型名推断
    pub fn from_sql_type(sql_type: &str) -> Self {
        match sql_type.to_ascii_uppercase().as_str() {
            "SMALLINT" | "SHORT" | "BYTE" | "TINYINT" => FieldType::SmallInteger,
            "INTEGER" | "LONG" | "COUNTER" | "AUTOINCREMENT" | "BIGINT" => FieldType::Integer,
            "REAL" | "SINGLE" => FieldType::Single,
            "DOUBLE" | "FLOAT" | "NUMERIC" | "DECIMAL" | "CURRENCY" => FieldType::Double,
            "BIT" | "YESNO" | "BOOLEAN" => FieldType::SmallInteger,
            "VARCHAR" | "CHAR" | "TEXT" | "LONGCHAR" | "MEMO" | "NVARCHAR" | "NCHAR" => {
                FieldType::String
            }
            "DATETIME" | "DATE" | "TIMESTAMP" | "TIME" => FieldType::Date,
            "VARBINARY" | "BINARY" | "LONGVARBINARY" | "OLEOBJECT" | "IMAGE" | "BLOB" => {
                FieldType::Blob
            }
            "GUID" | "UNIQUEIDENTIFIER" => FieldType::Guid,
            _ => FieldType::String,
        }
    }

    /// 中文名称
    pub fn label(&self) -> &'static str {
        match self {
            FieldType::SmallInteger => "短整型",
            FieldType::Integer => "长整型",
            FieldType::Single => "单精度",
            FieldType::Double => "双精度",
            FieldType::String => "文本",
            FieldType::Date => "日期",
            FieldType::Blob => "二进制",
            FieldType::Guid => "GUID",
            FieldType::Oid => "对象ID",
            FieldType::Geometry => "几何",
            FieldType::Raster => "栅格",
            FieldType::Xml => "XML",
        }
    }

    /// 是否为数值类型
    pub fn is_numeric(&self) -> bool {
        matches!(
            self,
            FieldType::SmallInteger
                | FieldType::Integer
                | FieldType::Single
                | FieldType::Double
                | FieldType::Oid
        )
    }
}

/// 单个字段定义（对应 `IField`）
#[derive(Debug, Clone)]
pub struct Field {
    /// 物理字段名
    pub name: String,
    /// 别名
    pub alias: Option<String>,
    /// 类型
    pub field_type: FieldType,
    /// 是否为几何字段
    pub is_geometry: bool,
    /// 长度
    pub length: Option<usize>,
    /// 精度
    pub precision: Option<usize>,
    /// 小数位
    pub scale: Option<usize>,
    /// 是否可为空
    pub nullable: bool,
    /// 是否必填（PGDB 中 NOT NULL）
    pub required: bool,
    /// 该 catalog 能否接受些字段 参与编辑
    pub editable: bool,
}

impl Field {
    /// 构造常规字段
    pub fn new(name: impl Into<String>, field_type: FieldType) -> Self {
        Self {
            name: name.into(),
            alias: None,
            field_type,
            is_geometry: false,
            length: None,
            precision: None,
            scale: None,
            nullable: true,
            required: false,
            editable: true,
        }
    }

    /// 是否自增字段
    pub fn is_auto_increment(&self) -> bool {
        false
    }
}

/// 字段集合（对应 `IFields`），提供 O(1) 名称查找。
#[derive(Debug, Clone, Default)]
pub struct Fields {
    fields: Vec<Field>,
    index: HashMap<String, usize>,
}

impl Fields {
    /// 由字段向量构造（同时建立名称索引，大小写不敏感）
    pub fn new(mut fields: Vec<Field>) -> Self {
        let mut index = HashMap::new();
        for (i, f) in fields.iter().enumerate() {
            index.insert(f.name.to_lowercase(), i);
        }
        // 别名索引同样建立，便于 OpenLike ArcMap 的别名访问
        for (i, f) in fields.iter().enumerate() {
            if let Some(a) = &f.alias {
                index.entry(a.to_lowercase()).or_insert(i);
            }
        }
        fields.shrink_to_fit();
        Self { fields, index }
    }

    /// 字段数量（`IFields::FieldCount`）
    pub fn count(&self) -> usize {
        self.fields.len()
    }

    /// 按下标取字段（`IFields::get_Field`）
    pub fn field(&self, index: usize) -> Option<&Field> {
        self.fields.get(index)
    }

    /// 按名称查找字段下标（`IFields::FindField`）
    pub fn find(&self, name: &str) -> Option<usize> {
        self.index.get(&name.to_lowercase()).copied()
    }

    /// 按名称查找字段（`IFields::get_Field` + FindField）
    pub fn by_name(&self, name: &str) -> Option<&Field> {
        self.find(name).and_then(|i| self.fields.get(i))
    }

    /// 迭代所有字段
    pub fn iter(&self) -> impl Iterator<Item = &Field> {
        self.fields.iter()
    }

    /// 几何字段下标，没有返回 None
    pub fn shape_index(&self) -> Option<usize> {
        self.fields.iter().position(|f| f.is_geometry)
    }

    /// 所有字段切片
    pub fn as_slice(&self) -> &[Field] {
        &self.fields
    }

    /// 追加字段
    pub fn push(&mut self, f: Field) {
        if !self.index.contains_key(&f.name.to_lowercase()) {
            self.index.insert(f.name.to_lowercase(), self.fields.len());
            self.fields.push(f);
        }
    }

    /// 校验字段名存在，否则报错
    pub fn require_field(&self, name: &str) -> Result<usize> {
        self.find(name)
            .ok_or_else(|| PgdbError::NotFound(format!("字段 {name} 不存在")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fields_lookup() {
        let fields = Fields::new(vec![
            Field::new("OBJECTID", FieldType::Oid),
            Field::new("NAME", FieldType::String),
        ]);
        assert_eq!(fields.find("name"), Some(1));
        assert_eq!(fields.find("Nope"), None);
        assert_eq!(fields.count(), 2);
    }
}
