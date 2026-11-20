//! The parts of the D-Bus wire format the keyring bus reads and writes:
//! message framing, header fields, a few argument types, and the line-based
//! authentication before the first message.

use std::io::{self, Read, Write};

pub const METHOD_CALL: u8 = 1;
pub const METHOD_RETURN: u8 = 2;
pub const ERROR: u8 = 3;
/// The caller expects no reply.
pub const NO_REPLY_EXPECTED: u8 = 0x1;

/// The largest message the keyring bus relays. The bus allows 128 MiB, but
/// keyring traffic is a few kilobytes.
const MAX_MESSAGE: usize = 16 << 20;
/// The longest authentication line.
const MAX_LINE: usize = 16 << 10;

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("D-Bus: {what}"))
}

/// Reads one whole message into `buf`, replacing what it held.
pub fn read_message(stream: &mut impl Read, buf: &mut Vec<u8>) -> io::Result<()> {
    buf.clear();
    buf.resize(16, 0);
    stream.read_exact(buf)?;
    let little = match buf[0] {
        b'l' => true,
        b'B' => false,
        _ => return Err(invalid("bad byte order")),
    };
    if buf[3] != 1 {
        return Err(invalid("unknown protocol version"));
    }
    let word = |at: usize| {
        let bytes = [buf[at], buf[at + 1], buf[at + 2], buf[at + 3]];
        (if little {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        }) as usize
    };
    let (body, fields) = (word(4), word(12));
    let total = align(16 + fields, 8)
        .checked_add(body)
        .filter(|total| *total <= MAX_MESSAGE)
        .ok_or_else(|| invalid("message too large"))?;
    buf.resize(total, 0);
    stream.read_exact(&mut buf[16..])
}

const fn align(at: usize, to: usize) -> usize {
    (at + to - 1) & !(to - 1)
}

/// What the keyring bus looks at in a message. The strings borrow from it.
#[derive(Debug, Default)]
pub struct Header<'a> {
    pub kind: u8,
    pub flags: u8,
    pub serial: u32,
    pub path: Option<&'a str>,
    pub interface: Option<&'a str>,
    pub member: Option<&'a str>,
    pub destination: Option<&'a str>,
    pub sender: Option<&'a str>,
    pub reply_serial: Option<u32>,
    pub signature: &'a str,
    /// The message carries file descriptors, which the keyring bus never
    /// passes on.
    pub fds: bool,
    /// Where the body starts.
    pub body: usize,
}

impl Header<'_> {
    pub fn wants_reply(&self) -> bool {
        self.kind == METHOD_CALL && self.flags & NO_REPLY_EXPECTED == 0
    }
}

/// Reads values at their D-Bus alignment, which counts from the start of
/// the message.
pub struct Cursor<'a> {
    buf: &'a [u8],
    at: usize,
    little: bool,
}

