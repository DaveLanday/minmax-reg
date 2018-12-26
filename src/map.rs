use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Debug;
use std::cmp::Ordering;

use serde_derive::{Serialize, Deserialize};

use crate::traits::{Causal, CvRDT, CmRDT};
use crate::vclock::{Dot, VClock, Actor};
use crate::ctx::{ReadCtx, AddCtx, RmCtx};

/// Key Trait alias to reduce redundancy in type decl.
pub trait Key: Debug + Ord + Clone {}
impl<T: Debug + Ord + Clone> Key for T {}

/// Val Trait alias to reduce redundancy in type decl.
pub trait Val<A: Actor>: Debug + Default + Clone + Causal<A> + CmRDT + CvRDT {}

impl<A, T> Val<A> for T where
    A: Actor,
    T: Debug + Default + Clone + Causal<A> + CmRDT + CvRDT
{}

/// Map CRDT - Supports Composition of CRDT's with reset-remove semantics.
///
/// Reset-remove means that if one replica removes an entry while another
/// actor concurrently edits that entry, once we sync these two maps, we
/// will see that the entry is still in the map but all edits seen by the
/// removing actor will be gone.
///
/// See examples/reset_remove.rs for an example of reset-remove semantics
/// in action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Map<K: Key, V: Val<A>, A: Actor> {
    // This clock stores the current version of the Map, it should
    // be greator or equal to all Entry.clock's in the Map.
    clock: VClock<A>,
    entries: BTreeMap<K, Entry<V, A>>,
    deferred: HashMap<VClock<A>, BTreeSet<K>>
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry<V: Val<A>, A: Actor> {
    // The entry clock tells us which actors edited this entry.
    clock: VClock<A>,

    // The nested CRDT
    val: V
}

/// Operations which can be applied to the Map CRDT
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op<K: Key, V: Val<A>, A: Actor> {
    /// No change to the CRDT
    Nop,
    /// Remove a key from the map
    Rm {
        /// The clock under which we will perform this remove
        clock: VClock<A>,
        /// Key to remove
        key: K
    },
    /// Update an entry in the map
    Up {
        /// Actors version at the time of the update
        dot: Dot<A>,
        /// Key of the value to update
        key: K,
        /// The operation to apply on the value under `key`
        op: V::Op
    }
}

impl<K: Key, V: Val<A>, A: Actor> Default for Map<K, V, A> {
    fn default() -> Self {
        Map::new()
    }
}

impl<K: Key, V: Val<A>, A: Actor> Causal<A> for Map<K, V, A> {
    fn truncate(&mut self, clock: &VClock<A>) {
        let mut to_remove: Vec<K> = Vec::new();
        for (key, entry) in self.entries.iter_mut() {
            entry.clock.subtract(&clock);
            if entry.clock.is_empty() {
                to_remove.push(key.clone());
            } else {
                entry.val.truncate(&clock);
            }
        }

        for key in to_remove {
            self.entries.remove(&key);
        }

        let mut deferred = HashMap::new();
        for (mut rm_clock, key) in self.deferred.clone().into_iter() {
            rm_clock.subtract(&clock);
            if !rm_clock.is_empty() {
                deferred.insert(rm_clock, key);
            }
        }
        self.deferred = deferred;

        self.clock.subtract(&clock);
    }
}

impl<K: Key, V: Val<A>, A: Actor> CmRDT for Map<K, V, A> {
    type Op = Op<K, V, A>;

    fn apply(&mut self, op: &Self::Op) {
        match op.clone() {
            Op::Nop => {/* do nothing */},
            Op::Rm { clock, key } => {
                self.apply_rm(key, &clock);
            },
            Op::Up { dot, key, op } => {
                if self.clock.get(&dot.actor) >= dot.counter {
                    // we've seen this op already
                    return;
                }

                let mut entry = self.entries.remove(&key)
                    .unwrap_or_else(|| Entry {
                        clock: VClock::new(),
                        val: V::default()
                    });

                entry.clock.apply(&dot);
                entry.val.apply(&op);
                self.entries.insert(key.clone(), entry);

                self.clock.apply(&dot);
                self.apply_deferred();
            }
        }
    }
}

