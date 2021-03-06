// Copyright 2012 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// -*- rust -*-
extern mod extra;

use std::vec;

fn grow(v: &mut ~[int]) { *v += ~[1]; }

pub fn main() {
    let mut v: ~[int] = ~[];
    grow(&mut v);
    grow(&mut v);
    grow(&mut v);
    let len = v.len();
    debug!(len);
    assert_eq!(len, 3 as uint);
}