impl<'a> Cursor<'a> {
    pub fn new(message: &'a [u8], at: usize) -> Self {
        Self {
            buf: message,
            at,
            little: message.first() == Some(&b'l'),
        }
    }

    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let end = self.at.checked_add(n).filter(|end| *end <= self.buf.len());
        let end = end.ok_or_else(|| invalid("message ends early"))?;
        let bytes = &self.buf[self.at..end];
        self.at = end;
        Ok(bytes)
    }

    fn align(&mut self, to: usize) -> io::Result<()> {
        let at = align(self.at, to);
        self.take(at - self.at).map(drop)
    }

    pub fn byte(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u32(&mut self) -> io::Result<u32> {
        self.align(4)?;
        let bytes: [u8; 4] = self.take(4)?.try_into().map_err(|_| invalid("short"))?;
        Ok(if self.little {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    }

    fn text(&mut self, len: usize) -> io::Result<&'a str> {
        let bytes = self.take(len + 1)?;
        if bytes[len] != 0 {
            return Err(invalid("string without its NUL"));
        }
        std::str::from_utf8(&bytes[..len]).map_err(|_| invalid("string is not UTF-8"))
    }

    /// A string or an object path.
    pub fn string(&mut self) -> io::Result<&'a str> {
        let len = self.u32()? as usize;
        self.text(len)
    }

    pub fn signature(&mut self) -> io::Result<&'a str> {
        let len = self.byte()? as usize;
        self.text(len)
    }

    /// An array of strings or object paths.
    pub fn strings(&mut self) -> io::Result<Vec<&'a str>> {
        let len = self.u32()? as usize;
        self.align(4)?;
        let end = self.at + len;
        let mut items = Vec::new();
        while self.at < end {
            items.push(self.string()?);
        }
        Ok(items)
    }

    /// A variant that holds a string, as a property reply does.
    pub fn string_variant(&mut self) -> io::Result<&'a str> {
        match self.signature()? {
            "s" | "o" => self.string(),
            _ => Err(invalid("variant is no string")),
        }
    }

    /// The length of an array and where it ends, with the cursor on its
    /// first element, whose alignment is `to`.
    fn array(&mut self, to: usize) -> io::Result<usize> {
        let len = self.u32()? as usize;
        self.align(to)?;
        (self.at.checked_add(len))
            .filter(|end| *end <= self.buf.len())
            .ok_or_else(|| invalid("array runs past the message"))
    }

    /// A dictionary of strings (`a{ss}`), such as a secret's attributes,
    /// as pairs in the order of the message.
    pub fn string_map(&mut self) -> io::Result<Vec<(&'a str, &'a str)>> {
        let end = self.array(8)?;
        let mut pairs = Vec::new();
        while self.at < end {
            self.align(8)?;
            pairs.push((self.string()?, self.string()?));
        }
        Ok(pairs)
    }

    /// A variant that holds a dictionary of strings, as the reply to
    /// reading a secret's `Attributes` does.
    pub fn string_map_variant(&mut self) -> io::Result<Vec<(&'a str, &'a str)>> {
        match self.signature()? {
            "a{ss}" => self.string_map(),
            _ => Err(invalid("variant is no dictionary of strings")),
        }
    }

    /// The attributes among the properties of a new secret (`a{sv}`), as
    /// `CreateItem` takes them. `None` when there are none, when they
    /// are named twice, or when a property before them has a type other
    /// than a string, an object path, a boolean or a 32-bit number.
    pub fn item_attributes(&mut self) -> io::Result<Option<Vec<(&'a str, &'a str)>>> {
        let end = self.array(8)?;
        let mut attributes = None;
        while self.at < end {
            self.align(8)?;
            let key = self.string()?;
            match (key, self.signature()?) {
                (ITEM_ATTRIBUTES, "a{ss}") if attributes.is_none() => {
                    attributes = Some(self.string_map()?);
                }
                (ITEM_ATTRIBUTES, _) => return Ok(None),
                (_, "s" | "o") => drop(self.string()?),
                (_, "b" | "u" | "i") => drop(self.u32()?),
                _ => return Ok(None),
            }
        }
        Ok(attributes)
    }
}

/// The property of a new secret that holds its attributes.
const ITEM_ATTRIBUTES: &str = "org.freedesktop.Secret.Item.Attributes";

/// Parses the header of a whole message from [`read_message`].
pub fn parse(message: &[u8]) -> io::Result<Header<'_>> {
    let mut cursor = Cursor::new(message, 0);
    cursor.take(1)?;
    let kind = cursor.byte()?;
    let flags = cursor.byte()?;
    cursor.take(1)?;
    cursor.u32()?;
    let serial = cursor.u32()?;
    let fields_len = cursor.u32()? as usize;
    let end = 16 + fields_len;
    let mut header = Header {
        kind,
        flags,
        serial,
        body: align(end, 8),
        ..Header::default()
    };
    while cursor.at < end {
        cursor.align(8)?;
        let code = cursor.byte()?;
        let signature = cursor.signature()?;
        match (code, signature) {
            (1, "o") => header.path = Some(cursor.string()?),
            (2, "s") => header.interface = Some(cursor.string()?),
            (3, "s") => header.member = Some(cursor.string()?),
            (6, "s") => header.destination = Some(cursor.string()?),
            (7, "s") => header.sender = Some(cursor.string()?),
            (4, "s") => drop(cursor.string()?),
            (5, "u") => header.reply_serial = Some(cursor.u32()?),
            (8, "g") => header.signature = cursor.signature()?,
            (9, "u") => header.fds = cursor.u32()? > 0,
            _ => return Err(invalid("unknown header field")),
        }
    }
    if header.body > message.len() {
        return Err(invalid("message ends early"));
    }
    Ok(header)
}

/// Builds messages in little-endian byte order.
pub struct Builder {
    buf: Vec<u8>,
}

