use luaufusion::denobridge::snapshot::create_v8_snapshot;

fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    if args.len() < 2 || args[1] == "--help" {
        println!("Generates a V8 snapshot for luaufusion+deno_core isolates.");
        println!("Usage: {} [filename/--help]", args[0]);
        return;
    }

    let filename = args[1].clone();

    println!("Creating V8 snapshot for luaufusion+deno_core isolates...");

    let snapshot = create_v8_snapshot();

    std::fs::write(&filename, snapshot).expect("Failed to write snapshot to file");
}