use rand::Rng;
use rand_distr::Zipf;

/// Wraps a Zipfian distribution for generating item IDs.
pub struct ZipfianGenerator {
    dist: Zipf<f64>,
    alpha: f64,
}

impl ZipfianGenerator {
    pub fn new(num_items: u64, alpha: f64) -> Self {
        let dist = Zipf::new(num_items, alpha).expect("invalid Zipfian parameters");
        Self { dist, alpha }
    }

    pub fn alpha(&self) -> f64 {
        self.alpha
    }

    /// Generate the next item ID (1-based).
    pub fn next_id(&mut self) -> u64 {
        let mut rng = rand::thread_rng();
        rng.sample(&self.dist) as u64
    }
}
