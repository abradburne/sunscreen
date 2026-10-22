fn main() {
    if let Err(e) = sunscreen::run(std::env::args().collect()) {
        eprintln!("{e}");
        std::process::exit(2);
    }
}
