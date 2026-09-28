//! Minimal protobuf wire reader/writer. Field numbers follow the observed Agent RPC;
//! unknown fields are skipped, not interpreted as JSON or OpenAI responses.
use super::{invalid, Result};

pub const LIMIT: usize = 32 * 1024 * 1024;
#[derive(Default)]
pub struct Message(pub Vec<u8>);
impl Message {
    pub fn bytes(mut self, field: u32, value: impl AsRef<[u8]>) -> Self {
        varint(&mut self.0, ((field as u64) << 3) | 2);
        varint(&mut self.0, value.as_ref().len() as u64);
        self.0.extend_from_slice(value.as_ref());
        self
    }
    pub fn number(mut self, field: u32, value: u64) -> Self {
        varint(&mut self.0, (field as u64) << 3);
        varint(&mut self.0, value);
        self
    }
}
fn varint(out: &mut Vec<u8>, mut n: u64) {
    while n >= 128 {
        out.push(n as u8 | 128);
        n >>= 7;
    }
    out.push(n as u8);
}
fn take_varint(input: &mut &[u8]) -> Result<u64> {
    let mut n = 0;
    for shift in (0..70).step_by(7) {
        let b = *input.first().ok_or_else(|| invalid("truncated varint"))?;
        *input = &input[1..];
        if shift == 63 && b > 1 {
            return Err(invalid("overflowing varint"));
        }
        n |= ((b & 127) as u64) << shift;
        if b < 128 {
            return Ok(n);
        }
    }
    Err(invalid("invalid varint"))
}
#[derive(Debug)]
pub enum Value<'a> {
    Bytes(&'a [u8]),
    Number(u64),
    Fixed,
}
pub struct Fields<'a>(pub Vec<(u32, Value<'a>)>);
impl<'a> Fields<'a> {
    pub fn parse(mut input: &'a [u8]) -> Result<Self> {
        let mut fields = Vec::new();
        while !input.is_empty() {
            let key = take_varint(&mut input)?;
            if key >> 3 == 0 || key >> 3 > 0x1fff_ffff {
                return Err(invalid("invalid protobuf field"));
            }
            let value = match key & 7 {
                0 => Value::Number(take_varint(&mut input)?),
                kind @ (1 | 2 | 5) => {
                    let len = match kind {
                        1 => 8,
                        5 => 4,
                        _ => usize::try_from(take_varint(&mut input)?)
                            .map_err(|_| invalid("length overflow"))?,
                    };
                    if len > input.len() {
                        return Err(invalid("truncated protobuf field"));
                    }
                    let data = &input[..len];
                    input = &input[len..];
                    if kind == 2 {
                        Value::Bytes(data)
                    } else {
                        Value::Fixed
                    }
                }
                _ => return Err(invalid("unsupported protobuf wire type")),
            };
            fields.push(((key >> 3) as u32, value));
        }
        Ok(Self(fields))
    }
    pub fn all(&self, tag: u32) -> impl Iterator<Item = &'a [u8]> + '_ {
        self.0.iter().filter_map(move |(n, v)| match v {
            Value::Bytes(b) if *n == tag => Some(*b),
            _ => None,
        })
    }
    pub fn bytes(&self, tag: u32) -> &'a [u8] {
        self.all(tag).last().unwrap_or_default()
    }
    pub fn has(&self, tag: u32) -> bool {
        self.0.iter().any(|(n, _)| *n == tag)
    }
    pub fn text(&self, tag: u32) -> Result<String> {
        String::from_utf8(self.bytes(tag).to_vec()).map_err(|_| invalid("invalid UTF-8"))
    }
    pub fn number(&self, tag: u32) -> u64 {
        self.0
            .iter()
            .rev()
            .find_map(|(n, v)| match v {
                Value::Number(x) if *n == tag => Some(*x),
                _ => None,
            })
            .unwrap_or(0)
    }
}
pub fn frame(payload: &[u8]) -> Result<Vec<u8>> {
    if payload.len() > LIMIT {
        return Err(invalid("frame exceeds 32 MiB"));
    }
    let mut out = vec![0];
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}
#[derive(Default)]
pub struct Decoder {
    pending: Vec<u8>,
}
impl Decoder {
    pub fn push(&mut self, input: &[u8]) {
        self.pending.extend_from_slice(input);
    }
    pub fn next(&mut self) -> Result<Option<(u8, Vec<u8>)>> {
        if self.pending.len() < 5 {
            return Ok(None);
        }
        let flag = self.pending[0];
        if flag != 0 && flag != 2 {
            return Err(invalid("compressed or unknown Connect frame flags"));
        }
        let len = u32::from_be_bytes(self.pending[1..5].try_into().unwrap()) as usize;
        if len > LIMIT {
            return Err(invalid("frame exceeds 32 MiB"));
        }
        if self.pending.len() < 5 + len {
            return Ok(None);
        }
        let payload = self.pending[5..5 + len].to_vec();
        self.pending.drain(..5 + len);
        Ok(Some((flag, payload)))
    }
}
