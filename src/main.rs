mod registry;
mod residency;

use registry::{compute_verdict, ModelKind};
use residency::{available_ram_bytes, check_admission, resolve_headroom_bytes, total_ram_bytes};

// ponytail: no CLI/daemon wiring yet, this module owns hardware-safety
// logic only. Other modules (config.rs, engine.rs, api/) plug in later.
fn main() {
    let total = total_ram_bytes();
    let available = available_ram_bytes();
    let headroom = resolve_headroom_bytes(total, None).expect("headroom floor check");
    println!(
        "total RAM: {:.2} GB, available: {:.2} GB, headroom reserved: {:.2} GB",
        total as f64 / 1e9,
        available as f64 / 1e9,
        headroom as f64 / 1e9
    );

    let budget = total.saturating_sub(headroom);
    let sample_size_bytes = 4_000_000_000u64; // ~4GB GGUF, e.g. OLMoE Q4_K_M
    let verdict = compute_verdict(ModelKind::Moe, sample_size_bytes, budget);
    println!("sample MoE model verdict: {:?}", verdict);

    match check_admission(0, sample_size_bytes, total, headroom) {
        Ok(()) => println!("admission: OK, model would be allowed to load"),
        Err(rejected) => println!(
            "admission: refused, short by {:.2} GB",
            rejected.shortfall_bytes as f64 / 1e9
        ),
    }
}
