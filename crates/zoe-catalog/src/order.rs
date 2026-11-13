//! The order clause of a plan (ADR 0059, the amendment of ZOE-302): a
//! sort on the `GopipIndex` of plone.folder for the rows of one container.
//!
//! `GopipIndex` stores nothing. For a result whose rows have one parent,
//! its `documentToKeyMap` gives each row the place of its id in
//! `idsInOrder()` of the container. With `DefaultOrdering`, that list is
//! the order: the `PersistentList` that `_order()` gives, one record. The
//! client finds the container and puts the oid of its order in the plan.
//!
//! At each query with an order clause, the mirror reads that record as of
//! the tid of the plan, or takes it from the transaction overlay of the
//! query, and decodes the ids. It keeps nothing of the order across
//! queries. The query then gives each row its place (`View::ordered`).

use crate::read::{Reads, Record};
use crate::reason::{Fallback, Reason};

/// The class of the index that an order clause sorts on.
pub const GOPIP: (&str, &str) = ("plone.folder.nogopip", "GopipIndex");

/// The most bytes of a record of an order that the mirror decodes: the cap
/// of a call.
pub const MAX_RECORD: usize = 16 << 20;

/// The order clause of a plan: `{"i": index, "oid": oid, "p": path}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Order {
    /// The id of the GopipIndex, the only index of the sort.
    pub index: String,
    /// The oid of the order of the container.
    pub oid: u64,
    /// The path of the container, as the parent of a path of `paths`.
    pub path: String,
}

/// The ids of an order, as of the tid of a query, for that query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrderList {
    /// The id of the GopipIndex.
    pub index: String,
    /// The path of the container.
    pub path: String,
    /// The items of the order, in order: `None` for an item that is not a
    /// str. Such an item keeps its place and names no row.
    pub ids: Vec<Option<Box<str>>>,
}

/// Decodes the record of an order: a `persistent.list.PersistentList`
/// whose state holds the list `data`.
pub fn decode(bytes: &[u8]) -> Result<Vec<Option<Box<str>>>, String> {
    if bytes.len() > MAX_RECORD {
        return Err(format!("an order of {} bytes", bytes.len()));
    }
    let record = Record::decode(bytes).map_err(|e| e.0)?;
    if !record.is("persistent.list", "PersistentList") {
        return Err(format!(
            "an order of the class {}.{}",
            record.module, record.name
        ));
    }
    let data = record
        .attr("data")
        .ok_or("an order without data in its state")?;
    let Some(zoe_pickle::python::Object::List(items)) = record.heap.object(data) else {
        return Err("the data of an order is not a list".into());
    };
    Ok(items
        .iter()
        .map(|item| record.heap.str(*item).map(Into::into))
        .collect())
}

/// Reads the order of `order` for a query before `tid`: the record of its
/// oid in `overlay` when the overlay holds one, else the record that was
/// current before `tid`. A record of the overlay that does not decode
/// gives `overlay`. A record that does not load before `tid`, or that does
/// not decode, gives `index_unavailable`.
pub fn read(
    reads: &dyn Reads,
    order: &Order,
    tid: u64,
    overlay: Option<&[(u64, Vec<u8>)]>,
) -> Result<OrderList, Fallback> {
    let found = overlay.and_then(|records| {
        records
            .iter()
            .rev()
            .find(|(oid, _)| *oid == order.oid)
            .map(|(_, pickle)| pickle)
    });
    let ids = match found {
        Some(pickle) => decode(pickle).map_err(|why| {
            Fallback::new(Reason::Overlay, format!("the order in the overlay: {why}"))
        })?,
        None => {
            let unavailable =
                |why: String| Fallback::new(Reason::IndexUnavailable, format!("the order: {why}"));
            let bytes = reads
                .load_before(order.oid, tid)
                .map_err(unavailable)?
                .ok_or_else(|| {
                    unavailable(format!("no record of {:#x} before the tid", order.oid))
                })?;
            decode(&bytes).map_err(unavailable)?
        }
    };
    Ok(OrderList {
        index: order.index.clone(),
        path: order.path.clone(),
        ids,
    })
}
