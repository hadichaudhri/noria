use std::collections::HashMap;
use std::collections::VecDeque;

use petgraph::graph::NodeIndex;

use flow::alt;
use flow::alt::domain::list;

use ops;
use query;
use shortcut;

pub struct NodeDescriptor {
    pub index: NodeIndex,
    pub inner: alt::Node,
    pub children: Vec<NodeIndex>,
}

impl NodeDescriptor {
    pub fn iterate(&mut self,
                   handoffs: &mut HashMap<NodeIndex, VecDeque<alt::Message>>,
                   state: &mut HashMap<NodeIndex, shortcut::Store<query::DataType>>,
                   nodes: &list::NodeList) {
        match self.inner {
            alt::Node::Ingress(_, ref mut rx) => {
                // receive an update
                debug_assert!(handoffs[&self.index].is_empty());
                broadcast!(handoffs, rx.recv().unwrap(), &self.children[..]);
            }
            alt::Node::Egress(_, ref txs) => {
                // send any queued updates to all external children
                let mut txs = txs.lock().unwrap();
                let txn = txs.len() - 1;
                while let Some(m) = handoffs.get_mut(&self.index).unwrap().pop_front() {
                    let mut m = Some(m); // so we can use .take()
                    for (txi, tx) in txs.iter_mut().enumerate() {
                        if txi == txn && self.children.is_empty() {
                            tx.send(m.take().unwrap()).unwrap();
                        } else {
                            tx.send(m.clone().unwrap()).unwrap();
                        }
                    }

                    if let Some(m) = m {
                        broadcast!(handoffs, m, &self.children[..]);
                    } else {
                        debug_assert!(self.children.is_empty());
                    }
                }
            }
            alt::Node::Internal(..) => {
                while let Some(m) = handoffs.get_mut(&self.index).unwrap().pop_front() {
                    if let Some(u) = self.process_one(m, state) {
                        broadcast!(handoffs,
                                   alt::Message {
                                       from: self.index,
                                       data: u,
                                   },
                                   &self.children[..]);
                    }
                }
            }
            _ => unreachable!(),
        }
    }

    fn process_one(&mut self,
                   m: alt::Message,
                   state: &mut HashMap<NodeIndex, shortcut::Store<query::DataType>>)
                   -> Option<ops::Update> {

        // first, process the incoming message
        let u = match self.inner {
            alt::Node::Internal(_, ref mut i) => i.process(m),
            _ => unreachable!(),
        };

        // if our output didn't change there's nothing more to do
        if u.is_none() {
            return u;
        }

        // our output changed -- do we need to modify materialized state?
        let state = state.get_mut(&self.index);
        if state.is_none() {
            // nope
            return u;
        }

        // yes!
        let mut state = state.unwrap();
        if let Some(ops::Update::Records(ref rs)) = u {
            for r in rs.iter().cloned() {
                match r {
                    ops::Record::Positive(r, _) => state.insert(r),
                    ops::Record::Negative(r, _) => {
                        // we need a cond that will match this row.
                        let conds = r.into_iter()
                            .enumerate()
                            .map(|(coli, v)| {
                                shortcut::Condition {
                                    column: coli,
                                    cmp: shortcut::Comparison::Equal(shortcut::Value::Const(v)),
                                }
                            })
                            .collect::<Vec<_>>();

                        // however, multiple rows may have the same values as this row for every
                        // column. afaict, it is safe to delete any one of these rows. we do this
                        // by returning true for the first invocation of the filter function, and
                        // false for all subsequent invocations.
                        let mut first = true;
                        state.delete_filter(&conds[..], |_| {
                            if first {
                                first = false;
                                true
                            } else {
                                false
                            }
                        });
                    }
                }
            }
        }

        u
    }
}
