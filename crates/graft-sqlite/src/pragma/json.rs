use super::*;

pub(super) fn to_json<T: Serialize>(value: &T) -> Result<String, ErrCtx> {
    serde_json::to_string(value).map_err(|e| ErrCtx::PragmaErr(format!("JSON error: {e}").into()))
}

pub(super) fn is_false(value: &bool) -> bool {
    !*value
}
