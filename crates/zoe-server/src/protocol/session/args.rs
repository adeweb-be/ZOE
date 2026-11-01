//! The readers of the arguments of a call. A bad argument fails the call
//! with the exception that ZEO gives for it, mostly `TypeError`.

use std::time::Duration;

use rmpv::Value;
use zoe_core::{Flavour, Tid};

use super::Fault;
use crate::protocol::message::Call;

/// The arguments of one call, with the name of its method for the text of
/// an error.
#[derive(Clone, Copy)]
pub(super) struct Args<'a> {
    pub(super) method: &'a str,
    pub(super) values: &'a [Value],
}

impl<'a> Args<'a> {
    /// The arguments of `call`.
    pub(super) fn of(call: &'a Call) -> Self {
        Self {
            method: &call.method,
            values: &call.args,
        }
    }

    /// Return the `N` arguments. Another number of arguments fails with
    /// `TypeError`.
    pub(super) fn exactly<const N: usize>(self) -> Result<&'a [Value; N], Fault> {
        self.values
            .try_into()
            .map_err(|_| self.takes(&format!("{N} arguments")))
    }

    /// Return the `TypeError` of a call with a wrong number of arguments:
    /// the method takes `count`, such as `1 or 2 arguments`.
    pub(super) fn takes(self, count: &str) -> Fault {
        Fault::type_error(format!("{} takes {count}", self.method))
    }
}

/// The arguments of one call that the call changes: `storea` and
/// `restorea` take the pickle out of them.
pub(super) struct ArgsMut<'a> {
    pub(super) method: &'a str,
    pub(super) values: &'a mut [Value],
}

impl<'a> ArgsMut<'a> {
    /// Return the `N` arguments. Another number of arguments fails with
    /// `TypeError`.
    pub(super) fn exactly<const N: usize>(self) -> Result<&'a mut [Value; N], Fault> {
        let method = self.method;
        self.values
            .try_into()
            .map_err(|_| Fault::type_error(format!("{method} takes {N} arguments")))
    }
}

/// Return the arguments of `register`: the storage name, the read-only
/// flag and the declaration of the extensions. A stock client gives no
/// declaration.
pub(super) fn register(args: Args<'_>) -> Result<(&Value, &Value, Option<&Value>), Fault> {
    match args.values {
        [storage, read_only] => Ok((storage, read_only, None)),
        [storage, read_only, declaration] => Ok((storage, read_only, Some(declaration))),
        _ => Err(args.takes("2 or 3 arguments")),
    }
}

/// Return the storage name that a client gives to `register`.
pub(super) fn storage_name(value: &Value) -> String {
    match value {
        Value::String(text) => text.as_str().unwrap_or_default().to_owned(),
        Value::Binary(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        other => other.to_string(),
    }
}

/// The flavour of a namespace: the str `"ephemeral"` or `"durable"`.
pub(super) fn flavour_of(value: &Value) -> Result<Flavour, Fault> {
    let name = value
        .as_str()
        .ok_or_else(|| Fault::type_error("the flavour of a namespace is not a str"))?;
    Flavour::from_name(name).ok_or_else(|| {
        Fault::value_error("the flavour of a namespace is not \"ephemeral\" or \"durable\"")
    })
}

/// A name: a str. `what` names it.
pub(super) fn str_of<'a>(value: &'a Value, what: &str) -> Result<&'a str, Fault> {
    value
        .as_str()
        .ok_or_else(|| Fault::type_error(format!("the {what} is not a str")))
}

/// A key, a bound or a value of a namespace: bin. `what` names it.
pub(super) fn bin_of<'a>(value: &'a Value, what: &str) -> Result<&'a [u8], Fault> {
    match value {
        Value::Binary(bytes) => Ok(bytes),
        _ => Err(Fault::type_error(format!("the {what} is not bytes"))),
    }
}

/// A bound of a scan: bin, or nil for an open end.
pub(super) fn optional_bin<'a>(value: &'a Value, what: &str) -> Result<Option<&'a [u8]>, Fault> {
    match value {
        Value::Nil => Ok(None),
        value => bin_of(value, what).map(Some),
    }
}

/// The TTL of a write: an int or a float of seconds, or nil for none. A
/// negative, not finite or too long TTL fails with `ValueError`. The core
/// checks its bounds.
pub(super) fn ttl_of(value: &Value) -> Result<Option<Duration>, Fault> {
    let refused = || Fault::value_error("a TTL is not a number of seconds from 0 to 2^32 - 1");
    match value {
        Value::Nil => Ok(None),
        Value::Integer(n) => match n.as_u64() {
            Some(seconds) => Ok(Some(Duration::from_secs(seconds))),
            None => Err(refused()),
        },
        Value::F32(seconds) => Duration::try_from_secs_f32(*seconds)
            .map(Some)
            .map_err(|_| refused()),
        Value::F64(seconds) => Duration::try_from_secs_f64(*seconds)
            .map(Some)
            .map_err(|_| refused()),
        _ => Err(Fault::type_error("a TTL is not an int, a float or None")),
    }
}

/// A tid, or nil for none.
pub(super) fn optional_tid(value: &Value) -> Result<Option<Tid>, Fault> {
    match value {
        Value::Nil => Ok(None),
        tid => Ok(Some(Tid(u64_of(tid)?))),
    }
}

/// The serial of a store, `storea`, `storeBlobEnd` or `storeBlobShared`.
/// A nil serial is the zero tid, the serial of a new object, as in
/// FileStorage. Another serial reads as `serial_of` reads it.
pub(super) fn store_serial(value: &Value) -> Result<Tid, Fault> {
    match value {
        Value::Nil => Ok(Tid::ZERO),
        serial => serial_of(serial),
    }
}

/// The serial argument of a call of the transaction: 8 bytes, or a str
/// whose UTF-8 bytes are 8 bytes. ZEO's tests send the serial of a new
/// object as the str `'\0' * 8`, and ZEO's server takes it. A str of
/// another length fails as a str oid or tid does.
pub(super) fn serial_of(value: &Value) -> Result<Tid, Fault> {
    match value {
        Value::String(text) => text
            .as_str()
            .and_then(|text| <[u8; 8]>::try_from(text.as_bytes()).ok())
            .map(|bytes| Tid(u64::from_be_bytes(bytes)))
            .ok_or_else(|| Fault::type_error("an oid or a tid is not bytes")),
        serial => Ok(Tid(u64_of(serial)?)),
    }
}

/// The number in the 8 big-endian bytes of `value`, a bin. A str is not
/// an oid or a tid here: only `serial_of` takes one.
pub(super) fn u64_of(value: &Value) -> Result<u64, Fault> {
    match value {
        Value::Binary(_) => zoe_wire::u64_of(value)
            .ok_or_else(|| Fault::type_error("an oid or a tid is not 8 bytes")),
        _ => Err(Fault::type_error("an oid or a tid is not bytes")),
    }
}

/// The bytes of a user name or a description: bytes, or a text in UTF-8.
pub(super) fn text_bytes(value: &Value) -> Result<Vec<u8>, Fault> {
    match value {
        Value::Binary(bytes) => Ok(bytes.clone()),
        Value::String(text) => Ok(text.as_bytes().to_vec()),
        Value::Nil => Ok(Vec::new()),
        _ => Err(Fault::type_error("a user or a description is not bytes")),
    }
}

/// The truth of a msgpack value, as Python gives it.
pub(super) fn truth(value: &Value) -> bool {
    match value {
        Value::Nil => false,
        Value::Boolean(flag) => *flag,
        Value::Integer(n) => n.as_i64() != Some(0),
        _ => true,
    }
}
