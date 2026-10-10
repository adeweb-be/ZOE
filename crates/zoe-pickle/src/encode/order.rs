//! The opcodes of a node in pickle order.
//!
//! The encoder writes them, and `Tree::resolve` follows the memo through
//! them. The order comes from an explicit stack of tasks, so the depth of a
//! node does not grow the call stack.

use crate::error::Error;
use crate::opcode::*;
use crate::tree::{Batch, Dict, List, Node, Put};

/// One step of a pickle.
pub(crate) enum Op<'a> {
    /// An opcode of one byte with no argument, for example MARK or TUPLE.
    Byte(u8),
    /// A node with no children. The encoder writes it whole.
    Leaf(&'a Node),
    /// A memo put, and the value inside its chain of puts.
    Put(&'a Put, &'a Node),
}

enum Task<'a> {
    Node(&'a Node),
    Byte(u8),
    Put(&'a Put, &'a Node),
    Nodes(&'a [Node]),
    Pairs(&'a [(Node, Node)]),
    ListBatches(&'a [Batch<Node>]),
    DictBatches(&'a [Batch<(Node, Node)>]),
}

/// An iterator over the opcodes of a node, in pickle order. After an
/// error, it ends.
pub(crate) struct Order<'a> {
    tasks: Vec<Task<'a>>,
}

impl<'a> Order<'a> {
    pub(crate) fn new(root: &'a Node) -> Self {
        Order {
            tasks: vec![Task::Node(root)],
        }
    }

    /// Takes the next task. Returns the op that it gives, if any.
    fn step(&mut self, task: Task<'a>) -> Result<Option<Op<'a>>, Error> {
        match task {
            Task::Byte(opcode) => return Ok(Some(Op::Byte(opcode))),
            Task::Put(put, value) => return Ok(Some(Op::Put(put, value))),
            Task::Nodes([]) | Task::Pairs([]) | Task::ListBatches([]) | Task::DictBatches([]) => {}
            Task::Nodes([node, rest @ ..]) => {
                self.push_rest(Task::Nodes(rest), rest.is_empty());
                self.tasks.push(Task::Node(node));
            }
            Task::Pairs([(key, value), rest @ ..]) => {
                self.push_rest(Task::Pairs(rest), rest.is_empty());
                self.tasks.push(Task::Node(value));
                self.tasks.push(Task::Node(key));
            }
            Task::ListBatches([batch, rest @ ..]) => {
                self.push_rest(Task::ListBatches(rest), rest.is_empty());
                return self.batch(batch.marked, batch.items.len(), APPEND, APPENDS, |tasks| {
                    tasks.push(Task::Nodes(&batch.items));
                });
            }
            Task::DictBatches([batch, rest @ ..]) => {
                self.push_rest(Task::DictBatches(rest), rest.is_empty());
                return self.batch(
                    batch.marked,
                    batch.items.len(),
                    SETITEM,
                    SETITEMS,
                    |tasks| {
                        tasks.push(Task::Pairs(&batch.items));
                    },
                );
            }
            Task::Node(node) => return self.node(node),
        }
        Ok(None)
    }

    fn push_rest(&mut self, task: Task<'a>, empty: bool) {
        if !empty {
            self.tasks.push(task);
        }
    }

    fn node(&mut self, node: &'a Node) -> Result<Option<Op<'a>>, Error> {
        match node {
            Node::Tuple(tuple) => {
                let close = match (tuple.marked, tuple.items.len()) {
                    (true, _) => TUPLE,
                    (false, 0) => EMPTY_TUPLE,
                    (false, 1) => TUPLE1,
                    (false, 2) => TUPLE2,
                    (false, 3) => TUPLE3,
                    (false, len) => {
                        return Err(Error::Encode(format!(
                            "a tuple without a mark has {len} items"
                        )));
                    }
                };
                self.tasks.push(Task::Byte(close));
                self.tasks.push(Task::Nodes(&tuple.items));
                Ok(tuple.marked.then_some(Op::Byte(MARK)))
            }
            Node::List(list) => {
                self.tasks.push(Task::ListBatches(&list.batches));
                Ok(Some(self.create_list(list)))
            }
            Node::Dict(dict) => {
                self.tasks.push(Task::DictBatches(&dict.batches));
                Ok(Some(self.create_dict(dict)))
            }
            Node::Persistent(pid) => {
                self.tasks.push(Task::Byte(BINPERSID));
                self.tasks.push(Task::Node(pid));
                Ok(None)
            }
            Node::Reduce(reduce) => {
                self.tasks.push(Task::Byte(REDUCE));
                self.tasks.push(Task::Node(&reduce.args));
                self.tasks.push(Task::Node(&reduce.callable));
                Ok(None)
            }
            Node::Put(_) => Ok(self.puts(node)),
            leaf => Ok(Some(Op::Leaf(leaf))),
        }
    }

    /// Plans a chain of puts. The puts come after the value, from the
    /// innermost out. For a list or a dict, they come after the opcodes
    /// that create it and before its batches.
    fn puts(&mut self, node: &'a Node) -> Option<Op<'a>> {
        let value = node.value();
        match value {
            Node::List(list) => self.tasks.push(Task::ListBatches(&list.batches)),
            Node::Dict(dict) => self.tasks.push(Task::DictBatches(&dict.batches)),
            _ => {}
        }
        let mut outer = node;
        while let Node::Put(put) = outer {
            self.tasks.push(Task::Put(put, value));
            outer = &put.value;
        }
        match value {
            Node::List(list) => Some(self.create_list(list)),
            Node::Dict(dict) => Some(self.create_dict(dict)),
            _ => {
                self.tasks.push(Task::Node(value));
                None
            }
        }
    }

    /// Plans the opcodes that create a list: EMPTY_LIST, or MARK, the
    /// items, LIST. Returns the first one.
    fn create_list(&mut self, list: &'a List) -> Op<'a> {
        match &list.initial {
            Some(items) => {
                self.tasks.push(Task::Byte(LIST));
                self.tasks.push(Task::Nodes(items));
                Op::Byte(MARK)
            }
            None => Op::Byte(EMPTY_LIST),
        }
    }

    /// Plans the opcodes that create a dict: EMPTY_DICT, or MARK, the
    /// items, DICT. Returns the first one.
    fn create_dict(&mut self, dict: &'a Dict) -> Op<'a> {
        match &dict.initial {
            Some(items) => {
                self.tasks.push(Task::Byte(DICT));
                self.tasks.push(Task::Pairs(items));
                Op::Byte(MARK)
            }
            None => Op::Byte(EMPTY_DICT),
        }
    }

    /// Plans one batch of a list or a dict: MARK, the items, then the
    /// opcode for many; or one item, then the opcode for one.
    fn batch(
        &mut self,
        marked: bool,
        len: usize,
        one: u8,
        many: u8,
        items: impl FnOnce(&mut Vec<Task<'a>>),
    ) -> Result<Option<Op<'a>>, Error> {
        if !marked && len != 1 {
            return Err(Error::Encode(format!(
                "a batch without a mark has {len} items, not 1"
            )));
        }
        self.tasks.push(Task::Byte(if marked { many } else { one }));
        items(&mut self.tasks);
        Ok(marked.then_some(Op::Byte(MARK)))
    }
}

impl<'a> Iterator for Order<'a> {
    type Item = Result<Op<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(task) = self.tasks.pop() {
            match self.step(task) {
                Ok(Some(op)) => return Some(Ok(op)),
                Ok(None) => {}
                Err(error) => {
                    self.tasks.clear();
                    return Some(Err(error));
                }
            }
        }
        None
    }
}
