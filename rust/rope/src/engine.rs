// Copyright 2016 Google Inc. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! An engine for handling edits (possibly from async sources) and undo. This
//! module actually implements a mini Conflict-free Replicated Data Type, but
//! is considerably simpler than the usual CRDT implementation techniques,
//! because all operations are serialized in this central engine.

use std::borrow::Cow;
use std::collections::BTreeSet;

use rope::{Rope, RopeInfo};
use subset::Subset;
use delta::Delta;

pub struct Engine {
    rev_id_counter: usize,
    union_str: Rope,
    revs: Vec<Revision>,
}

struct Revision {
    rev_id: usize,
    from_union: Subset,
    union_str_len: usize,
    edit: Contents,
}

use self::Contents::*;

enum Contents {
    Edit {
        priority: usize,
        undo_group: usize,
        inserts: Subset,
        deletes: Subset,
    },
    Undo {
        groups: BTreeSet<usize>,  // set of undo_group id's
    }
}

impl Engine {
    fn get_current_undo(&self) -> Option<&BTreeSet<usize>> {
        for rev in self.revs.iter().rev() {
            if let Undo { ref groups } = rev.edit {
                return Some(&groups);
            }
        }
        None
    }

    fn find_rev(&self, rev_id: usize) -> Option<usize> {
        for (i, rev) in self.revs.iter().enumerate().rev() {
            if rev.rev_id == rev_id {
                return Some(i)
            }
        }
        None
    }

    fn get_rev(&self, rev_index: usize) -> Rope {
        let mut from_union = Cow::Borrowed(&self.revs[rev_index].from_union);
        for rev in &self.revs[rev_index + 1..] {
            if let Edit { ref inserts, .. } = rev.edit {
                if !inserts.is_trivial() {
                    from_union = Cow::Owned(from_union.transform_intersect(inserts));
                }
            }
        }
        from_union.apply(&self.union_str)
    }

    pub fn get_head(&self) -> Rope {
        self.get_rev(self.revs.len() - 1)
    }

    /// A delta that, when applied to previous head, results in the current head. Panics
    /// if there is not at least one edit.
    pub fn delta_head(&self) -> Delta<RopeInfo> {
        let mut prev_from_union = Cow::Borrowed(&self.revs[self.revs.len() - 2].from_union);
        let rev = &self.revs.last().unwrap();
        if let Edit { ref inserts, .. } = rev.edit {
            if !inserts.is_trivial() {
                prev_from_union = Cow::Owned(prev_from_union.transform_intersect(inserts));
            }
        }
        Delta::synthesize(&self.union_str, &prev_from_union, &rev.from_union)
    }

    fn mk_new_rev(&self, new_priority: usize, undo_group: usize,
            base_rev: usize, delta: Delta<RopeInfo>) -> (Revision, Rope) {
        let ix = self.find_rev(base_rev).expect("base revision not found");
        let rev = &self.revs[ix];
        let (ins_delta, deletes) = delta.factor();
        let mut union_ins_delta = ins_delta.transform_expand(&rev.from_union, rev.union_str_len, true);
        let mut new_deletes = deletes.transform_expand(&rev.from_union);
        for r in &self.revs[ix + 1..] {
            if let Edit { priority, ref inserts, .. } = rev.edit {
                if !inserts.is_trivial() {
                    let after = new_priority >= priority;  // should never be ==
                    union_ins_delta = union_ins_delta.transform_expand(&inserts, r.union_str_len, after);
                    new_deletes = new_deletes.transform_expand(&inserts);
                }
            }
        }
        let new_inserts = union_ins_delta.invert_insert();
        let new_union_str = union_ins_delta.apply(&self.union_str);
        let undone = self.get_current_undo().map_or(false, |undos| undos.contains(&undo_group));
        let mut new_from_union = Cow::Borrowed(&rev.from_union);
        {
            let edit = if undone { &new_inserts } else { &new_deletes };
            if !edit.is_trivial() {
                new_from_union = Cow::Owned(new_from_union.intersect(edit));
            }
        }
        (Revision {
            rev_id: self.rev_id_counter,
            from_union: new_from_union.into_owned(),
            union_str_len: new_union_str.len(),
            edit: Edit {
                priority: new_priority,
                undo_group: undo_group,
                inserts: new_inserts,
                deletes: new_deletes,
            }
        }, new_union_str)
    }

    pub fn edit_rev(&mut self, priority: usize, undo_group: usize,
            base_rev: usize, delta: Delta<RopeInfo>) {
        let (new_rev, new_union_str) = self.mk_new_rev(priority, undo_group, base_rev, delta);
        self.rev_id_counter += 1;
        self.revs.push(new_rev);
        self.union_str = new_union_str;
    }

    // This computes undo all the way from the beginning. An optimization would be to not
    // recompute the prefix up to where the history diverges, but it's not clear that's
    // even worth the code complexity.
    fn compute_undo(&self, groups: BTreeSet<usize>) -> Revision {
        let mut from_union = Cow::Borrowed(&self.revs[0].from_union);
        for rev in &self.revs[1..] {
            if let Edit { ref undo_group, ref inserts, ref deletes, .. } = rev.edit {
                if groups.contains(undo_group) {
                    if !inserts.is_trivial() {
                        from_union = Cow::Owned(from_union.transform_intersect(inserts));
                    }
                } else {
                    if !inserts.is_trivial() {
                        from_union = Cow::Owned(from_union.transform_expand(inserts));
                    }
                    if !deletes.is_trivial() {
                        from_union = Cow::Owned(from_union.intersect(deletes));
                    }
                }
            }
        }
        Revision {
            rev_id: self.rev_id_counter,
            from_union: from_union.into_owned(),
            union_str_len: self.union_str.len(),
            edit: Undo {
                groups: groups
            }
        }
    }

    pub fn undo(&mut self, groups: BTreeSet<usize>) {
        let new_rev = self.compute_undo(groups);
        self.revs.push(new_rev);
        self.rev_id_counter += 1;
    }
}
