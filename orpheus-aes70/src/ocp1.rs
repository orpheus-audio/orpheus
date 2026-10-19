//! OCP.1, AES70's binary protocol over TCP and WebSocket: messages, and
//! the data types they carry, all big-endian.
//!
//! A message is a header (sync byte `0x3B`, protocol version, size, type,
//! count) followed by PDUs of that one type: commands, responses,
//! notifications or a keepalive.

use alloc::string::String;
use alloc::vec::Vec;

/// First byte of every message.
const SYNC: u8 = 0x3B;
/// The protocol version this device sends; every version reads it.
pub const PROTOCOL_VERSION: u16 = 1;
/// Bytes of a message header, the sync byte included.
const HEADER: usize = 10;
/// The largest message taken, to bound what a client can make us buffer.
pub const MAX_MESSAGE: usize = 1 << 20;

/// Why data could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// It ends early.
    Short,
    /// It isn't what OCP.1 allows.
    BadFormat,
}

/// Reads OCP.1 data types.
pub struct Reader<'a> {
    data: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    /// Whether all was read.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if self.data.len() < n {
            return Err(Error::Short);
        }
        let (head, rest) = self.data.split_at(n);
        self.data = rest;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        Ok(self.bytes(N)?.try_into().expect("N bytes"))
    }

    pub fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.array::<1>()?[0])
    }

    pub fn u16(&mut self) -> Result<u16, Error> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    pub fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    pub fn f32(&mut self) -> Result<f32, Error> {
        Ok(f32::from_be_bytes(self.array()?))
    }

    /// An `OcaBoolean`: one byte, 0 or 1.
    pub fn bool(&mut self) -> Result<bool, Error> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Error::BadFormat),
        }
    }

    /// An `OcaString`: its length in code points, then UTF-8.
    pub fn string(&mut self) -> Result<String, Error> {
        let chars = self.u16()?;
        let mut len = 0;
        for _ in 0..chars {
            let lead = *self.data.get(len).ok_or(Error::Short)?;
            len += match lead {
                0x00..=0x7F => 1,
                0xC0..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF7 => 4,
                _ => return Err(Error::BadFormat),
            };
        }
        let text = core::str::from_utf8(self.bytes(len)?).map_err(|_| Error::BadFormat)?;
        Ok(text.into())
    }

    /// An `OcaBlob`: its length, then the bytes.
    pub fn blob(&mut self) -> Result<&'a [u8], Error> {
        let len = self.u16()?;
        self.bytes(len.into())
    }

    /// The length of an `OcaList` or `OcaMap`.
    pub fn count(&mut self) -> Result<usize, Error> {
        Ok(self.u16()?.into())
    }

    /// An ID of two numbers, such as an `OcaMethodID`, `OcaPropertyID` or
    /// `OcaEventID`: the definition level, then the index.
    pub fn id(&mut self) -> Result<Id, Error> {
        Ok(Id(self.u16()?, self.u16()?))
    }

    /// An `OcaEvent`: the emitting object, then the event.
    pub fn event(&mut self) -> Result<Event, Error> {
        Ok(Event {
            emitter: self.u32()?,
            id: self.id()?,
        })
    }
}

/// Writes OCP.1 data types.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Writer {
    data: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.data
    }

    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.data.extend_from_slice(b);
        self
    }

    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.bytes(&[v])
    }

    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.bytes(&v.to_be_bytes())
    }

    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.bytes(&v.to_be_bytes())
    }

    pub fn f32(&mut self, v: f32) -> &mut Self {
        self.bytes(&v.to_be_bytes())
    }

    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.u8(v.into())
    }

    /// An `OcaString`, cut to the 65535 code points it can hold.
    pub fn string(&mut self, s: &str) -> &mut Self {
        let end = s
            .char_indices()
            .nth(usize::from(u16::MAX))
            .map_or(s.len(), |(i, _)| i);
        let s = &s[..end];
        self.u16(s.chars().count() as u16).bytes(s.as_bytes())
    }

    /// An `OcaBlob`, cut to the 65535 bytes it can hold.
    pub fn blob(&mut self, b: &[u8]) -> &mut Self {
        let b = &b[..b.len().min(usize::from(u16::MAX))];
        self.u16(b.len() as u16).bytes(b)
    }

    /// The length of an `OcaList` or `OcaMap`, which the items follow.
    pub fn count(&mut self, n: usize) -> &mut Self {
        self.u16(n.min(usize::from(u16::MAX)) as u16)
    }

    pub fn id(&mut self, id: Id) -> &mut Self {
        self.u16(id.0).u16(id.1)
    }

    /// An `OcaClassIdentification`: the class ID's fields, then the
    /// version.
    pub fn class(&mut self, id: &[u16], version: u16) -> &mut Self {
        self.count(id.len());
        for &field in id {
            self.u16(field);
        }
        self.u16(version)
    }
}

