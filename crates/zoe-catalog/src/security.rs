//! The security index of `zoe.plone` (ADR 0061): `allowedRolesAndUsers`
//! resolved from the inherited inputs of each entry.
//!
//! A `ZoeSecurityIndex` keeps, in its tree `_inputs`, the inputs that the
//! object of each entry holds itself: the setting of the permission
//! "Access contents information", its own local roles and its block flag.
//! With them it keeps the chain: the inputs of the containers outside the
//! catalog above the object, up to the first entry, nearest first. An
//! opaque entry keeps the value of the indexer instead.
//!
//! The value of an entry follows from its inputs and from the inputs of
//! its containers, as the indexer of CMFPlone computes it. The resolver
//! finds the entry of a container by the path of its row. An entry of the
//! catalog at a path of the chain takes the place of the item of the
//! chain. An entry gives no token when a container on its path is an
//! opaque entry, or is neither an entry nor an item of its chain.
//!
//! A base keeps the inputs of each row, the value of each row as one of
//! the distinct values, and the rows of each distinct value. A version
//! resolves again the rows whose inputs or path changed and the rows below
//! them, and puts them over the rows of the base.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use roaring::RoaringBitmap;

use crate::base::{NONE, Rows};
use crate::value::Val;

/// The class of the security index of `zoe.plone`.
pub const SECURITY_INDEX: (&str, &str) = ("zoe.plone.security", "ZoeSecurityIndex");

/// The kind of the permission setting of an object, as
/// `rolesForPermissionOn` reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Setting {
    /// No setting: the roles of the container.
    Missing,
    /// A tuple: these roles only.
    Tuple,
    /// Another sequence: these roles and the roles of the container.
    List,
    /// The setting `None`: Anonymous.
    Anonymous,
    /// An opaque entry: the roles are the value of the indexer.
    Opaque,
}

impl Setting {
    fn of(code: &str) -> Option<Setting> {
        Some(match code {
            "" => Setting::Missing,
            "t" => Setting::Tuple,
            "l" => Setting::List,
            "a" => Setting::Anonymous,
            "v" => Setting::Opaque,
            _ => return None,
        })
    }

    fn code(self) -> &'static str {
        match self {
            Setting::Missing => "",
            Setting::Tuple => "t",
            Setting::List => "l",
            Setting::Anonymous => "a",
            Setting::Opaque => "v",
        }
    }
}

/// The inputs of one object, with names as strs.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RawNode {
    setting: Setting,
    roles: Vec<Box<str>>,
    local: Vec<(Box<str>, Vec<Box<str>>)>,
    block: bool,
}

/// The inputs of an entry as `_inputs` holds them, with names as strs:
/// the inputs of the object, and the chain of its containers outside the
/// catalog, nearest first, each with its path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Raw {
    own: RawNode,
    chain: Vec<(Box<str>, RawNode)>,
}

fn items(value: &Val) -> Result<&[Val], String> {
    match value {
        Val::Tuple(items) | Val::List(items) => Ok(items),
        other => Err(format!("an input of the security index: {other:?}")),
    }
}

fn text(value: &Val) -> Result<Box<str>, String> {
    match value {
        Val::Str(s) => Ok(s.clone()),
        other => Err(format!("a name of the security index: {other:?}")),
    }
}

