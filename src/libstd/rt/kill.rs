// Copyright 2013 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Task death: asynchronous killing, linked failure, exit code propagation.

use cell::Cell;
use option::{Option, Some, None};
use ptr;
use prelude::*;
use unstable::sync::{UnsafeAtomicRcBox, LittleLock};
use util;

// State shared between tasks used for task killing during linked failure.
// FIXME(#7544)(bblum): think about the cache efficiency of this
struct KillHandleInner {
    // Does the task need to die?
    killed: bool,
    // Has the task requested not to be killed temporarily?
    unkillable: bool,
    // A possibly-null pointer for punting the task awake if it's blocked.
    // If non-null, points to the state flag of the blocked-on packet's port.
    blocked_on_port: *mut util::Void,
    // Protects (sometimes) 'killed', 'unkillable', and 'blocked_on_port'.
    kill_lock: LittleLock,

    // Shared state between task and children for exit code propagation. These
    // are here so we can re-use the kill handle to implement watched children
    // tasks. Using a separate ARClike would introduce extra atomic adds/subs
    // into common spawn paths, so this is just for speed.

    // Locklessly accessed; protected by the enclosing refcount's barriers.
    any_child_failed: bool,
    // A lazy list, consuming which may unwrap() many child tombstones.
    child_tombstones: Option<~fn() -> bool>,
    // Protects multiple children simultaneously creating tombstones.
    graveyard_lock: LittleLock,
}

#[deriving(Clone)]
pub struct KillHandle(UnsafeAtomicRcBox<KillHandleInner>);

impl KillHandle {
    pub fn new() -> KillHandle {
        KillHandle(UnsafeAtomicRcBox::new(KillHandleInner {
            // Linked failure fields
            killed:           false,
            unkillable:       false,
            blocked_on_port:  ptr::mut_null(),
            kill_lock:        LittleLock(),
            // Exit code propagation fields
            any_child_failed: false,
            child_tombstones: None,
            graveyard_lock:   LittleLock(),
        }))
    }

    pub fn notify_immediate_failure(&mut self) {
        // A benign data race may happen here if there are failing sibling
        // tasks that were also spawned-watched. The refcount's write barriers
        // in UnsafeAtomicRcBox ensure that this write will be seen by the
        // unwrapper/destructor, whichever task may unwrap it.
        unsafe { (*self.get()).any_child_failed = true; }
    }

    // For use when a task does not need to collect its children's exit
    // statuses, but the task has a parent which might want them.
    pub fn reparent_children_to(self, parent: &mut KillHandle) {
        // Optimistic path: If another child of the parent's already failed,
        // we don't need to worry about any of this.
        if unsafe { (*parent.get()).any_child_failed } {
            return;
        }

        // Try to see if all our children are gone already.
        match unsafe { self.try_unwrap() } {
            // Couldn't unwrap; children still alive. Reparent entire handle as
            // our own tombstone, to be unwrapped later.
            Left(this) => {
                let this = Cell::new(this); // :(
                do add_lazy_tombstone(parent) |other_tombstones| {
                    let this = Cell::new(this.take()); // :(
                    let others = Cell::new(other_tombstones); // :(
                    || {
                        // Prefer to check tombstones that were there first,
                        // being "more fair" at the expense of tail-recursion.
                        others.take().map_consume_default(true, |f| f()) && {
                            let mut inner = unsafe { this.take().unwrap(false) };
                            (!inner.any_child_failed) &&
                                inner.child_tombstones.swap_map_default(true, |f| f())
                        }
                    }
                }
            }
            // Whether or not all children exited, one or more already failed.
            Right(KillHandleInner { any_child_failed: true, _ }) => {
                parent.notify_immediate_failure();
            }
            // All children exited, but some left behind tombstones that we
            // don't want to wait on now. Give them to our parent.
            Right(KillHandleInner { any_child_failed: false,
                                    child_tombstones: Some(f), _ }) => {
                let f = Cell::new(f); // :(
                do add_lazy_tombstone(parent) |other_tombstones| {
                    let f = Cell::new(f.take()); // :(
                    let others = Cell::new(other_tombstones); // :(
                    || {
                        // Prefer fairness to tail-recursion, as in above case.
                        others.take().map_consume_default(true, |f| f()) &&
                            f.take()()
                    }
                }
            }
            // All children exited, none failed. Nothing to do!
            Right(KillHandleInner { any_child_failed: false,
                                    child_tombstones: None, _ }) => { }
        }

        // NB: Takes a pthread mutex -- 'blk' not allowed to reschedule.
        fn add_lazy_tombstone(parent: &mut KillHandle,
                              blk: &fn(Option<~fn() -> bool>) -> ~fn() -> bool) {

            let inner: &mut KillHandleInner = unsafe { &mut *parent.get() };
            unsafe {
                do inner.graveyard_lock.lock {
                    // Update the current "head node" of the lazy list.
                    inner.child_tombstones =
                        Some(blk(util::replace(&mut inner.child_tombstones, None)));
                }
            }
        }
    }
}

#[cfg(test)]
mod test {
    #[allow(unused_mut)];
    use rt::test::*;
    use super::*;
    use util;

