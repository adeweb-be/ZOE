//! The guardrails of a store: the size limits of a store call.
//!
//! A size limit refuses a `store` or a `store_blob` before the transaction
//! stages it, so a refused call writes nothing.

use std::fmt;

use crate::error::{Result, ZoeError};
use crate::store::Options;
use crate::types::Oid;

/// A size limit of a store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SizeLimit {
    /// The length of one pickle: `max-record-size`.
    Record,
    /// The sum of the pickles of one transaction: `max-transaction-size`.
    Transaction,
    /// The length of one blob: `max-blob-size`.
    Blob,
}

impl SizeLimit {
    /// Return the configuration key of the limit, for example
    /// `max-record-size`.
    pub fn key(self) -> &'static str {
        match self {
            SizeLimit::Record => "max-record-size",
            SizeLimit::Transaction => "max-transaction-size",
            SizeLimit::Blob => "max-blob-size",
        }
    }

    /// Return the name of the thing that the limit measures.
    pub(crate) fn subject(self) -> &'static str {
        match self {
            SizeLimit::Record => "record",
            SizeLimit::Transaction => "transaction",
            SizeLimit::Blob => "blob",
        }
    }
}

/// The text of a limit is its configuration key.
impl fmt::Display for SizeLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.key())
    }
}

/// The guardrail options of a store. `None` turns a guardrail off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Guardrails {
    max_record_size: Option<u64>,
    max_transaction_size: Option<u64>,
    max_blob_size: Option<u64>,
}

impl Guardrails {
    pub(crate) fn new(options: &Options) -> Self {
        Self {
            max_record_size: options.max_record_size,
            max_transaction_size: options.max_transaction_size,
            max_blob_size: options.max_blob_size,
        }
    }

    /// Fail with `TooLarge` when the store of `oid` makes `size` bytes,
    /// over `limit`. A size equal to the limit passes.
    pub(crate) fn check(&self, limit: SizeLimit, oid: Oid, size: u64) -> Result<()> {
        let max = match limit {
            SizeLimit::Record => self.max_record_size,
            SizeLimit::Transaction => self.max_transaction_size,
            SizeLimit::Blob => self.max_blob_size,
        };
        match max {
            Some(max) if size > max => Err(ZoeError::TooLarge {
                oid,
                limit,
                size,
                max,
            }),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: [SizeLimit; 3] = [SizeLimit::Record, SizeLimit::Transaction, SizeLimit::Blob];

    fn guardrails(options: Options) -> Guardrails {
        Guardrails::new(&options)
    }

    #[test]
    fn a_size_equal_to_a_limit_passes_and_one_byte_more_fails() {
        let limits = guardrails(Options {
            max_record_size: Some(10),
            max_transaction_size: Some(10),
            max_blob_size: Some(10),
            ..Options::default()
        });
        for limit in LIMITS {
            limits.check(limit, Oid(7), 10).unwrap();
            let error = limits.check(limit, Oid(7), 11).unwrap_err();
            assert!(
                matches!(
                    error,
                    ZoeError::TooLarge { oid: Oid(7), limit: refused, size: 11, max: 10 }
                        if refused == limit
                ),
                "{error:?}"
            );
            let message = error.to_string();
            assert!(message.contains(limit.key()), "{message}");
            assert!(message.contains(limit.subject()), "{message}");
        }
    }

    #[test]
    fn no_limit_is_set_by_default() {
        let limits = guardrails(Options::default());
        for limit in LIMITS {
            limits.check(limit, Oid(1), u64::MAX).unwrap();
        }
    }

    #[test]
    fn each_limit_shows_its_configuration_key() {
        let keys: Vec<String> = LIMITS.iter().map(ToString::to_string).collect();
        assert_eq!(
            keys,
            ["max-record-size", "max-transaction-size", "max-blob-size"]
        );
    }
}
