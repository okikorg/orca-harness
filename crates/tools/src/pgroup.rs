//! Process-group helpers: every spawned child leads its own group so a
//! kill reaches grandchildren (`sh -c` wrappers, servers they fork).
//! No `libc` dependency: `killpg` is declared directly, in the same
//! spirit as the hand-rolled `either` in `process.rs`.

#[cfg(unix)]
mod imp {
    extern "C" {
        fn killpg(pgrp: i32, sig: i32) -> i32;
    }
    const SIGKILL: i32 = 9;

    /// SIGKILL every process in `pgid`'s group. Best-effort: the group
    /// may already be gone.
    pub fn kill_group(pgid: u32) {
        unsafe {
            killpg(pgid as i32, SIGKILL);
        }
    }
}

#[cfg(not(unix))]
mod imp {
    /// Non-Unix fallback: no process groups; callers also keep
    /// `kill_on_drop`/`start_kill` which reach the direct child.
    pub fn kill_group(_pgid: u32) {}
}

pub(crate) use imp::kill_group;

#[cfg(all(test, unix))]
mod tests {
    use super::kill_group;

    #[tokio::test]
    async fn kill_group_reaches_grandchildren() {
        // sh spawns sleep as its own child; both live in sh's group.
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "sleep 283.1 & wait"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = child.id().unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let found = std::process::Command::new("pgrep")
            .args(["-f", "sleep 283.1"])
            .output()
            .unwrap();
        assert!(found.status.success(), "grandchild should be running");

        kill_group(pgid);
        let _ = child.wait().await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let found = std::process::Command::new("pgrep")
            .args(["-f", "sleep 283.1"])
            .output()
            .unwrap();
        assert!(
            !found.status.success(),
            "grandchild must be dead after group kill"
        );
    }
}