impl Builder {
    pub fn new(kind: u8, flags: u8, serial: u32) -> Self {
        let mut buf = vec![b'l', kind, flags, 1];
        buf.extend_from_slice(&[0; 4]);
        buf.extend_from_slice(&serial.to_le_bytes());
        buf.extend_from_slice(&[0; 4]);
        Self { buf }
    }

    fn pad(&mut self, to: usize) {
        let at = align(self.buf.len(), to);
        self.buf.resize(at, 0);
    }

    fn put_string(&mut self, value: &str) {
        self.pad(4);
        self.buf
            .extend_from_slice(&(value.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(value.as_bytes());
        self.buf.push(0);
    }

    fn put_signature(&mut self, value: &str) {
        self.buf.push(value.len() as u8);
        self.buf.extend_from_slice(value.as_bytes());
        self.buf.push(0);
    }

    /// Adds header field `code` with a value of type `s`, `o` or `g`.
    pub fn field(mut self, code: u8, kind: &str, value: &str) -> Self {
        self.pad(8);
        self.buf.push(code);
        self.put_signature(kind);
        if kind == "g" {
            self.put_signature(value);
        } else {
            self.put_string(value);
        }
        self
    }

    /// Adds header field `code` with a value of type `u`.
    pub fn field_u32(mut self, code: u8, value: u32) -> Self {
        self.pad(8);
        self.buf.push(code);
        self.put_signature("u");
        self.pad(4);
        self.buf.extend_from_slice(&value.to_le_bytes());
        self
    }

    /// Ends the header and adds a body of strings, with the signature field
    /// to match. Returns the whole message.
    pub fn strings(self, args: &[&str]) -> Vec<u8> {
        let mut body = Vec::new();
        for arg in args {
            body.resize(align(body.len(), 4), 0);
            body.extend_from_slice(&(arg.len() as u32).to_le_bytes());
            body.extend_from_slice(arg.as_bytes());
            body.push(0);
        }
        self.finish(&"s".repeat(args.len()), &body)
    }

    /// Ends the header and adds `body`, already marshalled for `signature`.
    /// Returns the whole message.
    pub fn finish(self, signature: &str, body: &[u8]) -> Vec<u8> {
        let mut message = if signature.is_empty() {
            self
        } else {
            self.field(8, "g", signature)
        };
        let fields = message.buf.len() - 16;
        message.buf[12..16].copy_from_slice(&(fields as u32).to_le_bytes());
        message.pad(8);
        message.buf[4..8].copy_from_slice(&(body.len() as u32).to_le_bytes());
        message.buf.extend_from_slice(body);
        message.buf
    }
}

/// Reads an authentication line up to `\r\n`, without it. Reads byte by
/// byte, so the first message after `BEGIN` stays in the stream.
pub fn read_line(stream: &mut impl Read) -> io::Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8];
    loop {
        stream.read_exact(&mut byte)?;
        line.push(byte[0]);
        if line.ends_with(b"\r\n") {
            line.truncate(line.len() - 2);
            return String::from_utf8(line).map_err(|_| invalid("auth line is not UTF-8"));
        }
        if line.len() > MAX_LINE {
            return Err(invalid("auth line too long"));
        }
    }
}

/// Authenticates to a bus as `uid` with `EXTERNAL`, without passing file
/// descriptors.
pub fn authenticate(stream: &mut (impl Read + Write), uid: u32) -> io::Result<()> {
    let hex: String = uid
        .to_string()
        .bytes()
        .map(|b| format!("{b:02x}"))
        .collect();
    stream.write_all(format!("\0AUTH EXTERNAL {hex}\r\n").as_bytes())?;
    let reply = read_line(stream)?;
    if !reply.starts_with("OK ") {
        return Err(invalid(&format!("the bus refused us: {reply}")));
    }
    stream.write_all(b"BEGIN\r\n")
}