fn names_of(value: &Val) -> Result<Vec<Box<str>>, String> {
    let mut out = items(value)?
        .iter()
        .map(text)
        .collect::<Result<Vec<_>, _>>()?;
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

fn truth(value: &Val) -> Result<bool, String> {
    match value {
        Val::Bool(b) => Ok(*b),
        Val::Int(i) => Ok(*i != 0),
        Val::None => Ok(false),
        other => Err(format!("a block flag of the security index: {other:?}")),
    }
}

impl RawNode {
    /// Reads `(kind, roles, local, block)` from `items`.
    fn parse(items_of: &[Val]) -> Result<RawNode, String> {
        let [kind, roles, local, block] = items_of else {
            return Err("inputs of the security index of another length".into());
        };
        let setting = Setting::of(&text(kind)?)
            .ok_or_else(|| format!("a kind of setting of the security index: {kind:?}"))?;
        let mut merged: BTreeMap<Box<str>, Vec<Box<str>>> = BTreeMap::new();
        for pair in items(local)? {
            let [principal, granted] = items(pair)? else {
                return Err("a local role pair of another length".into());
            };
            merged
                .entry(text(principal)?)
                .or_default()
                .extend(names_of(granted)?);
        }
        let local = merged
            .into_iter()
            .filter_map(|(principal, mut granted)| {
                granted.sort_unstable();
                granted.dedup();
                (!granted.is_empty()).then_some((principal, granted))
            })
            .collect();
        Ok(RawNode {
            setting,
            roles: names_of(roles)?,
            local,
            block: truth(block)?,
        })
    }

    fn to_vals(&self) -> Vec<Val> {
        let strs = |names: &[Box<str>]| -> Val {
            Val::Tuple(names.iter().map(|n| Val::Str(n.clone())).collect())
        };
        vec![
            Val::Str(self.setting.code().into()),
            strs(&self.roles),
            Val::Tuple(
                self.local
                    .iter()
                    .map(|(p, r)| Val::Tuple(vec![Val::Str(p.clone()), strs(r)].into()))
                    .collect(),
            ),
            Val::Bool(self.block),
        ]
    }

    fn intern(&self, names: &mut dyn Intern, sets: &mut Sets) -> Node {
        let roles = sets.ids(self.roles.iter().map(|r| names.id(r)).collect());
        let local: Vec<(u32, Ids)> = self
            .local
            .iter()
            .map(|(p, r)| {
                let ids = sets.ids(r.iter().map(|x| names.id(x)).collect());
                (names.id(p), ids)
            })
            .collect();
        Node {
            setting: self.setting,
            roles,
            local: local.into(),
            block: self.block,
        }
    }
}

impl Raw {
    /// Reads the value of an entry of `_inputs`: `(kind, roles, local,
    /// block, chain)`, each item of the chain `(path, kind, roles, local,
    /// block)`. Names and roles are strs, the local roles pairs of a
    /// principal and its roles.
    pub fn parse(value: &Val) -> Result<Raw, String> {
        let all = items(value)?;
        let [head @ .., chain] = all else {
            return Err("inputs of the security index without a chain".into());
        };
        let own = RawNode::parse(head)?;
        let mut links = Vec::new();
        for item in items(chain)? {
            let [path, rest @ ..] = items(item)? else {
                return Err("an item of a chain without a path".into());
            };
            links.push((text(path)?, RawNode::parse(rest)?));
        }
        Ok(Raw { own, chain: links })
    }

    /// The value in the form that the change sets keep: each list sorted,
    /// the local roles merged by principal, without a principal with no
    /// role.
    pub fn to_val(&self) -> Val {
        let mut out = self.own.to_vals();
        out.push(Val::Tuple(
            self.chain
                .iter()
                .map(|(path, node)| {
                    let mut item = vec![Val::Str(path.clone())];
                    item.extend(node.to_vals());
                    Val::Tuple(item.into())
                })
                .collect(),
        ));
        Val::Tuple(out.into())
    }

    fn intern(&self, names: &mut dyn Intern, sets: &mut Sets) -> Inputs {
        Inputs {
            own: self.own.intern(names, sets),
            chain: self
                .chain
                .iter()
                .map(|(path, node)| (path.clone(), node.intern(names, sets)))
                .collect(),
        }
    }
}

/// The value of an item of `_inputs` in the form that the change sets
/// keep. Refuses a value that the index does not model.
pub fn normalize(value: &Val) -> Result<Val, String> {
    Raw::parse(value).map(|raw| raw.to_val())
}

/// Sorted ids of names.
type Ids = Arc<[u32]>;
/// Local roles: each principal with its roles, by principal.
type LocalRoles = Arc<[(u32, Ids)]>;

/// The inputs of one object, with names as ids.
#[derive(Clone, Debug)]
struct Node {
    setting: Setting,
    roles: Ids,
    local: LocalRoles,
    block: bool,
}

impl Node {
    fn to_raw(&self, names: &dyn Intern) -> RawNode {
        let mut roles: Vec<Box<str>> = self.roles.iter().map(|r| names.name(*r).into()).collect();
        roles.sort_unstable();
        let mut local: Vec<(Box<str>, Vec<Box<str>>)> = self
            .local
            .iter()
            .map(|(p, r)| {
                let mut held: Vec<Box<str>> = r.iter().map(|x| names.name(*x).into()).collect();
                held.sort_unstable();
                (names.name(*p).into(), held)
            })
            .collect();
        local.sort_unstable();
        RawNode {
            setting: self.setting,
            roles,
            local,
            block: self.block,
        }
    }
}

/// The inputs of an entry, with names as ids.
#[derive(Clone, Debug)]
pub struct Inputs {
    own: Node,
    chain: Box<[(Box<str>, Node)]>,
}

impl Inputs {
    fn to_val(&self, names: &dyn Intern) -> Val {
        Raw {
            own: self.own.to_raw(names),
            chain: self
                .chain
                .iter()
                .map(|(path, node)| (path.clone(), node.to_raw(names)))
                .collect(),
        }
        .to_val()
    }

    fn heap_bytes(&self) -> usize {
        let node = |n: &Node| 48 + n.roles.len() * 4 + n.local.len() * 24;
        64 + node(&self.own)
            + self
                .chain
                .iter()
                .map(|(p, n)| p.len() + 16 + node(n))
                .sum::<usize>()
    }
}

/// The ids of names.
pub trait Intern {
    /// The id of `name`; a new name gets a new id.
    fn id(&mut self, name: &str) -> u32;
    /// The name of `id`.
    fn name(&self, id: u32) -> &str;
}

/// The names of an index: roles, principals and tokens.
#[derive(Clone, Debug, Default)]
pub struct Names {
    names: Vec<Box<str>>,
    ids: HashMap<Box<str>, u32>,
}

impl Names {
    /// The id of `name`, if it has one.
    pub fn find(&self, name: &str) -> Option<u32> {
        self.ids.get(name).copied()
    }

    fn heap_bytes(&self) -> usize {
        self.names.iter().map(|n| n.len() * 2 + 48).sum()
    }
}

impl Intern for Names {
    fn id(&mut self, name: &str) -> u32 {
        if let Some(&id) = self.ids.get(name) {
            return id;
        }
        let id = self.names.len() as u32;
        self.names.push(name.into());
        self.ids.insert(name.into(), id);
        id
    }

    fn name(&self, id: u32) -> &str {
        &self.names[id as usize]
    }
}

/// The names of a base, with the names that a version adds after them.
pub struct Overlay<'a> {
    base: &'a Names,
    extra: Names,
}

