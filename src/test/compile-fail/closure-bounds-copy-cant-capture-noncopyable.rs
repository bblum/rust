// Copyright 2013 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::comm;

fn foo(blk: ~fn:Copy()) {
    blk();
}

fn main() {
    let (p,c) = comm::stream();
    do foo { // shouldn't be legal
        c.send(()); //~ ERROR cannot capture variable of type `std::comm::Chan<()>`, which does not fulfill `Copy`, in a bounded closure
        //~^ NOTE this closure's environment must satisfy `Copy`
    }
    p.recv();
}
