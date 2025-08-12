fn main() {
    let who = std::env::var("WHO").unwrap_or_else(|_| "world".to_string());
    println!("Hello, {}! I am running this WASM workload from a k8s cluster!", who);
}
