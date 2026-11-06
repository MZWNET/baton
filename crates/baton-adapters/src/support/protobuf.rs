// The protobuf wire format, read and written without schemas because Antigravity and Cursor publish none

use std::str;

use anyhow::{Context, Result, bail};

// A field as the wire stores it; nested messages, strings and packed repeated fields are all bytes
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Value<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    Fixed32(u32),
}

// The fields of one message in wire order, a repeated field once per element
#[derive(Debug)]
pub(crate) struct Message<'a> {
    fields: Vec<(u32, Value<'a>)>,
}

impl<'a> Message<'a> {
    pub(crate) fn parse(mut bytes: &'a [u8]) -> Result<Message<'a>> {
        let mut fields = Vec::new();
        while !bytes.is_empty() {
            let key = varint(&mut bytes)?;
            let number = u32::try_from(key >> 3).context("protobuf field number out of range")?;
            let value = match key & 7 {
                0 => Value::Varint(varint(&mut bytes)?),
                1 => Value::Fixed64(u64::from_le_bytes(take(&mut bytes, 8)?.try_into()?)),
                2 => {
                    let length = usize::try_from(varint(&mut bytes)?)?;
                    Value::Bytes(take(&mut bytes, length)?)
                }
                5 => Value::Fixed32(u32::from_le_bytes(take(&mut bytes, 4)?.try_into()?)),
                wire => bail!("unsupported protobuf wire type {wire}"),
            };
            fields.push((number, value));
        }
        Ok(Message { fields })
    }

    // A later occurrence of a singular field overrides earlier ones
    fn last(&self, number: u32) -> Option<Value<'a>> {
        self.fields
            .iter()
            .rev()
            .find(|(field, _)| *field == number)
            .map(|(_, value)| *value)
    }

    pub(crate) fn varint(&self, number: u32) -> Option<u64> {
        match self.last(number)? {
            Value::Varint(value) => Some(value),
            _ => None,
        }
    }

    pub(crate) fn bytes(&self, number: u32) -> Option<&'a [u8]> {
        match self.last(number)? {
            Value::Bytes(bytes) => Some(bytes),
            _ => None,
        }
    }

    pub(crate) fn string(&self, number: u32) -> Option<&'a str> {
        str::from_utf8(self.bytes(number)?).ok()
    }

    pub(crate) fn message(&self, number: u32) -> Option<Message<'a>> {
        Message::parse(self.bytes(number)?).ok()
    }

    pub(crate) fn repeated(&self, number: u32) -> impl Iterator<Item = &'a [u8]> + '_ {
        self.fields
            .iter()
            .filter_map(move |(field, value)| match value {
                Value::Bytes(bytes) if *field == number => Some(*bytes),
                _ => None,
            })
    }
}

fn varint(bytes: &mut &[u8]) -> Result<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let (&byte, rest) = bytes.split_first().context("truncated protobuf varint")?;
        *bytes = rest;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    bail!("protobuf varint longer than 10 bytes")
}

fn take<'a>(bytes: &mut &'a [u8], length: usize) -> Result<&'a [u8]> {
    if bytes.len() < length {
        bail!("truncated protobuf field");
    }
    let (taken, rest) = bytes.split_at(length);
    *bytes = rest;
    Ok(taken)
}

// Builds a message field by field, in the order the fields are added
#[derive(Default)]
pub(crate) struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    pub(crate) fn varint(mut self, number: u32, value: u64) -> Encoder {
        self.key(number, 0);
        self.put_varint(value);
        self
    }

    pub(crate) fn bytes(mut self, number: u32, value: &[u8]) -> Encoder {
        self.key(number, 2);
        self.put_varint(value.len() as u64);
        self.bytes.extend_from_slice(value);
        self
    }

    pub(crate) fn string(self, number: u32, value: &str) -> Encoder {
        self.bytes(number, value.as_bytes())
    }

    pub(crate) fn message(self, number: u32, value: Encoder) -> Encoder {
        self.bytes(number, &value.bytes)
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        self.bytes
    }

    fn key(&mut self, number: u32, wire: u64) {
        self.put_varint((u64::from(number) << 3) | wire);
    }

    fn put_varint(&mut self, mut value: u64) {
        while value >= 0x80 {
            self.bytes.push((value as u8 & 0x7f) | 0x80);
            value >>= 7;
        }
        self.bytes.push(value as u8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_messages_parse_back() {
        let nested = Encoder::default().string(2, "héllo");
        let bytes = Encoder::default()
            .varint(1, 300)
            .message(140, nested)
            .bytes(8, b"a")
            .bytes(8, b"b")
            .finish();
        let message = Message::parse(&bytes).unwrap();
        assert_eq!(message.varint(1), Some(300));
        assert_eq!(message.message(140).unwrap().string(2), Some("héllo"));
        assert_eq!(message.repeated(8).collect::<Vec<_>>(), [b"a", b"b"]);
        assert!(
            Message::parse(&bytes[..bytes.len() - 1]).is_err(),
            "a truncated field is rejected"
        );
    }
}
