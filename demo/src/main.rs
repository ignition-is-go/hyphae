use rx3::{Cell, DepNode, JoinExt, MapExt, Mutable, Watchable, flat};

fn main() {
    println!("=== Panic Isolation Demo ===\n");
    println!("One subscriber panics, but others still receive updates.\n");

    let cell = Cell::new(0);

    // Subscriber 1: prints normally
    let _g1 = cell.subscribe(|v| {
        println!("  Subscriber 1: got {}", v);
    });

    // Subscriber 2: panics on value 2
    let _g2 = cell.subscribe(|v| {
        if *v == 2 {
            panic!("Subscriber 2 panics on value 2!");
        }
        println!("  Subscriber 2: got {}", v);
    });

    // Subscriber 3: prints normally
    let _g3 = cell.subscribe(|v| {
        println!("  Subscriber 3: got {}", v);
    });

    println!("Setting value to 1:");
    cell.set(1);

    println!("\nSetting value to 2 (subscriber 2 will panic):");
    cell.set(2);

    println!("\nSetting value to 3 (all subscribers still work):");
    cell.set(3);

    println!("\nDone! All subscribers survived the panic.");

    let a = Cell::new(1).with_name("a");

    let b = Cell::new('A').with_name("b");

    let c = Cell::new("alpha").with_name("c");

    let d = Cell::new(1.0).with_name("d");

    let aa = a.join(&b);

    let cc = c.join(&d);

    let mega = aa.join(&cc);

    let mapped = mega.map(|((a, b), (c, d))| println!("{:?}:{:?}:{:?}:{:?}", a, b, c, d));

    let flat_demo = a.join(&b).join(&c).join(&d).map(flat!(|a, b, c, d| {
        println!("Flattened values: {:?}, {:?}, {:?}, {:?}", a, b, c, d);
    }));

    flat_demo.print_dependency_tree();

}