impl<K: Key, V: Val<A>, A: Actor> CvRDT for Map<K, V, A> {
    fn merge(&mut self, other: &Self) {
        let mut other_remaining = other.entries.clone();
        let mut keep = BTreeMap::new();
        for (key, mut entry) in self.entries.clone().into_iter() {
            match other.entries.get(&key) {
                None => {
                    // other doesn't contain this entry because it:
                    //  1. has witnessed it and dropped it
                    //  2. hasn't witnessed it
                    entry.clock.subtract(&other.clock);
                    if entry.clock.is_empty() {
                        // other has seen this entry and dropped it
                    } else {
                        // the other map has not seen this version of this entry, so add it
                        let mut actors_who_have_deleted_this_entry = other.clock.clone();
                        actors_who_have_deleted_this_entry.subtract(&entry.clock);
                        entry.val.truncate(&actors_who_have_deleted_this_entry);
                        keep.insert(key, entry);
                    }
                }
                Some(other_entry) => {
                    // SUBTLE: this entry is present in both orswots, BUT that doesn't mean we
                    // shouldn't drop it!
                    let mut e_clock = entry.clock.clone();
                    let mut oe_clock = other_entry.clock.clone();
                    let mut common = e_clock.intersection(&oe_clock);
                    e_clock.subtract(&common);
                    oe_clock.subtract(&common);
                    e_clock.subtract(&other.clock);
                    oe_clock.subtract(&self.clock);

                    // Perfectly possible that an item in both sets should be dropped
                    common.merge(&e_clock);
                    common.merge(&oe_clock);
                    if !common.is_empty() {
                        // we should not drop, as there are common clocks
                        entry.val.merge(&other_entry.val);
                        let mut actors_who_have_deleted_this_entry = entry.clock.clone();
                        actors_who_have_deleted_this_entry.merge(&other_entry.clock);
                        actors_who_have_deleted_this_entry.subtract(&common);

                        entry.val.truncate(&actors_who_have_deleted_this_entry);
                        entry.clock = common;
                        keep.insert(key.clone(), entry);
                    }
                    // don't want to consider this again below
                    other_remaining.remove(&key).unwrap();
                }
            }
        }

        for (key, mut entry) in other_remaining.into_iter() {
            entry.clock.subtract(&self.clock);
            if !entry.clock.is_empty() {
                // other has witnessed a novel addition, so add it
                let mut actors_who_deleted_this_entry = self.clock.clone();
                actors_who_deleted_this_entry.subtract(&entry.clock);
                entry.val.truncate(&actors_who_deleted_this_entry);
                keep.insert(key, entry);
            }
        }

        // merge deferred removals
        for (clock, deferred) in other.deferred.iter() {
            for key in deferred {
                self.apply_rm(key.clone(), &clock);
            }
        }

        self.entries = keep;

        // merge vclocks
        self.clock.merge(&other.clock);

        self.apply_deferred();
    }
}

impl<K: Key, V: Val<A>, A: Actor> Map<K, V, A> {
    /// Constructs an empty Map
    pub fn new() -> Self {
        Map {
            clock: VClock::new(),
            entries: BTreeMap::new(),
            deferred: HashMap::new()
         }
    }

    /// Returns true if the map has no entries, false otherwise
    pub fn is_empty(&self) -> ReadCtx<bool, A> {
        ReadCtx {
            add_clock: self.clock.clone(),
            rm_clock: self.clock.clone(),
            val: self.entries.is_empty()
        }
    }

    /// Returns the number of entries in the Map
    pub fn len(&self) -> ReadCtx<usize, A> {
        ReadCtx {
            add_clock: self.clock.clone(),
            rm_clock: self.clock.clone(),
            val: self.entries.len()
        }
    }

    /// Retrieve value stored under a key
    pub fn get(&self, key: &K) -> ReadCtx<Option<V>, A> {
        let add_clock = self.clock.clone();
        let entry_opt = self.entries.get(&key);
        ReadCtx {
            add_clock,
            rm_clock: entry_opt
                .map(|map_entry| map_entry.clock.clone())
                .unwrap_or_default(),
            val: entry_opt
                .map(|map_entry| map_entry.val.clone())
        }
    }

    /// Update a value under some key, if the key is not present in the map,
    /// the updater will be given the result of V::default().
    pub fn update<F, I>(&self, key: I, ctx: AddCtx<A>, f: F) -> Op<K, V, A>
        where F: FnOnce(&V, AddCtx<A>) -> V::Op,
              I: Into<K>
    {
        let key = key.into();
        let dot = ctx.dot.clone();
        let op = match self.entries.get(&key).map(|e| &e.val) {
            Some(data) => f(&data, ctx),
            None => f(&V::default(), ctx)
        };

        Op::Up { dot, key, op }
    }

    /// Remove an entry from the Map
    pub fn rm(&self, key: impl Into<K>, ctx: RmCtx<A>) -> Op<K, V, A> {
        Op::Rm { clock: ctx.clock, key: key.into() }
    }

    /// apply the pending deferred removes 
    fn apply_deferred(&mut self) {
        let deferred = self.deferred.clone();
        self.deferred = HashMap::new();
        for (clock, keys) in deferred {
            for key in keys {
                self.apply_rm(key, &clock);
            }
        }
    }

