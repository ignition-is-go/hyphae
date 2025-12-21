use rx3::{
    cell::Cell,
    combine,
    traits::{Mutable, Watchable},
};

fn main() {
    println!("╔═══════════════════════════════════════════════════════╗");
    println!("║  Advanced Dependency Tracking & Visualization Demo   ║");
    println!("╚═══════════════════════════════════════════════════════╝\n");

    // Create base cells (leaf nodes in dependency graph)
    let temperature = Cell::new(20.0).with_name("temperature");
    let humidity = Cell::new(60.0).with_name("humidity");
    let pressure = Cell::new(1013.25).with_name("pressure");

    println!("📊 Created base weather sensors:");
    println!("  • temperature = 20.0°C");
    println!("  • humidity = 60.0%");
    println!("  • pressure = 1013.25 hPa\n");

    // First level derivatives
    let temp_fahrenheit = temperature
        .map(|c| c * 9.0 / 5.0 + 32.0)
        .with_name("temp_fahrenheit");

    let comfort_index = combine!((&temperature, &humidity), |t: &f64, h: &f64| {
        t - (0.55 - 0.0055 * h) * (t - 14.5)
    })
    .with_name("comfort_index");

    // Second level derivatives
    let weather_score = combine!((&comfort_index, &pressure), |comfort: &f64, p: &f64| {
        let pressure_factor = (p - 1000.0) / 20.0;
        (comfort / 25.0) + pressure_factor
    })
    .with_name("weather_score");

    // Third level - complex computation
    let display_value = combine!((&temp_fahrenheit, &weather_score), |tf: &f64, ws: &f64| {
        format!("{}°F (score: {:.2})", *tf as i32, ws)
    })
    .with_name("display_value");

    // ============================================================
    // Demonstrate various dependency inspection methods
    // ============================================================

    println!("═══════════════════════════════════════════════════════");
    println!("1️⃣  Simple Dependency Inspection");
    println!("═══════════════════════════════════════════════════════\n");

    println!("🔍 temp_fahrenheit dependencies:");
    temp_fahrenheit.print_dependencies();
    println!();

    println!("🔍 comfort_index dependencies:");
    comfort_index.print_dependencies();
    println!();

    println!("🔍 weather_score dependencies:");
    weather_score.print_dependencies();
    println!();

    println!("🔍 display_value dependencies:");
    display_value.print_dependencies();
    println!();

    println!("═══════════════════════════════════════════════════════");
    println!("2️⃣  Dependency Tree Visualization");
    println!("═══════════════════════════════════════════════════════\n");

    println!("🌳 Full dependency tree for display_value:\n");
    display_value.print_dependency_tree();
    println!();

    println!("═══════════════════════════════════════════════════════");
    println!("3️⃣  Dependency Statistics");
    println!("═══════════════════════════════════════════════════════\n");

    println!(
        "📌 {:<20} | ID: {:.8} | Dependencies: {} | Has deps: {}",
        "temperature",
        temperature.id().to_string(),
        temperature.dependency_count(),
        temperature.has_dependencies()
    );
    println!(
        "📌 {:<20} | ID: {:.8} | Dependencies: {} | Has deps: {}",
        "humidity",
        humidity.id().to_string(),
        humidity.dependency_count(),
        humidity.has_dependencies()
    );
    println!(
        "📌 {:<20} | ID: {:.8} | Dependencies: {} | Has deps: {}",
        "pressure",
        pressure.id().to_string(),
        pressure.dependency_count(),
        pressure.has_dependencies()
    );
    println!(
        "📌 {:<20} | ID: {:.8} | Dependencies: {} | Has deps: {}",
        "temp_fahrenheit",
        temp_fahrenheit.id().to_string(),
        temp_fahrenheit.dependency_count(),
        temp_fahrenheit.has_dependencies()
    );
    println!(
        "📌 {:<20} | ID: {:.8} | Dependencies: {} | Has deps: {}",
        "comfort_index",
        comfort_index.id().to_string(),
        comfort_index.dependency_count(),
        comfort_index.has_dependencies()
    );
    println!(
        "📌 {:<20} | ID: {:.8} | Dependencies: {} | Has deps: {}",
        "weather_score",
        weather_score.id().to_string(),
        weather_score.dependency_count(),
        weather_score.has_dependencies()
    );
    println!(
        "📌 {:<20} | ID: {:.8} | Dependencies: {} | Has deps: {}",
        "display_value",
        display_value.id().to_string(),
        display_value.dependency_count(),
        display_value.has_dependencies()
    );

    println!();

    println!("═══════════════════════════════════════════════════════");
    println!("4️⃣  All Dependencies (Direct)");
    println!("═══════════════════════════════════════════════════════\n");

    let all_deps = display_value.get_all_dependencies();
    println!(
        "📦 display_value has {} direct dependencies:",
        all_deps.len()
    );
    for (id, name) in all_deps {
        println!(
            "   └─ {} (ID: {:.8})",
            name.unwrap_or_else(|| "Unnamed".to_string()),
            id.to_string()
        );
    }
    println!();

    println!("═══════════════════════════════════════════════════════");
    println!("5️⃣  Dependency Graph as String");
    println!("═══════════════════════════════════════════════════════\n");

    let graph = display_value.dependency_graph();
    println!("📋 Graph representation:\n{}", graph);

    println!("═══════════════════════════════════════════════════════");
    println!("6️⃣  Live Reactivity Test");
    println!("═══════════════════════════════════════════════════════\n");

    println!("📊 Initial computed values:");
    println!("  • temp_fahrenheit = {:.1}°F", temp_fahrenheit.get());
    println!("  • comfort_index = {:.2}", comfort_index.get());
    println!("  • weather_score = {:.2}", weather_score.get());
    println!("  • display_value = {}", display_value.get());
    println!();

    println!("🔄 Updating temperature from 20.0°C to 25.0°C...\n");
    temperature.set(25.0);

    println!("📊 After temperature change:");
    println!("  • temp_fahrenheit = {:.1}°F", temp_fahrenheit.get());
    println!("  • comfort_index = {:.2}", comfort_index.get());
    println!("  • weather_score = {:.2}", weather_score.get());
    println!("  • display_value = {}", display_value.get());
    println!();

    println!("🔄 Updating humidity from 60.0% to 80.0%...\n");
    humidity.set(80.0);

    println!("📊 After humidity change:");
    println!("  • temp_fahrenheit = {:.1}°F", temp_fahrenheit.get());
    println!("  • comfort_index = {:.2}", comfort_index.get());
    println!("  • weather_score = {:.2}", weather_score.get());
    println!("  • display_value = {}", display_value.get());
    println!();

    println!("🔄 Updating pressure from 1013.25 hPa to 1020.0 hPa...\n");
    pressure.set(1020.0);

    println!("📊 After pressure change:");
    println!("  • temp_fahrenheit = {:.1}°F", temp_fahrenheit.get());
    println!("  • comfort_index = {:.2}", comfort_index.get());
    println!("  • weather_score = {:.2}", weather_score.get());
    println!("  • display_value = {}", display_value.get());
    println!();

    println!("═══════════════════════════════════════════════════════");
    println!("7️⃣  Complex Dependency Chain Example");
    println!("═══════════════════════════════════════════════════════\n");

    // Create a more complex chain
    let a = Cell::new(1).with_name("a");
    let b = Cell::new(2).with_name("b");
    let c = Cell::new(3).with_name("c");

    let ab = combine!((&a, &b), |x: &i32, y: &i32| { x + y }).with_name("ab");
    let bc = combine!((&b, &c), |x: &i32, y: &i32| { x * y }).with_name("bc");
    let abc = combine!((&ab, &bc, &c), |x: &i32, y: &i32, z: &i32| { x + y + z }).with_name("abc");

    println!("🔗 Created chain: a, b, c → ab, bc → abc\n");

    println!("Dependencies for 'abc':");
    abc.print_dependencies();
    println!();

    println!("Dependency tree for 'abc':");
    abc.print_dependency_tree();
    println!();

    println!("═══════════════════════════════════════════════════════");
    println!("✅ Dependency Tracking Demo Complete!");
    println!("═══════════════════════════════════════════════════════");
}
