//! The security index: `allowedRolesAndUsers` resolved from inherited inputs.
//!
//! Each object holds three inputs of its value: the setting of the
//! permission "Access contents information", its own local roles, and its
//! block flag. The value of an entry is a function of the inputs of the
//! object and of its containers, as the indexer of Plone computes it. The
//! index keeps the inputs of each node and resolves the value of every
//! entry through the parent of each node. A change of the inputs of one
//! folder changes one node; a new resolution gives the values below it.

use std::collections::HashMap;

/// The kind of the permission setting of a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// No setting: the roles of the container.
    Missing,
    /// A tuple: these roles only.
    Tuple,
    /// A list: these roles and the roles of the container.
    List,
    /// The setting `None`: Anonymous only.
    Anonymous,
    /// The whole value, from a provider that the index cannot read.
    Opaque,
}

/// The inputs that one object holds itself. Roles and principals are ids
/// of the interner of the index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inputs {
    /// The kind of the permission setting.
    pub kind: Kind,
    /// The roles of the setting, or the whole value of an opaque node.
    pub roles: Vec<u32>,
    /// The local roles of the object, by principal.
    pub local: LocalRoles,
    /// True when the object blocks the local roles of its containers.
    pub block: bool,
}

/// The local roles of a node: each principal with its roles.
pub type LocalRoles = Vec<(u32, Vec<u32>)>;

impl Inputs {
    fn empty() -> Self {
        Inputs {
            kind: Kind::Missing,
            roles: Vec::new(),
            local: Vec::new(),
            block: false,
        }
    }
}

/// The resolved roles of the permission at a node.
#[derive(Clone, Debug)]
enum Roles {
    /// No node of the chain sets them: the default roles.
    Unset,
    Anonymous,
    Set(Vec<u32>),
}

/// The nodes of the tree of paths and their inputs.
#[derive(Default)]
pub struct SecurityIndex {
    names: HashMap<String, u32>,
    strings: Vec<String>,
    nodes: HashMap<String, usize>,
    parents: Vec<Option<usize>>,
    inputs: Vec<Inputs>,
}

impl SecurityIndex {
    /// Return an empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the id of a role, a principal or a token.
    pub fn intern(&mut self, name: &str) -> u32 {
        if let Some(&id) = self.names.get(name) {
            return id;
        }
        let id = u32::try_from(self.strings.len()).expect("fewer than 2^32 names");
        self.names.insert(name.to_owned(), id);
        self.strings.push(name.to_owned());
        id
    }

    /// Return the name of an id.
    pub fn name(&self, id: u32) -> &str {
        &self.strings[id as usize]
    }

    /// Return the node of `path`, and add it and its containers when they
    /// are new. The root has the path "".
    pub fn node(&mut self, path: &str) -> usize {
        if let Some(&node) = self.nodes.get(path) {
            return node;
        }
        let parent = if path.is_empty() {
            None
        } else {
            let (above, _) = path.rsplit_once('/').unwrap_or(("", path));
            Some(self.node(above))
        };
        let node = self.parents.len();
        self.parents.push(parent);
        self.inputs.push(Inputs::empty());
        self.nodes.insert(path.to_owned(), node);
        node
    }

    /// Set the inputs of the node of `path`.
    pub fn set(&mut self, path: &str, inputs: Inputs) -> usize {
        let node = self.node(path);
        self.inputs[node] = inputs;
        node
    }

