// Tiny `--key value` parser shared by the binaries (included with `include!`).
#[allow(dead_code)]
fn arg<T: std::str::FromStr>(name: &str, default: T) -> T {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == &format!("--{name}"))
        .and_then(|i| args.get(i + 1))
        .map(|v| v.parse().ok().unwrap_or_else(|| panic!("bad value for --{name}")))
        .unwrap_or(default)
}
