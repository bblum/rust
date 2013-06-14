fn foo(x: ~fn:Copy()) -> (~fn(), ~fn()) {
    (copy x, x)
}
fn main() {
    let v = ~[~[1,2,3],~[4,5,6]]; // shouldn't get double-freed
    let (f1,f2) = do foo {
        assert!(v.len() == 2);
    };
    f1();
    f2();
}