/// Takes a client through authentication as the server, accepting
/// `EXTERNAL` and refusing file descriptor passing. Only sandboxes reach
/// the socket, so who connects is known already. Returns once the client
/// sent `BEGIN`.
pub fn accept(stream: &mut (impl Read + Write), guid: &str) -> io::Result<()> {
    let mut nul = [0u8];
    stream.read_exact(&mut nul)?;
    if nul[0] != 0 {
        return Err(invalid("no NUL byte before authentication"));
    }
    let mut ok = false;
    loop {
        let line = read_line(stream)?;
        let mut words = line.split(' ');
        let reply = match (words.next(), words.next(), words.next()) {
            (Some("AUTH"), Some("EXTERNAL"), Some(_)) => {
                ok = true;
                format!("OK {guid}\r\n")
            }
            (Some("AUTH"), Some("EXTERNAL"), None) => "DATA\r\n".to_owned(),
            (Some("DATA"), ..) => {
                ok = true;
                format!("OK {guid}\r\n")
            }
            (Some("BEGIN"), None, None) if ok => return Ok(()),
            (Some("NEGOTIATE_UNIX_FD"), ..) if ok => {
                "ERROR file descriptors are not passed\r\n".to_owned()
            }
            (Some("AUTH" | "CANCEL" | "ERROR"), ..) => {
                ok = false;
                "REJECTED EXTERNAL\r\n".to_owned()
            }
            _ => "ERROR\r\n".to_owned(),
        };
        stream.write_all(reply.as_bytes())?;
    }
}

/// A message body in little-endian D-Bus encoding, for tests.
#[cfg(test)]
#[derive(Default)]
pub struct Body(pub Vec<u8>);

#[cfg(test)]
impl Body {
    pub fn pad(&mut self, to: usize) {
        self.0.resize(align(self.0.len(), to), 0);
    }