/// A method, property or event of a class: its definition level, then its
/// index there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(pub u16, pub u16);

/// An event of an object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Event {
    pub emitter: u32,
    pub id: Id,
}

/// How a request went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    Ok = 0,
    DeviceError = 2,
    BadFormat = 4,
    BadONo = 5,
    ParameterError = 6,
    ParameterOutOfRange = 7,
    NotImplemented = 8,
    InvalidRequest = 9,
    ProcessingFailed = 10,
    BadMethod = 11,
    PermissionDenied = 15,
}

impl From<Error> for Status {
    fn from(_: Error) -> Self {
        Status::BadFormat
    }
}

/// A command from a controller.
#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    /// What the response names it by.
    pub handle: u32,
    /// The object it is for.
    pub target: u32,
    pub method: Id,
    /// The encoded parameters.
    pub params: Vec<u8>,
    /// Whether it wants a response (`CommandRrq`).
    pub response: bool,
}

/// What a controller sends.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Command(Command),
    /// The longest the controller stays silent, in milliseconds; 0 for no
    /// keepalive.
    KeepAlive(u32),
}

/// Message types.
const COMMAND: u8 = 0;
const COMMAND_RRQ: u8 = 1;
const NOTIFICATION: u8 = 2;
const RESPONSE: u8 = 3;
const KEEPALIVE: u8 = 4;
const NOTIFICATION2: u8 = 5;

/// Reads the first message in `data`: what it says and how many bytes it
/// took; `None` while it is incomplete. Messages a device doesn't take
/// (responses and notifications) read as nothing.
pub fn decode(data: &[u8]) -> Result<Option<(Vec<Incoming>, usize)>, Error> {
    if data.len() < HEADER {
        return Ok(None);
    }
    let mut r = Reader::new(data);
    if r.u8()? != SYNC {
        return Err(Error::BadFormat);
    }
    let _version = r.u16()?;
    // The size counts from the protocol version on.
    let size = r.u32()? as usize;
    if !(HEADER - 1..=MAX_MESSAGE).contains(&size) {
        return Err(Error::BadFormat);
    }
    let kind = r.u8()?;
    let count = r.u16()?;
    if data.len() < size + 1 {
        return Ok(None);
    }
    let mut r = Reader::new(&data[HEADER..size + 1]);
    let mut pdus = Vec::new();
    match kind {
        COMMAND | COMMAND_RRQ => {
            for _ in 0..count {
                let len = r.u32()? as usize;
                let handle = r.u32()?;
                let target = r.u32()?;
                let method = r.id()?;
                let _param_count = r.u8()?;
                let params = r.bytes(len.checked_sub(17).ok_or(Error::BadFormat)?)?;
                pdus.push(Incoming::Command(Command {
                    handle,
                    target,
                    method,
                    params: params.into(),
                    response: kind == COMMAND_RRQ,
                }));
            }
        }
        KEEPALIVE => {
            let millis = match size + 1 - HEADER {
                2 => u32::from(r.u16()?) * 1000,
                4 => r.u32()?,
                _ => return Err(Error::BadFormat),
            };
            pdus.push(Incoming::KeepAlive(millis));
        }
        NOTIFICATION | RESPONSE | NOTIFICATION2 => return Ok(Some((Vec::new(), size + 1))),
        _ => return Err(Error::BadFormat),
    }
    if !r.is_empty() {
        return Err(Error::BadFormat);
    }
    Ok(Some((pdus, size + 1)))
}

/// What a device sends.
#[derive(Debug, Clone, PartialEq)]
pub enum Outgoing {
    Response {
        handle: u32,
        status: Status,
        /// How many parameters `params` holds.
        count: u8,
        params: Vec<u8>,
    },
    /// An event for a subscription made with `AddSubscription`: it calls
    /// the controller's method `method` of object `target`, passing
    /// `context` back.
    Notification {
        target: u32,
        method: Id,
        context: Vec<u8>,
        event: Event,
        data: Vec<u8>,
    },
    /// An event for a subscription made with `AddSubscription2`.
    Notification2 { event: Event, data: Vec<u8> },
    /// The longest this device stays silent, in milliseconds.
    KeepAlive(u32),
}