impl<'a> Overlay<'a> {
    /// The names of `base`, with no name added.
    pub fn new(base: &'a Names) -> Overlay<'a> {
        Overlay {
            base,
            extra: Names::default(),
        }
    }

    /// The id of `name`, if it has one.
    pub fn find(&self, name: &str) -> Option<u32> {
        self.base.find(name).or_else(|| {
            self.extra
                .find(name)
                .map(|id| id + self.base.names.len() as u32)
        })
    }
}

impl Intern for Overlay<'_> {
    fn id(&mut self, name: &str) -> u32 {
        match self.base.find(name) {
            Some(id) => id,
            None => self.extra.id(name) + self.base.names.len() as u32,
        }
    }

    fn name(&self, id: u32) -> &str {
        let n = self.base.names.len() as u32;
        if id < n {
            self.base.name(id)
        } else {
            self.extra.name(id - n)
        }
    }
}

/// The distinct sorted sets of ids of one build, so that equal settings
/// share one allocation.
#[derive(Default)]
struct Sets {
    sets: HashMap<Vec<u32>, Ids>,
}

impl Sets {
    fn ids(&mut self, mut ids: Vec<u32>) -> Ids {
        ids.sort_unstable();
        ids.dedup();
        if let Some(found) = self.sets.get(&ids) {
            return Arc::clone(found);
        }
        let shared: Ids = ids.clone().into();
        self.sets.insert(ids, Arc::clone(&shared));
        shared
    }
}

/// The container of a path: the path without its last component. The
/// root has the path "" and no container.
fn parent(path: &str) -> Option<&str> {
    if path.is_empty() {
        return None;
    }
    Some(path.rsplit_once('/').map_or("", |(above, _)| above))
}

/// The roles of the permission at a node.
#[derive(Clone, Debug)]
enum Roles {
    /// No node up to the root sets them: `rolesForPermissionOn` then gives
    /// `Manager`.
    Unset,
    /// The setting `None` at the node or above it.
    Anonymous,
    /// These roles.
    Set(Ids),
}

/// What an entry gives the entries below it.
#[derive(Clone, Debug)]
enum State {
    /// The entry is opaque, has no inputs, or is below such a node: the
    /// entries below it give no token.
    Closed,
    /// The roles of the permission and the local roles at the entry.
    Open(Roles, LocalRoles),
}

fn union(a: &[u32], b: &[u32]) -> Ids {
    let mut all: Vec<u32> = a.iter().chain(b).copied().collect();
    all.sort_unstable();
    all.dedup();
    all.into()
}

