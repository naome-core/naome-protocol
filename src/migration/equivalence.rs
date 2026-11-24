//! Connected components of verified format mappings. Physical identity is evaluated separately.
use crate::wire::Id;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub struct Equivalence {
    group_for: BTreeMap<Id, usize>,
    groups: Vec<BTreeSet<Id>>,
}

impl Equivalence {
    pub fn add(&mut self, pairs: BTreeMap<Id, Id>) {
        for (a, b) in pairs {
            if a == b {
                continue;
            }
            match (
                self.group_for.get(&a).copied(),
                self.group_for.get(&b).copied(),
            ) {
                (None, None) => {
                    let index = self.groups.len();
                    self.groups.push(BTreeSet::from([a, b]));
                    self.group_for.extend([(a, index), (b, index)]);
                }
                (Some(index), None) => self.attach(index, b),
                (None, Some(index)) => self.attach(index, a),
                (Some(left), Some(right)) if left != right => {
                    // Moving the smaller component bounds how often a member changes groups.
                    let (keep, move_from) = if self.groups[left].len() >= self.groups[right].len() {
                        (left, right)
                    } else {
                        (right, left)
                    };
                    for id in std::mem::take(&mut self.groups[move_from]) {
                        self.attach(keep, id);
                    }
                }
                _ => {}
            }
        }
    }

    fn attach(&mut self, index: usize, id: Id) {
        self.groups[index].insert(id);
        self.group_for.insert(id, index);
    }

    fn group(&self, id: Id) -> Option<&BTreeSet<Id>> {
        self.group_for.get(&id).map(|&index| &self.groups[index])
    }

    pub(crate) fn representative(&self, id: Id) -> Id {
        self.group(id)
            .and_then(BTreeSet::first)
            .copied()
            .unwrap_or(id)
    }

    pub(crate) fn equivalent(&self, left: Id, right: Id) -> bool {
        left == right || self.group(left).is_some_and(|ids| ids.contains(&right))
    }

    pub(crate) fn intersects(&self, id: Id, other: &BTreeSet<Id>) -> bool {
        match self.group(id) {
            Some(ids) => !ids.is_disjoint(other),
            None => other.contains(&id),
        }
    }

    pub(crate) fn members(&self, id: Id) -> impl Iterator<Item = Id> + '_ {
        let group = self.group(id);
        group
            .into_iter()
            .flatten()
            .copied()
            .chain(group.is_none().then_some(id))
    }

    pub fn endpoints(&self, id: Id) -> BTreeSet<Id> {
        self.group(id)
            .cloned()
            .unwrap_or_else(|| BTreeSet::from([id]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_mappings_share_the_lowest_representative_in_either_order() {
        let [a, b, c, d, other] = [0, 1, 2, 3, 4].map(|n| Id::from_blob_digest([n; 32]));
        for links in [[(d, c), (b, a), (c, b)], [(a, b), (b, c), (d, c)]] {
            let mut aliases = Equivalence::default();
            for link in links {
                aliases.add([link].into());
            }
            aliases.add([(b, d), (other, other)].into());
            for id in [a, b, c, d] {
                assert_eq!(aliases.representative(id), a);
                assert_eq!(aliases.members(id).collect::<Vec<_>>(), [a, b, c, d]);
                assert_eq!(aliases.endpoints(id), [a, b, c, d].into());
                assert!(aliases.equivalent(id, c));
                assert!(!aliases.intersects(id, &[other].into()));
            }
            assert_eq!(aliases.representative(other), other);
            assert_eq!(aliases.endpoints(other), [other].into());
            assert!(!aliases.equivalent(other, a));
        }
    }
}