impl Outgoing {
    /// A response without parameters.
    pub fn status(handle: u32, status: Status) -> Self {
        Self::Response {
            handle,
            status,
            count: 0,
            params: Vec::new(),
        }
    }

    /// This as a message of its own.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        let kind = match self {
            Self::Response {
                handle,
                status,
                count,
                params,
            } => {
                w.u32(10 + params.len() as u32)
                    .u32(*handle)
                    .u8(*status as u8)
                    .u8(*count)
                    .bytes(params);
                RESPONSE
            }
            Self::Notification {
                target,
                method,
                context,
                event,
                data,
            } => {
                let context = &context[..context.len().min(usize::from(u16::MAX))];
                w.u32(23 + (context.len() + data.len()) as u32)
                    .u32(*target)
                    .id(*method)
                    // The context and the event data.
                    .u8(2)
                    .blob(context)
                    .u32(event.emitter)
                    .id(event.id)
                    .bytes(data);
                NOTIFICATION
            }
            Self::Notification2 { event, data } => {
                w.u32(13 + data.len() as u32)
                    .u32(event.emitter)
                    .id(event.id)
                    // Not an exception.
                    .u8(0)
                    .bytes(data);
                NOTIFICATION2
            }
            Self::KeepAlive(millis) => {
                if millis % 1000 == 0 && millis / 1000 <= u32::from(u16::MAX) {
                    w.u16((millis / 1000) as u16);
                } else {
                    w.u32(*millis);
                }
                KEEPALIVE
            }
        };
        let body = w.into_bytes();
        let mut m = Writer::new();
        m.u8(SYNC)
            .u16(PROTOCOL_VERSION)
            .u32((HEADER - 1 + body.len()) as u32)
            .u8(kind)
            .u16(1)
            .bytes(&body);
        m.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// A `CommandRrq` as AES70.js sends it: `GetClassIdentification` of
    /// the root block, handle 7.
    const GET_CLASS: [u8; 27] = [
        0x3B, 0x00, 0x04, 0x00, 0x00, 0x00, 0x1A, 0x01, 0x00, 0x01, // header
        0x00, 0x00, 0x00, 0x11, 0x00, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00, 0x64, 0x00, 0x01, 0x00,
        0x01, 0x00,
    ];

    #[test]
    fn reads_a_command() {
        let (pdus, used) = decode(&GET_CLASS).unwrap().unwrap();
        assert_eq!(used, GET_CLASS.len());
        assert_eq!(
            pdus,
            vec![Incoming::Command(Command {
                handle: 7,
                target: 100,
                method: Id(1, 1),
                params: vec![],
                response: true,
            })]
        );
        assert_eq!(decode(&GET_CLASS[..20]), Ok(None));
        let mut bad = GET_CLASS;
        bad[0] = 0;
        assert_eq!(decode(&bad), Err(Error::BadFormat));
    }

    #[test]
    fn reads_keepalives_in_seconds_and_milliseconds() {
        for millis in [5000, 1500] {
            let message = Outgoing::KeepAlive(millis).encode();
            let (pdus, used) = decode(&message).unwrap().unwrap();
            assert_eq!(
                (pdus, used),
                (vec![Incoming::KeepAlive(millis)], message.len())
            );
        }
    }

    #[test]
    fn writes_a_response() {
        let mut w = Writer::new();
        w.class(&[1, 1, 3], 3);
        let params = w.into_bytes();
        let message = Outgoing::Response {
            handle: 7,
            status: Status::Ok,
            count: 1,
            params,
        }
        .encode();
        assert_eq!(
            message,
            [
                0x3B, 0x00, 0x01, 0x00, 0x00, 0x00, 0x1D, 0x03, 0x00, 0x01, // header
                0x00, 0x00, 0x00, 0x14, 0x00, 0x00, 0x00, 0x07, 0x00, 0x01, // response
                0x00, 0x03, 0x00, 0x01, 0x00, 0x01, 0x00, 0x03, 0x00, 0x03, // class
            ]
        );
    }

    #[test]
    fn strings_count_code_points() {
        let mut w = Writer::new();
        w.string("Küche").blob(&[1, 2]);
        let data = w.into_bytes();
        assert_eq!(&data[..2], &[0, 5]);
        let mut r = Reader::new(&data);
        assert_eq!(r.string().unwrap(), "Küche");
        assert_eq!(r.blob().unwrap(), &[1, 2]);
        assert!(r.is_empty());
        assert_eq!(Reader::new(&[0, 2, b'a']).string(), Err(Error::Short));
    }
}