    #[test]
    fn no_tombstone_success() {
        do run_in_newsched_task {
            // Tests case 4 of the 4-way match in reparent_children.
            let mut parent = KillHandle::new();
            let mut child  = KillHandle::new();

            // Without another handle to child, the try unwrap should succeed.
            child.reparent_children_to(&mut parent);
            let mut parent_inner = unsafe { parent.unwrap(true) };
            assert!(parent_inner.child_tombstones.is_none());
            assert!(parent_inner.any_child_failed == false);
        }
    }
    #[test]
    fn no_tombstone_failure() {
        do run_in_newsched_task {
            // Tests case 2 of the 4-way match in reparent_children.
            let mut parent = KillHandle::new();
            let mut child  = KillHandle::new();

            child.notify_immediate_failure();
            // Without another handle to child, the try unwrap should succeed.
            child.reparent_children_to(&mut parent);
            let mut parent_inner = unsafe { parent.unwrap(true) };
            assert!(parent_inner.child_tombstones.is_none());
            // Immediate failure should have been propagated.
            assert!(parent_inner.any_child_failed);
        }
    }
    #[test]
    fn no_tombstone_because_sibling_already_failed() {
        do run_in_newsched_task {
            // Tests "case 0, the optimistic path in reparent_children.
            let mut parent = KillHandle::new();
            let mut child1 = KillHandle::new();
            let mut child2 = KillHandle::new();
            let mut link   = child2.clone();

            // Should set parent's child_failed flag
            child1.notify_immediate_failure();
            child1.reparent_children_to(&mut parent);
            // Should bypass trying to unwrap child2 entirely.
            // Otherwise, due to 'link', it would try to tombstone.
            child2.reparent_children_to(&mut parent);
            // Should successfully unwrap even though 'link' is still alive.
            let mut parent_inner = unsafe { parent.unwrap(true) };
            assert!(parent_inner.child_tombstones.is_none());
            // Immediate failure should have been propagated by first child.
            assert!(parent_inner.any_child_failed);
            util::ignore(link);
        }
    }
    #[test]
    fn one_tombstone_success() {
        do run_in_newsched_task {
            let mut parent = KillHandle::new();
            let mut child  = KillHandle::new();
            let mut link   = child.clone();

            // Creates 1 tombstone. Existence of 'link' makes try-unwrap fail.
            child.reparent_children_to(&mut parent);
            // Let parent collect tombstones.
            util::ignore(link);
            // Must have created a tombstone
            let mut parent_inner = unsafe { parent.unwrap(true) };
            assert!(parent_inner.child_tombstones.swap_unwrap()());
            assert!(parent_inner.any_child_failed == false);
        }
    }
    #[test]
    fn one_tombstone_failure() {
        do run_in_newsched_task {
            let mut parent = KillHandle::new();
            let mut child  = KillHandle::new();
            let mut link   = child.clone();

            // Creates 1 tombstone. Existence of 'link' makes try-unwrap fail.
            child.reparent_children_to(&mut parent);
            // Must happen after tombstone to not be immediately propagated.
            link.notify_immediate_failure();
            // Let parent collect tombstones.
            util::ignore(link);
            // Must have created a tombstone
            let mut parent_inner = unsafe { parent.unwrap(true) };
            // Failure must be seen in the tombstone.
            assert!(parent_inner.child_tombstones.swap_unwrap()() == false);
            assert!(parent_inner.any_child_failed == false);
        }
    }
    #[test]
    fn two_tombstones_success() {
        do run_in_newsched_task {
            let mut parent = KillHandle::new();
            let mut middle = KillHandle::new();
            let mut child  = KillHandle::new();
            let mut link   = child.clone();

            child.reparent_children_to(&mut middle); // case 1 tombstone
            // 'middle' should try-unwrap okay, but still have to reparent.
            middle.reparent_children_to(&mut parent); // case 3 tombston
            // Let parent collect tombstones.
            util::ignore(link);
            // Must have created a tombstone
            let mut parent_inner = unsafe { parent.unwrap(true) };
            assert!(parent_inner.child_tombstones.swap_unwrap()());
            assert!(parent_inner.any_child_failed == false);
        }
    }
    #[test]
    fn two_tombstones_failure() {
        do run_in_newsched_task {
            let mut parent = KillHandle::new();
            let mut middle = KillHandle::new();
            let mut child  = KillHandle::new();
            let mut link   = child.clone();

            child.reparent_children_to(&mut middle); // case 1 tombstone
            // Must happen after tombstone to not be immediately propagated.
            link.notify_immediate_failure();
            // 'middle' should try-unwrap okay, but still have to reparent.
            middle.reparent_children_to(&mut parent); // case 3 tombstone
            // Let parent collect tombstones.
            util::ignore(link);
            // Must have created a tombstone
            let mut parent_inner = unsafe { parent.unwrap(true) };
            // Failure must be seen in the tombstone.
            assert!(parent_inner.child_tombstones.swap_unwrap()() == false);
            assert!(parent_inner.any_child_failed == false);
        }
    }
}
