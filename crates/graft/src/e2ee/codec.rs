use super::{EncryptionError, Result};

// Also bounds temporary AAD/derivation inputs. Object payloads are never encoded here.
const MAX_ENCODED_BYTES: usize = 16 * 1024 * 1024;

pub(crate) fn encode(fields: &[&[u8]]) -> Result<Vec<u8>> {
    let count = u16::try_from(fields.len()).map_err(|_| EncryptionError::TooLarge)?;
    let mut size = 2_usize;
    for field in fields {
        u32::try_from(field.len()).map_err(|_| EncryptionError::TooLarge)?;
        size = size
            .checked_add(4)
            .and_then(|n| n.checked_add(field.len()))
            .filter(|n| *n <= MAX_ENCODED_BYTES)
            .ok_or(EncryptionError::TooLarge)?;
    }
    let mut bytes = Vec::with_capacity(size);
    bytes.extend_from_slice(&count.to_be_bytes());
    for field in fields {
        bytes.extend_from_slice(&(field.len() as u32).to_be_bytes());
        bytes.extend_from_slice(field);
    }
    Ok(bytes)
}

pub(crate) fn decode(bytes: &[u8], expected: usize, maximum: usize) -> Result<Vec<&[u8]>> {
    let fields = decode_bounded(bytes, expected, maximum)?;
    if fields.len() != expected {
        return Err(EncryptionError::InvalidEncoding);
    }
    Ok(fields)
}

pub(crate) fn decode_bounded(
    bytes: &[u8],
    maximum_fields: usize,
    maximum: usize,
) -> Result<Vec<&[u8]>> {
    if bytes.len() > maximum {
        return Err(EncryptionError::TooLarge);
    }
    let mut cursor = Cursor::new(bytes);
    let count = u16::from_be_bytes(cursor.array()?);
    if usize::from(count) > maximum_fields {
        return Err(EncryptionError::InvalidEncoding);
    }
    let mut fields = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        let len = u32::from_be_bytes(cursor.array()?) as usize;
        fields.push(cursor.take(len)?);
    }
    cursor.finish()?;
    Ok(fields)
}

pub(crate) struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    pub(crate) fn take(&mut self, length: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(EncryptionError::InvalidEncoding)?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or(EncryptionError::InvalidEncoding)?;
        self.offset = end;
        Ok(bytes)
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.take(N)?
            .try_into()
            .map_err(|_| EncryptionError::InvalidEncoding)
    }

    pub(crate) fn finish(self) -> Result<()> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(EncryptionError::InvalidEncoding)
        }
    }
}
