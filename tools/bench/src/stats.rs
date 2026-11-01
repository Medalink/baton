use std::time::Duration;

pub fn header() {
    println!(
        "{:<34} {:>5} {:>10} {:>10} {:>10} {:>10}",
        "case", "n", "min ms", "p50 ms", "p95 ms", "max ms"
    );
}

pub fn row(name: &str, samples: &mut [Duration]) {
    samples.sort();
    let n = samples.len();
    let at = |q: f64| {
        let rank = ((q * n as f64).ceil() as usize).clamp(1, n);
        ms(samples[rank - 1])
    };
    println!(
        "{:<34} {:>5} {:>10.3} {:>10.3} {:>10.3} {:>10.3}",
        name,
        n,
        ms(samples[0]),
        at(0.5),
        at(0.95),
        ms(samples[n - 1])
    );
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}
