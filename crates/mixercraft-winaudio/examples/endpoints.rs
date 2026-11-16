//! Lists audio endpoints and defaults, renames an endpoint, or sets a
//! default (development tool).
//!
//! ```text
//! cargo run -p mixercraft-winaudio --example endpoints
//! cargo run -p mixercraft-winaudio --example endpoints -- defaults
//! cargo run -p mixercraft-winaudio --example endpoints -- rename <endpoint id> "<name>"
//! cargo run -p mixercraft-winaudio --example endpoints -- default <endpoint id> console|multimedia|communications
//! ```

use mixercraft_winaudio::Role;

fn fail(e: impl std::fmt::Display) -> ! {
    eprintln!("{e}");
    std::process::exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("rename") => {
            let (Some(id), Some(name)) = (args.get(1), args.get(2)) else {
                fail("usage: endpoints rename <id> <name>")
            };
            mixercraft_winaudio::set_description(id, name).unwrap_or_else(|e| fail(e));
            println!("renamed");
        }
        Some("default") => {
            let (Some(id), Some(role)) = (args.get(1), args.get(2)) else {
                fail("usage: endpoints default <id> console|multimedia|communications")
            };
            let role = match role.as_str() {
                "console" => Role::Console,
                "multimedia" => Role::Multimedia,
                "communications" => Role::Communications,
                other => fail(format!("unknown role {other}")),
            };
            mixercraft_winaudio::set_default(id, role).unwrap_or_else(|e| fail(e));
            println!("default set");
        }
        Some("defaults") => {
            let names = mixercraft_winaudio::endpoints().unwrap_or_else(|e| fail(e));
            let defaults = mixercraft_winaudio::snapshot().unwrap_or_else(|e| fail(e));
            for (flow, role, id) in defaults.entries {
                let name = names
                    .iter()
                    .find(|e| e.id == id)
                    .map_or_else(|| id.clone(), |e| format!("{} ({})", e.description, e.device_name));
                println!("{flow:?}\t{role:?}\t{name}");
            }
        }
        _ => {
            for e in mixercraft_winaudio::endpoints().unwrap_or_else(|e| fail(e)) {
                println!("{:?}\t{} ({})\t{}", e.flow, e.description, e.device_name, e.id);
            }
        }
    }
}