    pub fn u32(&mut self, value: u32) {
        self.pad(4);
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    pub fn string(&mut self, value: &str) {
        self.u32(value.len() as u32);
        self.0.extend_from_slice(value.as_bytes());
        self.0.push(0);
    }

    pub fn signature(&mut self, value: &str) {
        self.0.push(value.len() as u8);
        self.0.extend_from_slice(value.as_bytes());
        self.0.push(0);
    }

    /// An array of `count` dictionary entries, each written by `entry`.
    pub fn dict(&mut self, count: usize, mut entry: impl FnMut(&mut Self, usize)) {
        self.u32(0);
        let len_at = self.0.len() - 4;
        self.pad(8);
        let start = self.0.len();
        for index in 0..count {
            self.pad(8);
            entry(self, index);
        }
        let len = (self.0.len() - start) as u32;
        self.0[len_at..len_at + 4].copy_from_slice(&len.to_le_bytes());
    }

    pub fn string_map(&mut self, pairs: &[(&str, &str)]) {
        self.dict(pairs.len(), |body, index| {
            body.string(pairs[index].0);
            body.string(pairs[index].1);
        });
    }

    /// `CreateItem`'s properties (`a{sv}`) with `attributes` and a
    /// label, the label first when `label_first`.
    pub fn new_item(&mut self, attributes: &[(&str, &str)], label_first: bool) {
        self.dict(2, |body, index| {
            if (index == 0) == label_first {
                body.string("org.freedesktop.Secret.Item.Label");
                body.signature("s");
                body.string("token");
            } else {
                body.string(ITEM_ATTRIBUTES);
                body.signature("a{ss}");
                body.string_map(attributes);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_secrets_name_their_attributes() {
        let login = [("service", "gemini"), ("username", "antigravity")];
        let read = |body: Body| {
            let message = Builder::new(METHOD_CALL, 0, 1)
                .field(1, "o", "/org/freedesktop/secrets/collection/login")
                .field(3, "s", "CreateItem")
                .finish("a{sv}", &body.0);
            let header = parse(&message).unwrap();
            let mut cursor = Cursor::new(&message, header.body);
            (cursor.item_attributes().unwrap()).map(|pairs| {
                (pairs.into_iter())
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect::<Vec<_>>()
            })
        };
        let owned = |pairs: &[(&str, &str)]| {
            (pairs.iter())
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect::<Vec<_>>()
        };
        for label_first in [true, false] {
            let mut body = Body::default();
            body.new_item(&login, label_first);
            assert_eq!(
                read(body),
                Some(owned(&login)),
                "label first: {label_first}"
            );
        }
        let mut body = Body::default();
        body.new_item(&[], true);
        assert_eq!(read(body), Some(Vec::new()));

        // Named twice, the service might keep either.
        let mut body = Body::default();
        body.dict(2, |body, index| {
            body.string(ITEM_ATTRIBUTES);
            body.signature("a{ss}");
            body.string_map(if index == 0 {
                &login
            } else {
                &[("service", "x")]
            });
        });
        assert_eq!(read(body), None);
        // A property of a type the cursor cannot step over.
        let mut body = Body::default();
        body.dict(2, |body, index| {
            if index == 0 {
                body.string("org.example.Size");
                body.signature("t");
                body.pad(8);
                body.0.extend_from_slice(&7u64.to_le_bytes());
            } else {
                body.string(ITEM_ATTRIBUTES);
                body.signature("a{ss}");
                body.string_map(&login);
            }
        });
        assert_eq!(read(body), None);

        // A property reply holding attributes.
        let mut body = Body::default();
        body.signature("a{ss}");
        body.string_map(&login);
        let message = Builder::new(METHOD_RETURN, 0, 2)
            .field_u32(5, 1)
            .finish("v", &body.0);
        let header = parse(&message).unwrap();
        let pairs = Cursor::new(&message, header.body)
            .string_map_variant()
            .unwrap();
        assert_eq!(pairs, login);
    }

    #[test]
    fn built_messages_parse_back() {
        let message = Builder::new(METHOD_CALL, 0, 7)
            .field(1, "o", "/org/freedesktop/DBus")
            .field(2, "s", "org.freedesktop.DBus")
            .field(3, "s", "GetNameOwner")
            .field(6, "s", "org.freedesktop.DBus")
            .strings(&["org.freedesktop.secrets"]);
        let mut read = Vec::new();
        read_message(&mut message.as_slice(), &mut read).unwrap();
        assert_eq!(read, message);
        let header = parse(&message).unwrap();
        assert_eq!(header.kind, METHOD_CALL);
        assert_eq!(header.serial, 7);
        assert_eq!(header.path, Some("/org/freedesktop/DBus"));
        assert_eq!(header.member, Some("GetNameOwner"));
        assert_eq!(header.destination, Some("org.freedesktop.DBus"));
        assert_eq!(header.signature, "s");
        let mut body = Cursor::new(&message, header.body);
        assert_eq!(body.string().unwrap(), "org.freedesktop.secrets");
    }

    #[test]
    fn errors_carry_their_reply_serial() {
        let message = Builder::new(ERROR, 0, 9)
            .field(4, "s", "org.freedesktop.DBus.Error.AccessDenied")
            .field_u32(5, 41)
            .strings(&["no"]);
        let header = parse(&message).unwrap();
        assert_eq!(header.kind, ERROR);
        assert_eq!(header.reply_serial, Some(41));
    }

    #[test]
    fn object_path_arrays_are_read() {
        // GetSecrets(ao items, o session) as a client would send it.
        let items = ["/org/freedesktop/secrets/collection/login/1", "/x/2"];
        let mut array = Vec::new();
        for item in items {
            array.resize(align(array.len(), 4), 0);
            array.extend_from_slice(&(item.len() as u32).to_le_bytes());
            array.extend_from_slice(item.as_bytes());
            array.push(0);
        }
        let mut body = (array.len() as u32).to_le_bytes().to_vec();
        body.extend_from_slice(&array);
        let message = Builder::new(METHOD_CALL, 0, 1).finish("ao", &body);
        let header = parse(&message).unwrap();
        assert_eq!(header.signature, "ao");
        let mut cursor = Cursor::new(&message, header.body);
        assert_eq!(cursor.strings().unwrap(), items);
    }

    #[test]
    fn clients_authenticate_with_external_and_get_no_fd_passing() {
        struct Duplex {
            input: io::Cursor<Vec<u8>>,
            output: Vec<u8>,
        }
        impl Read for Duplex {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.input.read(buf)
            }
        }
        impl Write for Duplex {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.output.write(buf)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let script = b"\0AUTH\r\nAUTH EXTERNAL 31303030\r\nNEGOTIATE_UNIX_FD\r\nBEGIN\r\nl";
        let mut duplex = Duplex {
            input: io::Cursor::new(script.to_vec()),
            output: Vec::new(),
        };
        accept(&mut duplex, "00ff").unwrap();
        assert_eq!(
            String::from_utf8(duplex.output).unwrap(),
            "REJECTED EXTERNAL\r\nOK 00ff\r\nERROR file descriptors are not passed\r\n"
        );
        // The first message byte stays unread.
        assert_eq!(duplex.input.position() as usize, script.len() - 1);
    }
}
