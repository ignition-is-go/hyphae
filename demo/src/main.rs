use rx3::{Cell, Mutable, SubscribeExt};

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
}