/// The roles and the local roles below `node`, from those above it.
fn apply(node: &Node, roles: Roles, local: LocalRoles) -> (Roles, LocalRoles) {
    let roles = match node.setting {
        Setting::Tuple => Roles::Set(Arc::clone(&node.roles)),
        Setting::Anonymous => Roles::Anonymous,
        Setting::Missing | Setting::Opaque => roles,
        Setting::List => match roles {
            Roles::Anonymous => Roles::Anonymous,
            Roles::Unset if node.roles.is_empty() => Roles::Unset,
            Roles::Unset => Roles::Set(Arc::clone(&node.roles)),
            Roles::Set(above) if node.roles.is_empty() => Roles::Set(above),
            Roles::Set(above) => Roles::Set(union(&above, &node.roles)),
        },
    };
    let local = if node.block {
        Arc::clone(&node.local)
    } else if node.local.is_empty() {
        local
    } else {
        let mut merged: Vec<(u32, Ids)> = local.to_vec();
        for (principal, granted) in node.local.iter() {
            match merged.iter_mut().find(|(p, _)| p == principal) {
                Some((_, held)) => *held = union(held, granted),
                None => merged.push((*principal, Arc::clone(granted))),
            }
        }
        merged.into()
    };
    (roles, local)
}

/// The entries of one version of an index, by node: what the resolver
/// reads.
pub trait Tree {
    /// The node of the entry at `path`, if the catalog has one.
    fn find(&self, path: &str) -> Option<u32>;
    /// The path of the entry of `node`.
    fn path(&self, node: u32) -> &str;
    /// The inputs of `node`, `None` for an entry without inputs.
    fn inputs(&self, node: u32) -> Option<&Inputs>;
}

/// The resolution of the values of one tree, with a memo of the state of
/// each entry.
pub struct Resolver<'t, T: Tree> {
    tree: &'t T,
    memo: HashMap<u32, State>,
    tokens: HashMap<u32, u32>,
}

