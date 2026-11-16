//! Lists audio endpoints, or renames one (development tool).
//!
//! ```text
//! cargo run -p mixercraft-winaudio --example endpoints
//! cargo run -p mixercraft-winaudio --example endpoints -- rename <endpoint id> "<name>"
//! ```

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("rename") {
        let (Some(id), Some(name)) = (args.get(1), args.get(2)) else {
            eprintln!("usage: endpoints rename <id> <name>");
            std::process::exit(2);
        };
        match mixercraft_winaudio::set_description(id, name) {
            Ok(()) => println!("renamed"),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        return;
    }
    match mixercraft_winaudio::endpoints() {
        Ok(list) => {
            for e in list {
                println!("{:?}\t{} ({})\t{}", e.flow, e.description, e.device_name, e.id);
            }
        }
        Err(e) => eprintln!("{e}"),
    }
}
