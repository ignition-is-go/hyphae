use rx3::{Cell, DepNode, JoinExt, MapExt, Mutable, Watchable};

fn main() {
    let a = Cell::new(1);
    let b = Cell::new("b").with_name("b");

    let aa = a.clone();

    let a_double = a.map(|x| x * 2).with_name("a_double");

    let aa_double = aa.map(|x| x * 2).with_name("aa_double");

    let combined = a_double
        .join(&b)
        // .map(|(a_val, b_val)| format!("{}{}", a_val, b_val))
        .with_name("combined");

    combined.watch(|v| println!(">>>: {:?}", v));

    aa.set(5);
    a.set(10);

    combined.print_dependency_tree();

    aa_double.print_dependency_tree();
}