impl<'t, T: Tree> Resolver<'t, T> {
    /// A resolver of `tree`.
    pub fn new(tree: &'t T) -> Resolver<'t, T> {
        Resolver {
            tree,
            memo: HashMap::new(),
            tokens: HashMap::new(),
        }
    }

    /// The state of the entry at `path`, or `Closed` when the catalog has
    /// no entry with inputs there.
    fn state_at(&mut self, path: &str) -> State {
        let tree = self.tree;
        match tree.find(path) {
            Some(node) if tree.inputs(node).is_some() => self.state(node),
            _ => State::Closed,
        }
    }

    fn state(&mut self, node: u32) -> State {
        if let Some(found) = self.memo.get(&node) {
            return found.clone();
        }
        let found = self.compute(node);
        self.memo.insert(node, found.clone());
        found
    }

    fn compute(&mut self, node: u32) -> State {
        let tree = self.tree;
        let Some(inputs) = tree.inputs(node) else {
            return State::Closed;
        };
        if inputs.own.setting == Setting::Opaque {
            return State::Closed;
        }
        // The items of the chain that no entry of the catalog replaces.
        let mut at = parent(tree.path(node));
        let mut used: Vec<&Node> = Vec::new();
        for (path, item) in inputs.chain.iter() {
            let Some(here) = at else {
                break;
            };
            if **path != *here {
                break;
            }
            if tree.find(here).is_some_and(|n| tree.inputs(n).is_some()) {
                break;
            }
            if item.setting == Setting::Opaque {
                return State::Closed;
            }
            used.push(item);
            at = parent(here);
        }
        let above = match at {
            None => State::Open(Roles::Unset, Arc::from(Vec::new())),
            Some(path) => self.state_at(path),
        };
        let State::Open(mut roles, mut local) = above else {
            return State::Closed;
        };
        for item in used.iter().rev() {
            (roles, local) = apply(item, roles, local);
        }
        let (roles, local) = apply(&inputs.own, roles, local);
        State::Open(roles, local)
    }

    /// The value of the entry of `node`, as sorted ids of `names`: the
    /// value of the indexer of CMFPlone. `None` for an entry without
    /// inputs.
    pub fn value(&mut self, node: u32, names: &mut dyn Intern) -> Option<Vec<u32>> {
        let inputs = self.tree.inputs(node)?;
        if inputs.own.setting == Setting::Opaque {
            return Some(inputs.own.roles.to_vec());
        }
        let State::Open(roles, local) = self.state(node) else {
            return Some(Vec::new());
        };
        let anonymous = names.id("Anonymous");
        let mut allowed: Vec<u32> = match roles {
            Roles::Unset => vec![names.id("Manager")],
            Roles::Anonymous => vec![anonymous],
            Roles::Set(set) => set.to_vec(),
        };
        if allowed.contains(&anonymous) {
            return Some(vec![anonymous]);
        }
        let authenticated = names.id("Authenticated");
        if allowed.contains(&authenticated) {
            return Some(vec![authenticated]);
        }
        let roles = allowed.clone();
        for (principal, granted) in local.iter() {
            if granted.iter().any(|r| roles.contains(r)) {
                let token = match self.tokens.get(principal) {
                    Some(&token) => token,
                    None => {
                        let text = format!("user:{}", names.name(*principal));
                        let token = names.id(&text);
                        self.tokens.insert(*principal, token);
                        token
                    }
                };
                allowed.push(token);
            }
        }
        let owner = names.id("Owner");
        allowed.retain(|t| *t != owner);
        allowed.sort_unstable();
        allowed.dedup();
        Some(allowed)
    }
}

/// The place in `by_path` of the rows below `path`: the paths that start
/// with `path` and a slash, which sort together.
fn below(rows: &Rows, by_path: &[u32], path: &str) -> (usize, usize) {
    let low = format!("{path}/");
    // The character after the slash.
    let high = format!("{path}0");
    let start = by_path.partition_point(|r| rows.paths.get(*r) < low.as_str());
    let end = by_path.partition_point(|r| rows.paths.get(*r) < high.as_str());
    (start, end.max(start))
}

/// The base of the security index: the inputs and the value of each row.
#[derive(Debug)]
pub struct SecurityIndex {
    names: Names,
    /// The inputs of each row, `None` for a row without inputs.
    inputs: Vec<Option<Arc<Inputs>>>,
    /// The live rows, in the order of their paths.
    by_path: Vec<u32>,
    /// The distinct value of each row, `NONE` for a row without one.
    values: Vec<u32>,
    /// The distinct values, as sorted ids.
    classes: Vec<Box<[u32]>>,
    /// The rows of each distinct value.
    members: Vec<RoaringBitmap>,
    /// The live rows with inputs.
    pub indexed: RoaringBitmap,
}

/// A base as the resolver reads it.
struct BaseTree<'a> {
    rows: &'a Rows,
    inputs: &'a [Option<Arc<Inputs>>],
    by_path: &'a [u32],
}

impl BaseTree<'_> {
    fn find_row(rows: &Rows, by_path: &[u32], path: &str) -> Option<u32> {
        let at = by_path.partition_point(|r| rows.paths.get(*r) < path);
        by_path
            .get(at)
            .copied()
            .filter(|r| rows.paths.get(*r) == path)
    }
}

impl Tree for BaseTree<'_> {
    fn find(&self, path: &str) -> Option<u32> {
        Self::find_row(self.rows, self.by_path, path)
    }

    fn path(&self, node: u32) -> &str {
        self.rows.paths.get(node)
    }

    fn inputs(&self, node: u32) -> Option<&Inputs> {
        self.inputs.get(node as usize)?.as_deref()
    }
}

impl SecurityIndex {
    /// Builds the index from the items of `_inputs`, one for each row, for
    /// `rows`.
    pub fn build(items: Vec<(u32, Val)>, rows: &Rows) -> Result<SecurityIndex, String> {
        let mut names = Names::default();
        let mut sets = Sets::default();
        let mut inputs: Vec<Option<Arc<Inputs>>> = vec![None; rows.len()];
        for (row, value) in items {
            let raw = Raw::parse(&value)?;
            if let Some(slot) = inputs.get_mut(row as usize) {
                *slot = Some(Arc::new(raw.intern(&mut names, &mut sets)));
            }
        }
        Ok(Self::resolved(names, inputs, rows))
    }

    /// Builds the index of a new base: the inputs of this index, with the
    /// inputs of `changed` over them, for `rows`. The rows keep their
    /// numbers.
    pub fn rebuilt(
        &self,
        changed: Option<&BTreeMap<i32, Option<Val>>>,
        rows: &Rows,
    ) -> Result<SecurityIndex, String> {
        let mut names = self.names.clone();
        let mut sets = Sets::default();
        let mut inputs = self.inputs.clone();
        inputs.resize(rows.len(), None);
        for (rid, value) in changed.into_iter().flatten() {
            let Some(row) = rows.row(*rid) else {
                continue;
            };
            inputs[row as usize] = match value {
                Some(value) => Some(Arc::new(Raw::parse(value)?.intern(&mut names, &mut sets))),
                None => None,
            };
        }
        Ok(Self::resolved(names, inputs, rows))
    }

