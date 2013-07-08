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
use unstable::atomics::AtomicOption;
use unstable::sync::{UnsafeAtomicRcBox, LittleLock};
use util;

// Possible states:
// 1: running			(killed = false; unkillable = false; blocked_ptr = 0)
// 2: kill pending		(killed = TRUE;  unkillable = false; blocked_ptr = 0)
// 3: unkillable		(killed = false; unkillable = TRUE;  blocked_ptr = 0)
// 4: unkillable + kill pend	(killed = TRUE;  unkillable = TRUE;  blocked_ptr = 0)
// 5: blocked			(killed = false; unkillable = false; blocked_ptr = TASK)
// 6: blocked unkill		(killed = false; unkillable = TRUE;  blocked_ptr = TASK)
// 7: blocked unkill + pend	(killed = TRUE;  unkillable = TRUE;  blocked_ptr = TASK)

// Possible operations:
// Block:		1->5, 2->fail, 3->5, 4->5
// Unblock:		5->1, 6->3, 7->4
// Become unkillable:	1->3, 2->fail, 3->3, 4->4
// Become killable:	1->1, 2->fail, 3->1, 4->fail
// Kill:		1->2, 2->2, 3->4, 4->4, 5->2, 6->7, 7->7


/*

   killer:
   match handle.unkillable.swap(KILLED) { // maybe this can be nonatomic???
   	KILLABLE => {
   		match handle.blocked_task.swap(KILLED) {
			RUNNING => { } // they'll get it later
			KILLED => { } // already killed; someone else
			task_ptr => {
				sched.enqueue_task(task_ptr);
			}
		}
	}
	UNKILLABLE => { } // they'll deal with it later; nothing to do
	KILLED => { } // someone else already killed; nothing to do
   }

   unkillable:

   if task.disallow_kill++ == 0 {
   	match task.handle.unkillable.swap(UNKILLABLE) {
   		KILLED => fail!(), // not super important
   		UNKILLABLE => rtassert!(false)
   		KILLABLE => { }
   	}
   }

   rekillable:

   if --task.disallow_kill == 0 {
   	match task.handle.unkillable.swap(KILLABLE) {
   		KILLED => fail!(), // more important than above
   		KILLABLE => rtassert!(false)
   		UNKILLABLE => { }
   	}
   }

*/

// State values for the 'killed' and 'unkillable' atomic flags below.
static KILL_RUNNING:    uint = 0;
static KILL_KILLED:     uint = 1;
static KILL_UNKILLABLE: uint = 2;

/// State shared between tasks used for task killing during linked failure.
// FIXME(#7544)(bblum): think about the cache efficiency of this
struct KillHandleInner {
    // Is the task running, blocked, or killed? Possible values:
    // * KILL_RUNNING    - Not unkillable, no kill pending.
    // * KILL_KILLED     - Kill pending.
    // * <ptr>           - A transmuted blocked ~Task pointer.
    // This flag is refcounted because it may also be referenced by a blocking
    // concurrency primitive, used to wake the task normally, whose reference
    // may outlive the handle's if the task is killed.
    killed: UnsafeAtomicRcBox<AtomicUint>,
    // Has the task deferred kill signals? This flag guards the above one.
    // Possible values:
    // * KILL_RUNNING    - Not unkillable, no kill pending.
    // * KILL_KILLED     - Kill pending.
    // * KILL_UNKILLABLE - Kill signals deferred.
    unkillable: AtomicUint,

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
            killed:     UnsafeAtomicRcBox::new(AtomicUint::new(KILL_RUNNING)),
            unkillable: AtomicUint::new(KILL_RUNNING),
            // Exit code propagation fields
            any_child_failed: false,
            child_tombstones: None,
            graveyard_lock:   LittleLock(),
        }))
    }

    // Will begin unwinding if a kill signal was received.
    pub fn inhibit_kill(&mut self) {
        // This function can't be used recursively, because a task which sees
        // a KILLED signal must fail immediately, which an already-unkillable
        // task cannot do.
        let inner = unsafe { &mut *self.get() };
        // Expect flag to contain RUNNING. If KILLED, it should stay KILLED.
        // TODO: is it necessary to prohibit double kill?
        match inner.unkillable.compare_and_swap(KILL_RUNNING, KILL_UNKILLABLE, SeqCst) {
            KILL_RUNNING    => { }, // normal case
            KILL_KILLED     => fail!("task killed"),
            _               => rtabort!("inhibit_kill: task already unkillable"),
        }
    }

    pub fn allow_kill(&mut self) {
        let inner = unsafe { &mut *self.get() };
        // Expect flag to contain UNKILLABLE. If KILLED, it should stay KILLED.
        // TODO: is it necessary to prohibit double kill?
        match inner.unkillable.compare_and_swap(KILL_UNKILLABLE, KILL_RUNNING, SeqCst) {
            KILL_UNKILLABLE => { }, // normal case
            KILL_KILLED     => fail!("task killed"),
            _               => rtabort!("allow_kill: task already killable"),
        }
    }

    pub fn kill(&mut self) {
        let inner = unsafe { &mut *self.get() };
        if inner.unkillable.swap(KILL_KILLED, SeqCst) == KILL_RUNNING {
            // Got in. Allowed to try to punt the task awake.
            let flag = unsafe { &mut *inner.killed.get() };
            match flag.swap(KILL_KILLED, SeqCst) {
                KILL_RUNNING | KILL_KILLED => { },
                task_ptr => {
                    let blocked_task: ~Task = unsafe { cast::transmute(task_ptr) };
                    let sched = Local::take::<Scheduler>();
                    sched.schedule_task(blocked_task);
                }
            }
        } else {
            // Otherwise it was either unkillable or already killed. Somebody
            // else was here first who will deal with the kill signal.
        }
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
