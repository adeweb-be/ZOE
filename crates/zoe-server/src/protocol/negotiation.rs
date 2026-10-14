//! The negotiation of the ZOE extensions (F13).
//!
//! A ZOE client declares the extensions that it wants in the third
//! argument of `register`: a list of names. The server accepts the names
//! that it serves, once each, in the order of the declaration. It leaves
//! out the other names, and that is not an error. `get_info` gives the
//! accepted names back under the key `extensions`.
//!
//! A stock client calls `register` with two arguments. It declares
//! nothing, and its `get_info` has no `extensions` key, so it never sees an
//! extension. A declaration holds for its connection only. A call to a
//! method of an extension that the connection did not accept fails, and
//! the connection stays open.

use rmpv::Value;

/// An extension that the server serves, and the methods that it adds.
pub(crate) struct Extension {
    pub(crate) name: &'static str,
    pub(crate) methods: &'static [&'static str],
}

/// The extensions that the server serves. It serves none yet, so each
/// declaration gives an empty list back.
pub(crate) const SERVED: &[Extension] = &[];

/// Return the names of the declaration `value`, the third argument of
/// `register`. A value that is not a list of strings is an error.
pub(crate) fn declared(value: &Value) -> Result<Vec<&str>, String> {
    let Value::Array(items) = value else {
        return Err("the extensions of register are not a list".into());
    };
    items
        .iter()
        .map(|item| {
            item.as_str()
                .ok_or_else(|| format!("the extension {item} is not a str"))
        })
        .collect()
}

/// Return the names of `declared` that `served` holds, once each, in the
/// order of the declaration.
pub(crate) fn accept(declared: &[&str], served: &[Extension]) -> Vec<&'static str> {
    let mut accepted = Vec::new();
    for name in declared {
        if let Some(extension) = served.iter().find(|e| e.name == *name)
            && !accepted.contains(&extension.name)
        {
            accepted.push(extension.name);
        }
    }
    accepted
}

/// Return the error text of a call to `method` when `method` belongs to an
/// extension of `served` that is not in `accepted`. A connection that
/// declared nothing has `accepted` none.
pub(crate) fn refused(
    method: &str,
    accepted: Option<&[&'static str]>,
    served: &[Extension],
) -> Option<String> {
    let extension = served.iter().find(|e| e.methods.contains(&method))?;
    let accepted = accepted.unwrap_or_default();
    (!accepted.contains(&extension.name)).then(|| {
        format!(
            "{method} needs the extension {}, and the connection did not declare it in register",
            extension.name
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SERVED: &[Extension] = &[
        Extension {
            name: "first",
            methods: &["first_call"],
        },
        Extension {
            name: "second",
            methods: &["second_call", "second_other_call"],
        },
    ];

    fn names(items: &[&str]) -> Value {
        Value::Array(items.iter().map(|name| Value::from(*name)).collect())
    }

    #[test]
    fn a_declaration_is_a_list_of_strings() {
        assert_eq!(declared(&names(&[])).unwrap(), Vec::<&str>::new());
        assert_eq!(declared(&names(&["a", "b"])).unwrap(), ["a", "b"]);
        for bad in [
            Value::from("a"),
            Value::Nil,
            Value::Map(vec![]),
            Value::Array(vec![Value::from(7)]),
            Value::Array(vec![Value::from("a"), Value::Binary(b"b".to_vec())]),
        ] {
            assert!(declared(&bad).is_err(), "{bad:?} is a declaration");
        }
    }

    #[test]
    fn accept_keeps_the_served_names_once_in_the_order_of_the_declaration() {
        assert_eq!(
            accept(&["second", "unknown", "first", "second"], TEST_SERVED),
            ["second", "first"]
        );
        assert!(accept(&["unknown"], TEST_SERVED).is_empty());
        assert!(accept(&["first"], SERVED).is_empty());
    }

    #[test]
    fn a_method_of_an_extension_needs_its_declaration() {
        assert!(refused("first_call", None, TEST_SERVED).is_some());
        assert!(refused("first_call", Some(&[]), TEST_SERVED).is_some());
        assert!(refused("first_call", Some(&["second"]), TEST_SERVED).is_some());
        assert!(refused("first_call", Some(&["first"]), TEST_SERVED).is_none());
        assert!(refused("second_other_call", Some(&["second"]), TEST_SERVED).is_none());
    }

    #[test]
    fn a_method_of_no_extension_is_never_refused() {
        assert!(refused("loadBefore", None, TEST_SERVED).is_none());
        assert!(refused("first_call", None, SERVED).is_none());
    }
}