    fn resolved(mut names: Names, inputs: Vec<Option<Arc<Inputs>>>, rows: &Rows) -> SecurityIndex {
        let mut by_path: Vec<u32> = rows.live.iter().collect();
        by_path.sort_by(|a, b| rows.paths.get(*a).cmp(rows.paths.get(*b)));
        let indexed: RoaringBitmap = by_path
            .iter()
            .copied()
            .filter(|r| inputs.get(*r as usize).is_some_and(Option::is_some))
            .collect();
        let mut values = vec![NONE; rows.len()];
        let mut classes: Vec<Box<[u32]>> = Vec::new();
        let mut members: Vec<RoaringBitmap> = Vec::new();
        {
            let tree = BaseTree {
                rows,
                inputs: &inputs,
                by_path: &by_path,
            };
            let mut resolver = Resolver::new(&tree);
            let mut ids: HashMap<Vec<u32>, u32> = HashMap::new();
            for row in indexed.iter() {
                let Some(value) = resolver.value(row, &mut names) else {
                    continue;
                };
                let class = *ids.entry(value).or_insert_with_key(|value| {
                    classes.push(value.clone().into());
                    members.push(RoaringBitmap::new());
                    (classes.len() - 1) as u32
                });
                values[row as usize] = class;
                members[class as usize].insert(row);
            }
        }
        SecurityIndex {
            names,
            inputs,
            by_path,
            values,
            classes,
            members,
            indexed,
        }
    }

    /// The names of the index.
    pub fn names(&self) -> &Names {
        &self.names
    }

    /// The inputs of `row` in the form that the change sets keep.
    pub fn value(&self, row: u32) -> Option<Val> {
        let inputs = self.inputs.get(row as usize)?.as_deref()?;
        Some(inputs.to_val(&self.names))
    }

    /// The resolved value of `row`, as names.
    pub fn tokens(&self, row: u32) -> Option<Vec<&str>> {
        let class = *self.values.get(row as usize)?;
        if class == NONE {
            return None;
        }
        let mut out: Vec<&str> = self.classes[class as usize]
            .iter()
            .map(|id| self.names.name(*id))
            .collect();
        out.sort_unstable();
        Some(out)
    }

    /// The distinct values, as names, with the number of their rows.
    pub fn distinct(&self) -> Vec<(Vec<&str>, u64)> {
        self.classes
            .iter()
            .zip(&self.members)
            .map(|(class, rows)| {
                let names = class.iter().map(|id| self.names.name(*id)).collect();
                (names, rows.len())
            })
            .collect()
    }

    /// The rows whose value holds one of `keys`, or, with `and`, each of
    /// them. `keys` are ids of the names of a version of the index.
    pub fn matching(&self, keys: &[u32], and: bool) -> RoaringBitmap {
        let mut out = RoaringBitmap::new();
        for (class, rows) in self.classes.iter().zip(&self.members) {
            if matches(class, keys, and) {
                out |= rows;
            }
        }
        out
    }

    /// The number of live rows at `path` and below it.
    pub fn below_count(&self, rows: &Rows, path: &str) -> usize {
        let (start, end) = below(rows, &self.by_path, path);
        let own = BaseTree::find_row(rows, &self.by_path, path).is_some();
        end - start + usize::from(own)
    }

    /// The bytes that the index takes.
    pub fn heap_bytes(&self) -> usize {
        self.names.heap_bytes()
            + self.inputs.capacity() * 8
            + self
                .inputs
                .iter()
                .flatten()
                .map(|i| i.heap_bytes())
                .sum::<usize>()
            + self.by_path.capacity() * 4
            + self.values.capacity() * 4
            + self.classes.iter().map(|c| c.len() * 4 + 16).sum::<usize>()
            + self
                .members
                .iter()
                .map(|m| m.serialized_size())
                .sum::<usize>()
            + self.indexed.serialized_size()
    }
}

