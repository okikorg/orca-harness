mod stats_segment_tests {
    use super::*;

    #[test]
    fn segments_render_only_nonzero_counts() {
        let stats = orca_harness_tools::BackgroundStats::new();
        assert!(stats_segments(&stats, 0).is_empty());
        stats.inc_processes();
        stats.inc_processes();
        stats.inc_agents();
        assert_eq!(stats_segments(&stats, 0), ["procs 2", "agents 1 ↓"]);
        stats.inc_kernels();
        assert_eq!(
            stats_segments(&stats, 0),
            ["procs 2", "pykernel", "agents 1 ↓"]
        );
        stats.inc_bun_repls();
        assert_eq!(
            stats_segments(&stats, 0),
            ["procs 2", "pykernel", "bun", "agents 1 ↓"]
        );
        assert_eq!(stats_segments(&stats, 3).last().unwrap(), "agents 3 ↓");
    }
}
