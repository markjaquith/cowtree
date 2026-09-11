//! Centralized implementation tuning backed by repeatable benchmarks.

pub const DIRECTORY_CLONE_MIN_FILES: usize = 32;

const DIRECTORY_CLONE_MAX_WORKERS: usize = 8;

pub fn directory_clone_workers(job_count: usize) -> usize {
    let available = std::thread::available_parallelism().map_or(1, usize::from);
    directory_clone_workers_for(available, job_count)
}

fn directory_clone_workers_for(available: usize, job_count: usize) -> usize {
    available.min(DIRECTORY_CLONE_MAX_WORKERS).min(job_count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_workers_respect_cores_cap_and_work() {
        assert_eq!(directory_clone_workers_for(16, 20), 8);
        assert_eq!(directory_clone_workers_for(4, 20), 4);
        assert_eq!(directory_clone_workers_for(16, 3), 3);
        assert_eq!(directory_clone_workers_for(16, 0), 0);
    }
}
