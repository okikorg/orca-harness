pub fn should_retry(attempts_made: u32, max_attempts: u32) -> bool {
    attempts_made <= max_attempts
}