    /// Apply a key removal given a clock.
    fn apply_rm(&mut self, key: K, clock: &VClock<A>) {
        match clock.partial_cmp(&self.clock) {
            None | Some(Ordering::Greater) => {
                let deferred_set = self.deferred.entry(clock.clone())
                    .or_default();
                deferred_set.insert(key.clone());
            },
            _ => { /* we've seen this remove already */ }
        }

        if let Some(mut existing_entry) = self.entries.remove(&key) {
            existing_entry.clock.subtract(&clock);
            if !existing_entry.clock.is_empty() {
                existing_entry.val.truncate(&clock);
                self.entries.insert(key, existing_entry);
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::mvreg::{self, MVReg};
    use crate::orswot::Orswot;

    type TestActor = u8;
    type TestKey = u8;
    type TestVal = MVReg<u8, TestActor>;
    type TestMap =  Map<TestKey, Map<TestKey, TestVal, TestActor>, TestActor>;

    #[test]
    fn test_get() {
        let mut m: TestMap = Map::new();

        assert_eq!(m.get(&0).val, None);

        m.clock.apply(&m.clock.inc(1));

        m.entries.insert(0, Entry {
            clock: m.clock.clone(),
            val: Map::default()
        });

        assert_eq!(m.get(&0).val, Some(Map::new()));
    }
    
    #[test]
    fn test_op_exchange_converges_quickcheck1() {
        let op_actor1 = Op::Up {
            dot: Dot::new(0, 3),
            key: 9,
            op: Op::Up {
                dot: Dot::new(0, 3),
                key: 0,
                op: mvreg::Op::Put {
                    clock: Dot::new(0, 3).into(),
                    val: 0
                }
            }
        };
        let op_1_actor2 = Op::Up {
            dot: Dot::new(1, 1),
            key: 9,
            op: Op::Rm {
                clock: Dot::new(1, 1).into(),
                key: 0
            }
        };
        let op_2_actor2 = Op::Rm {
            clock: Dot::new(1, 2).into(),
            key: 9
        };
        
        let mut m1: TestMap = Map::new();
        let mut m2: TestMap = Map::new();

        m1.apply(&op_actor1);
        assert_eq!(m1.clock, Dot::new(0, 3).into());
        assert_eq!(m1.entries.get(&9).unwrap().clock, Dot::new(0, 3).into());
        assert_eq!(m1.entries.get(&9).unwrap().val.deferred.len(), 0);

        m2.apply(&op_1_actor2);
        m2.apply(&op_2_actor2);
        assert_eq!(m2.clock, Dot::new(1, 1).into());
        assert_eq!(m2.entries.get(&9), None);
        assert_eq!(
            m2.deferred.get(&Dot::new(1, 2).into()),
            Some(&vec![9].into_iter().collect())
        );
        
        // m1 <- m2
        m1.apply(&op_1_actor2);
        m1.apply(&op_2_actor2);
        
        // m2 <- m1
        m2.apply(&op_actor1);
        
        // m1 <- m2 == m2 <- m1
        assert_eq!(m1, m2);
    }

    #[test]
    fn merge_error() {
        let mut m1: Map<u8, Orswot<u8, u8>, u8> = Map {
            clock: VClock::from(Dot::new(75, 1)),
            entries: BTreeMap::new(),
            deferred: HashMap::new()
        };

        let mut m2: Map<u8, Orswot<u8, u8>, u8> = Map {
            clock: vec![Dot::new(75, 1), Dot::new(93, 1)]
                .into_iter()
                .collect(),
            entries: vec![
                (101, Entry {
                    clock: vec![Dot::new(75, 1), Dot::new(93, 1)]
                        .into_iter()
                        .collect(),
                    val: Orswot {
                        clock: vec![Dot::new(75, 1), Dot::new(93, 1)]
                            .into_iter()
                            .collect(),
                        entries: vec![
                            (1, VClock::from(Dot::new(75, 1))),
                            (2, VClock::from(Dot::new(93, 1)))
                        ].into_iter().collect(),
                        deferred: HashMap::new()
                    }
                })
            ].into_iter().collect(),
            deferred: HashMap::new()
        };

        m1.merge(&m2);

        assert_eq!(
            m1,
            Map {
                clock: vec![Dot::new(75, 1), Dot::new(93, 1)]
                    .into_iter()
                    .collect(),
                entries: vec![
                    (101, Entry {
                        clock: Dot::new(93, 1).into(),
                        val: Orswot {
                            clock: vec![Dot::new(93, 1)]
                                .into_iter()
                                .collect(),
                            entries: vec![
                                (2, VClock::from(Dot::new(93, 1)))
                            ].into_iter().collect(),
                            deferred: HashMap::new()
                        }
                    })
                ].into_iter().collect(),
                deferred: HashMap::new()
            }
        );
        
        m2.merge(&m1);

        assert_eq!(m1, m2);
    }
}