    /// Resolve the value of every node. The value is a sorted list of
    /// token ids.
    pub fn resolve(&mut self) -> Vec<Vec<u32>> {
        let anonymous = self.intern("Anonymous");
        let authenticated = self.intern("Authenticated");
        let owner = self.intern("Owner");
        let manager = self.intern("Manager");
        let count = self.parents.len();
        let mut roles: Vec<Option<Roles>> = vec![None; count];
        let mut local: Vec<Option<LocalRoles>> = vec![None; count];
        let mut users: HashMap<u32, u32> = HashMap::new();
        let mut values = Vec::with_capacity(count);
        for node in 0..count {
            self.roles_at(node, &mut roles);
            self.local_at(node, &mut local);
        }
        for node in 0..count {
            let inputs = &self.inputs[node];
            if inputs.kind == Kind::Opaque {
                values.push(inputs.roles.clone());
                continue;
            }
            let mut allowed: Vec<u32> = match roles[node].as_ref().expect("resolved") {
                Roles::Unset => vec![manager],
                Roles::Anonymous => vec![anonymous],
                Roles::Set(set) => set.clone(),
            };
            if allowed.contains(&anonymous) {
                values.push(vec![anonymous]);
                continue;
            }
            if allowed.contains(&authenticated) {
                values.push(vec![authenticated]);
                continue;
            }
            let granted = local[node].as_ref().expect("resolved");
            let mut added = Vec::new();
            for (principal, held) in granted {
                if held.iter().any(|role| allowed.contains(role)) {
                    added.push(*principal);
                }
            }
            for principal in added {
                let token = match users.get(&principal) {
                    Some(&token) => token,
                    None => {
                        let text = format!("user:{}", self.strings[principal as usize]);
                        let token = self.intern(&text);
                        users.insert(principal, token);
                        token
                    }
                };
                allowed.push(token);
            }
            allowed.retain(|&token| token != owner);
            allowed.sort_unstable();
            allowed.dedup();
            values.push(allowed);
        }
        values
    }

    fn roles_at(&self, node: usize, memo: &mut [Option<Roles>]) {
        // Walk up to the first node that has a memo or that stops the
        // walk, then fill the memo on the way down.
        let mut chain = Vec::new();
        let mut current = Some(node);
        while let Some(at) = current {
            if memo[at].is_some() {
                break;
            }
            chain.push(at);
            match self.inputs[at].kind {
                Kind::Tuple | Kind::Anonymous | Kind::Opaque => break,
                Kind::Missing | Kind::List => current = self.parents[at],
            }
        }
        for &at in chain.iter().rev() {
            let above = || match self.parents[at] {
                Some(parent) => memo[parent].clone().expect("resolved above"),
                None => Roles::Unset,
            };
            let inputs = &self.inputs[at];
            let found = match inputs.kind {
                Kind::Tuple => Roles::Set(sorted(inputs.roles.clone())),
                Kind::Anonymous => Roles::Anonymous,
                Kind::Opaque => Roles::Unset,
                Kind::Missing => above(),
                Kind::List => match above() {
                    Roles::Anonymous => Roles::Anonymous,
                    Roles::Unset if inputs.roles.is_empty() => Roles::Unset,
                    Roles::Unset => Roles::Set(sorted(inputs.roles.clone())),
                    Roles::Set(set) => {
                        let mut all = set;
                        all.extend(inputs.roles.iter().copied());
                        Roles::Set(sorted(all))
                    }
                },
            };
            memo[at] = Some(found);
        }
    }

    fn local_at(&self, node: usize, memo: &mut [Option<LocalRoles>]) {
        let mut chain = Vec::new();
        let mut current = Some(node);
        while let Some(at) = current {
            if memo[at].is_some() {
                break;
            }
            chain.push(at);
            if self.inputs[at].block {
                break;
            }
            current = self.parents[at];
        }
        for &at in chain.iter().rev() {
            let inputs = &self.inputs[at];
            let mut found: LocalRoles = if inputs.block {
                Vec::new()
            } else {
                match self.parents[at] {
                    Some(parent) => memo[parent].clone().expect("resolved above"),
                    None => Vec::new(),
                }
            };
            for (principal, roles) in &inputs.local {
                match found.iter_mut().find(|(held, _)| held == principal) {
                    Some((_, held)) => {
                        held.extend(roles.iter().copied());
                        held.sort_unstable();
                        held.dedup();
                    }
                    None => found.push((*principal, sorted(roles.clone()))),
                }
            }
            memo[at] = Some(found);
        }
    }