/// Tells if the value `value` holds one of `keys`, or, with `and`, each of
/// them.
fn matches(value: &[u32], keys: &[u32], and: bool) -> bool {
    if and {
        !keys.is_empty() && keys.iter().all(|k| value.contains(k))
    } else {
        keys.iter().any(|k| value.contains(k))
    }
}

/// A version of the index: its base, with the inputs and the paths of the
/// rows that the overrides changed. Its nodes are the rows of the view of
/// the query: the rows of the base, then the rows of the rids that the
/// base does not hold.
pub struct Version<'a> {
    base: &'a SecurityIndex,
    rows: &'a Rows,
    /// The inputs of the rows whose inputs changed.
    inputs: HashMap<u32, Option<Inputs>>,
    /// The path of the rows whose path changed, `None` for a row that left
    /// the catalog.
    paths: HashMap<u32, Option<Box<str>>>,
    /// The row of each path of `paths`.
    at: HashMap<Box<str>, u32>,
}

impl<'a> Version<'a> {
    /// The version of `base` with the changed inputs `inputs` and the
    /// changed paths `paths`, each by row. New names go to `names`.
    pub fn new<'v>(
        base: &'a SecurityIndex,
        rows: &'a Rows,
        names: &mut Overlay<'_>,
        inputs: impl IntoIterator<Item = (u32, Option<&'v Val>)>,
        paths: impl IntoIterator<Item = (u32, Option<&'v Val>)>,
    ) -> Result<Version<'a>, String> {
        let mut sets = Sets::default();
        let mut changed = HashMap::new();
        for (row, value) in inputs {
            let found = match value {
                Some(value) => Some(Raw::parse(value)?.intern(names, &mut sets)),
                None => None,
            };
            changed.insert(row, found);
        }
        let mut moved = HashMap::new();
        let mut at = HashMap::new();
        for (row, value) in paths {
            let path: Option<Box<str>> = value.and_then(Val::text).map(Into::into);
            if let Some(path) = &path {
                at.insert(path.clone(), row);
            }
            moved.insert(row, path);
        }
        Ok(Version {
            base,
            rows,
            inputs: changed,
            paths: moved,
            at,
        })
    }

    /// Tells if the version differs from its base.
    pub fn changed(&self) -> bool {
        !self.inputs.is_empty() || !self.paths.is_empty()
    }

    /// The rows whose value may differ from the base: the rows whose
    /// inputs or path changed, and the rows of the base at their paths and
    /// below them.
    pub fn dirty(&self) -> RoaringBitmap {
        let mut out = RoaringBitmap::new();
        let base_len = self.rows.len() as u32;
        for &row in self.inputs.keys().chain(self.paths.keys()) {
            out.insert(row);
            let mut at = |path: &str| {
                let (start, end) = below(self.rows, &self.base.by_path, path);
                out.extend(self.base.by_path[start..end].iter().copied());
                if let Some(found) = BaseTree::find_row(self.rows, &self.base.by_path, path) {
                    out.insert(found);
                }
            };
            if row < base_len {
                at(self.rows.paths.get(row));
            }
            if let Some(Some(path)) = self.paths.get(&row) {
                at(path);
            }
        }
        out
    }

    /// The rows of the base whose value holds the keys, as `matching` of
    /// the base.
    pub fn base_matching(&self, keys: &[u32], and: bool) -> RoaringBitmap {
        self.base.matching(keys, and)
    }
}

impl Tree for Version<'_> {
    fn find(&self, path: &str) -> Option<u32> {
        if let Some(&row) = self.at.get(path) {
            return Some(row);
        }
        let row = BaseTree::find_row(self.rows, &self.base.by_path, path)?;
        (!self.paths.contains_key(&row)).then_some(row)
    }

    fn path(&self, node: u32) -> &str {
        match self.paths.get(&node) {
            Some(Some(path)) => path,
            _ if (node as usize) < self.rows.len() => self.rows.paths.get(node),
            _ => "",
        }
    }

    fn inputs(&self, node: u32) -> Option<&Inputs> {
        match self.inputs.get(&node) {
            Some(found) => found.as_ref(),
            None => self.base.inputs.get(node as usize)?.as_deref(),
        }
    }
}

