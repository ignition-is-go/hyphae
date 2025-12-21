use rx3::{
    cell::Cell,
    combine,
    traits::{Mutable, Watchable},
};

fn main() {
    println!("=== Dependency Tracking Example ===\n");

    // Create some base cells
    let x = Cell::new(5).with_name("x");
    let y = Cell::new(10).with_name("y");
    let z = Cell::new(2).with_name("z");

    println!("Created base cells: x=5, y=10, z=2\n");

    // Create a derived cell using map
    let x_doubled = x.map(|val| val * 2).with_name("x_doubled");

    println!("--- x_doubled dependencies ---");
    x_doubled.print_dependencies();
    println!();

    // Create a combined cell
    let sum = combine!((&x, &y), |a: &i32, b: &i32| { a + b }).with_name("sum");

    println!("--- sum dependencies ---");
    sum.print_dependencies();
    println!();

    // Create a more complex dependency chain
    let complex = combine!((&sum, &z), |s: &i32, z_val: &i32| { s * z_val }).with_name("complex");

    println!("--- complex dependencies ---");
    complex.print_dependencies();
    println!();

    // Create a cell that depends on multiple levels
    let final_result =
        combine!((&x_doubled, &complex), |xd: &i32, c: &i32| { xd + c }).with_name("final_result");

    println!("--- final_result dependencies ---");
    final_result.print_dependencies();
    println!();

    // Show the full dependency graph
    println!("=== Full Dependency Graph ===");
    println!("{}", final_result.dependency_graph());

    // Demonstrate that updates still work
    println!("\n=== Testing Reactivity ===");
    println!("Initial values:");
    println!("  x_doubled = {}", x_doubled.get());
    println!("  sum = {}", sum.get());
    println!("  complex = {}", complex.get());
    println!("  final_result = {}", final_result.get());

    println!("\nUpdating x from 5 to 20...\n");
    x.set(20);

    println!("After update:");
    println!("  x_doubled = {}", x_doubled.get());
    println!("  sum = {}", sum.get());
    println!("  complex = {}", complex.get());
    println!("  final_result = {}", final_result.get());
}