    /// The number of nodes.
    pub fn len(&self) -> usize {
        self.parents.len()
    }

    /// True when the index has no node.
    pub fn is_empty(&self) -> bool {
        self.parents.is_empty()
    }
}

fn sorted(mut ids: Vec<u32>) -> Vec<u32> {
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// The entries of each distinct value, as a bitset of entry numbers.
pub struct Classes {
    /// The distinct values.
    pub values: Vec<Vec<u32>>,
    /// The entries of each value.
    pub members: Vec<Vec<u64>>,
}

impl Classes {
    /// Group `values`, the value of each entry, by distinct value.
    pub fn new(values: &[&Vec<u32>]) -> Self {
        let words = values.len().div_ceil(64);
        let mut ids: HashMap<&Vec<u32>, usize> = HashMap::new();
        let mut classes = Classes {
            values: Vec::new(),
            members: Vec::new(),
        };
        for (entry, value) in values.iter().enumerate() {
            let class = *ids.entry(value).or_insert_with(|| {
                classes.values.push((*value).clone());
                classes.members.push(vec![0; words]);
                classes.values.len() - 1
            });
            classes.members[class][entry / 64] |= 1 << (entry % 64);
        }
        classes
    }

    /// The entries whose value holds one of `tokens`.
    pub fn query(&self, tokens: &[u32]) -> Vec<u64> {
        let words = self.members.first().map_or(0, Vec::len);
        let mut found = vec![0; words];
        for (value, members) in self.values.iter().zip(&self.members) {
            if value.iter().any(|token| tokens.contains(token)) {
                for (word, bits) in found.iter_mut().zip(members) {
                    *word |= bits;
                }
            }
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(
        index: &mut SecurityIndex,
        kind: Kind,
        roles: &[&str],
        local: &[(&str, &[&str])],
    ) -> Inputs {
        Inputs {
            kind,
            roles: roles.iter().map(|r| index.intern(r)).collect(),
            local: local
                .iter()
                .map(|(p, r)| (index.intern(p), r.iter().map(|x| index.intern(x)).collect()))
                .collect(),
            block: false,
        }
    }

    fn names(index: &SecurityIndex, value: &[u32]) -> Vec<String> {
        let mut out: Vec<String> = value.iter().map(|&id| index.name(id).to_owned()).collect();
        out.sort();
        out
    }

    #[test]
    fn a_child_follows_a_change_of_its_folder() {
        let mut index = SecurityIndex::new();
        let private = ["Manager", "Owner", "Reader", "Editor"];
        let folder = inputs(&mut index, Kind::Tuple, &private, &[("alice", &["Owner"])]);
        index.set("/site/folder", folder);
        let doc = inputs(&mut index, Kind::Tuple, &private, &[]);
        let child = index.set("/site/folder/doc", doc);
        let file = inputs(&mut index, Kind::Missing, &[], &[]);
        let attached = index.set("/site/folder/file", file);
        let values = index.resolve();
        assert_eq!(
            names(&index, &values[child]),
            ["Editor", "Manager", "Reader", "user:alice"]
        );
        // A share on the folder changes the folder alone.
        let shared = inputs(
            &mut index,
            Kind::Tuple,
            &private,
            &[("alice", &["Owner"]), ("bob", &["Reader"])],
        );
        index.set("/site/folder", shared);
        let values = index.resolve();
        assert_eq!(
            names(&index, &values[child]),
            ["Editor", "Manager", "Reader", "user:alice", "user:bob"]
        );
        // A transition of the folder changes the file, which has no setting.
        let published = inputs(&mut index, Kind::Tuple, &["Anonymous"], &[]);
        index.set("/site/folder", published);
        let values = index.resolve();
        assert_eq!(names(&index, &values[attached]), ["Anonymous"]);
        assert_eq!(
            names(&index, &values[child]),
            ["Editor", "Manager", "Reader"]
        );
    }
}
