//! The reasons of a fallback: why a query takes the Python path (ADR 0059,
//! section 7). The table is a contract: the names are the names of the
//! reply and of the counts.

/// Why a query takes the Python path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Reason {
    /// The storage does not serve `catalog_query`.
    Unsupported,
    /// The store has no registration key for the catalog.
    NotRegistered,
    /// The build refused the catalog.
    Refused,
    /// An index of the query is of a class that the mirror does not serve.
    Index,
    /// An option that the plan does not carry.
    Option,
    /// A range with the operator `and`.
    AndRange,
    /// A sort that the mirror does not serve.
    Sort,
    /// The transaction changed records, and no overlay carries them.
    Transaction,
    /// The plan and the overlay pass the cap of a call.
    TooLarge,
    /// The plan has a key or a kind of clause that the mirror does not know.
    Plan,
    /// A row of the result has no entry in a DateRangeIndex of the query.
    DateRangeOrder,
    /// The decoder refused a record of an index of the query.
    IndexUnavailable,
    /// A record of the overlay does not read.
    Overlay,
    /// The mirror is building.
    NotReady,
    /// The memory bound dropped the mirror.
    Dropped,
    /// The mirror met a commit that it cannot follow.
    Stale,
    /// A commit after the tid of the query has no change set in the mirror.
    OutsideWindow,
    /// The mirror did not reach the tid of the query in time.
    Lag,
}

impl Reason {
    /// Every reason, in the order of the table.
    pub const ALL: [Reason; 18] = [
        Reason::Unsupported,
        Reason::NotRegistered,
        Reason::Refused,
        Reason::Index,
        Reason::Option,
        Reason::AndRange,
        Reason::Sort,
        Reason::Transaction,
        Reason::TooLarge,
        Reason::Plan,
        Reason::DateRangeOrder,
        Reason::IndexUnavailable,
        Reason::Overlay,
        Reason::NotReady,
        Reason::Dropped,
        Reason::Stale,
        Reason::OutsideWindow,
        Reason::Lag,
    ];

    /// The name of the reason.
    pub fn name(self) -> &'static str {
        match self {
            Reason::Unsupported => "unsupported",
            Reason::NotRegistered => "not_registered",
            Reason::Refused => "refused",
            Reason::Index => "index",
            Reason::Option => "option",
            Reason::AndRange => "and_range",
            Reason::Sort => "sort",
            Reason::Transaction => "transaction",
            Reason::TooLarge => "too_large",
            Reason::Plan => "plan",
            Reason::DateRangeOrder => "date_range_order",
            Reason::IndexUnavailable => "index_unavailable",
            Reason::Overlay => "overlay",
            Reason::NotReady => "not_ready",
            Reason::Dropped => "dropped",
            Reason::Stale => "stale",
            Reason::OutsideWindow => "outside_window",
            Reason::Lag => "lag",
        }
    }

    /// The reason of `name`.
    pub fn from_name(name: &str) -> Option<Reason> {
        Reason::ALL.into_iter().find(|r| r.name() == name)
    }
}

/// A query that takes the Python path: the reason and a text that says
/// why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fallback {
    /// The reason.
    pub reason: Reason,
    /// The text.
    pub why: String,
}

impl Fallback {
    /// A fallback for `reason`, with `why`.
    pub fn new(reason: Reason, why: impl Into<String>) -> Fallback {
        Fallback {
            reason,
            why: why.into(),
        }
    }
}