/// Tells if the value `value` matches the keys, for a query of a version.
pub fn value_matches(value: &[u32], keys: &[u32], and: bool) -> bool {
    matches(value, keys, and)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(kind: &str, roles: &[&str], local: &[(&str, &[&str])], block: bool) -> Val {
        let strs =
            |items: &[&str]| Val::Tuple(items.iter().map(|s| Val::Str((*s).into())).collect());
        Val::Tuple(
            vec![
                Val::Str(kind.into()),
                strs(roles),
                Val::Tuple(
                    local
                        .iter()
                        .map(|(p, r)| Val::Tuple(vec![Val::Str((*p).into()), strs(r)].into()))
                        .collect(),
                ),
                Val::Bool(block),
            ]
            .into(),
        )
    }

    fn entry(own: Val, chain: Vec<(&str, Val)>) -> Val {
        let mut items = items(&own).unwrap().to_vec();
        items.push(Val::Tuple(
            chain
                .into_iter()
                .map(|(path, n)| {
                    let mut item = vec![Val::Str(path.into())];
                    item.extend(super::items(&n).unwrap().iter().cloned());
                    Val::Tuple(item.into())
                })
                .collect(),
        ));
        Val::Tuple(items.into())
    }

    #[test]
    fn the_form_of_the_change_sets_sorts_and_merges() {
        let value = entry(
            node(
                "t",
                &["Reader", "Manager", "Reader"],
                &[("bob", &["Reader"]), ("al", &[]), ("bob", &["Editor"])],
                false,
            ),
            vec![],
        );
        let raw = Raw::parse(&value).unwrap();
        assert_eq!(
            raw.to_val(),
            entry(
                node(
                    "t",
                    &["Manager", "Reader"],
                    &[("bob", &["Editor", "Reader"])],
                    false
                ),
                vec![]
            )
        );
        assert!(Raw::parse(&Val::Tuple(vec![Val::Str("x".into())].into())).is_err());
        assert!(Raw::parse(&entry(node("?", &[], &[], false), vec![])).is_err());
    }

    #[test]
    fn a_child_follows_its_folder_and_the_chain() {
        let pairs = vec![
            (1, "/site".to_owned()),
            (2, "/site/folder".to_owned()),
            (3, "/site/folder/doc".to_owned()),
            (4, "/site/folder/file".to_owned()),
            (5, "/site/folder/doc/++conversation++default/7".to_owned()),
        ];
        let rows = Rows::from_sorted(&pairs);
        let private = ["Manager", "Owner", "Reader", "Editor"];
        let items = vec![
            (
                0,
                entry(
                    node("l", &["Reader"], &[], false),
                    vec![("", node("t", &["Anonymous", "Manager"], &[], false))],
                ),
            ),
            (
                1,
                entry(
                    node(
                        "t",
                        &private,
                        &[("alice", &["Owner"]), ("bob", &["Reader"])],
                        false,
                    ),
                    vec![],
                ),
            ),
            (2, entry(node("t", &private, &[], false), vec![])),
            (3, entry(node("", &[], &[], false), vec![])),
            (
                4,
                entry(
                    node("", &[], &[], false),
                    vec![(
                        "/site/folder/doc/++conversation++default",
                        node("", &[], &[("carol", &["Reader"])], false),
                    )],
                ),
            ),
        ];
        let index = SecurityIndex::build(items, &rows).unwrap();
        assert_eq!(index.tokens(0).unwrap(), ["Anonymous"]);
        assert_eq!(
            index.tokens(2).unwrap(),
            ["Editor", "Manager", "Reader", "user:alice", "user:bob"].map(|s| s)
        );
        assert_eq!(index.tokens(3).unwrap(), index.tokens(2).unwrap());
        // The comment takes the local roles of its conversation.
        assert!(index.tokens(4).unwrap().contains(&"user:carol"));
    }

    #[test]
    fn an_entry_below_an_opaque_or_unknown_node_gives_no_token() {
        let pairs = vec![
            (1, "/site".to_owned()),
            (2, "/site/workspace".to_owned()),
            (3, "/site/workspace/doc".to_owned()),
            (4, "/site/gap/doc".to_owned()),
        ];
        let rows = Rows::from_sorted(&pairs);
        let items = vec![
            (0, entry(node("t", &["Anonymous"], &[], false), vec![])),
            (
                1,
                entry(node("v", &["Reader", "user:bob"], &[], false), vec![]),
            ),
            (2, entry(node("", &[], &[], false), vec![])),
            (3, entry(node("", &[], &[], false), vec![])),
        ];
        let index = SecurityIndex::build(items, &rows).unwrap();
        assert_eq!(index.tokens(1).unwrap(), ["Reader", "user:bob"]);
        assert!(index.tokens(2).unwrap().is_empty());
        assert!(index.tokens(3).unwrap().is_empty());
    }
}
