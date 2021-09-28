use arrow2::datatypes::DataType;
use thiserror::Error;

use super::frame::to_arrow;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Arrow serialization error: {0}")]
    ArrowSerialization(#[from] to_arrow::Error),
    #[error("Data type mismatch in field {} (existing: {existing}, new: {new})", field.as_deref().unwrap_or("unnamed field"))]
    DataTypeMismatch {
        existing: DataType,
        new: DataType,
        field: Option<String>,
    },
    #[error("Unsupported Arrow data type: {0}")]
    UnsupportedArrowDataType(DataType),
}
