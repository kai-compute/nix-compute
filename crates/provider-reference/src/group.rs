use crate::model::NodeContext;
use anyhow::{ensure, Context};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Member {
    task_id: String,
    context: NodeContext,
    phase: String,
    sequence: u64,
    error: Option<String>,
}

struct Shared {
    root: PathBuf,
    member: Mutex<Member>,
    publication_directories: Mutex<Vec<fs::File>>,
    cancelled: Arc<AtomicBool>,
    stopped: AtomicBool,
}

impl Shared {
    fn published(&self) -> bool {
        fs::read_link(self.root.join("reports/current"))
            .is_ok_and(|path| path == Path::new("final"))
    }

    fn committed(&self) -> bool {
        if !self.published() {
            return false;
        }
        let directories = self.publication_directories.lock().unwrap();
        !directories.is_empty()
            && directories
                .iter()
                .all(|directory| directory.sync_all().is_ok())
    }

    fn prepare_publication(&self) -> anyhow::Result<()> {
        let directories = [
            Some(self.root.join("reports")),
            Some(self.root.clone()),
            self.root.parent().map(Path::to_path_buf),
        ]
        .into_iter()
        .flatten()
        .map(|path| {
            let directory = fs::File::open(&path)
                .with_context(|| format!("opening publication directory {}", path.display()))?;
            directory.sync_all()?;
            Ok(directory)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
        *self.publication_directories.lock().unwrap() = directories;
        Ok(())
    }

    fn decision_lock(&self) -> anyhow::Result<fs::File> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.root.join("decision.lock"))?;
        file.lock_exclusive()?;
        Ok(file)
    }

    fn write(&self, member: &Member) -> anyhow::Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        file.write_all(&serde_json::to_vec(member)?)?;
        file.as_file().sync_all()?;
        file.persist(self.root.join(format!("{}.json", member.context.node_rank)))?;
        Ok(())
    }

    fn phase(&self, phase: &str) -> anyhow::Result<()> {
        let mut member = self.member.lock().unwrap();
        ensure!(
            member.error.is_none(),
            "{}",
            member.error.as_deref().unwrap_or("group failed")
        );
        member.phase = phase.into();
        self.write(&member)
    }

    fn fail(&self, error: String) {
        let _decision = self.decision_lock();
        if self.published() {
            return;
        }
        self.cancelled.store(true, Ordering::Relaxed);
        let mut member = self.member.lock().unwrap();
        if member.error.is_none() {
            member.phase = "failed".into();
            member.error = Some(error);
            let _ = self.write(&member);
        }
    }

    fn peers(&self) -> anyhow::Result<Vec<Option<Member>>> {
        let own = self.member.lock().unwrap().clone();
        let mut result = Vec::new();
        let mut identities = std::collections::BTreeSet::new();
        for rank in 0..own.context.node_count {
            let bytes = match fs::read(self.root.join(format!("{rank}.json"))) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    result.push(None);
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            let peer: Member = serde_json::from_slice(&bytes)?;
            ensure!(
                peer.task_id == own.task_id
                    && peer.context.run_id == own.context.run_id
                    && peer.context.node_count == own.context.node_count
                    && peer.context.node_rank == rank
                    && peer.context.master_addr == own.context.master_addr
                    && peer.context.master_port == own.context.master_port,
                "node {rank} has a different task or rendezvous context"
            );
            ensure!(
                identities.insert(peer.context.node_id.clone()),
                "duplicate node identity"
            );
            ensure!(
                matches!(
                    peer.phase.as_str(),
                    "preparing" | "ready" | "prepared" | "publishing" | "completed" | "failed"
                ),
                "invalid peer phase"
            );
            ensure!(
                peer.phase != "failed",
                "node {rank} failed: {}",
                peer.error.as_deref().unwrap_or("unknown failure")
            );
            result.push(Some(peer));
        }
        Ok(result)
    }
}

pub struct ReportPaths {
    pub provisional: PathBuf,
    pub candidate: PathBuf,
}

pub struct Group {
    shared: Option<Arc<Shared>>,
    worker: Option<JoinHandle<()>>,
    timeout: Duration,
    finished: bool,
    cancelled: Arc<AtomicBool>,
}

impl Group {
    pub fn join(
        directory: Option<&Path>,
        context: &NodeContext,
        task_id: &str,
        timeout: u64,
        cancelled: Arc<AtomicBool>,
    ) -> anyhow::Result<Self> {
        ensure!(timeout > 0, "coordination timeout must be positive");
        let timeout = Duration::from_secs(timeout);
        let mut group = Self {
            shared: None,
            worker: None,
            timeout,
            finished: false,
            cancelled: cancelled.clone(),
        };
        if context.node_count == 1 {
            return Ok(group);
        }
        let root = directory
            .context("multi-node reference provider requires a shared --coordination-dir")?
            .join(&context.run_id);
        fs::create_dir_all(&root)?;
        let root = root.canonicalize()?;
        // Claims survive process death: reusing a run ID cannot adopt stale members.
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.join(format!("{}.claim", context.node_rank)))
            .context("node rank already claimed; use a new run_id for a new attempt")?;
        let shared = Arc::new(Shared {
            root,
            publication_directories: Mutex::new(Vec::new()),
            member: Mutex::new(Member {
                task_id: task_id.into(),
                context: context.clone(),
                phase: "preparing".into(),
                sequence: 0,
                error: None,
            }),
            cancelled,
            stopped: AtomicBool::new(false),
        });
        shared.phase("preparing")?;
        let worker = shared.clone();
        group.worker = Some(thread::spawn(move || {
            let count = worker.member.lock().unwrap().context.node_count;
            let mut seen = vec![(None, Instant::now()); count as usize];
            while !worker.stopped.load(Ordering::Relaxed) {
                if worker.committed() {
                    break;
                }
                let result = (|| -> anyhow::Result<()> {
                    ensure!(!worker.cancelled.load(Ordering::Relaxed), "node cancelled");
                    {
                        let mut own = worker.member.lock().unwrap();
                        own.sequence += 1;
                        worker.write(&own)?;
                    }
                    for (rank, peer) in worker.peers()?.iter().enumerate() {
                        if let Some(peer) = peer {
                            if peer.phase == "completed" {
                                continue;
                            }
                            if seen[rank].0 != Some(peer.sequence) {
                                seen[rank] = (Some(peer.sequence), Instant::now());
                            }
                        }
                        ensure!(
                            seen[rank].1.elapsed() < timeout,
                            "node {rank} missing or heartbeat expired"
                        );
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    worker.fail(format!("{error:#}"));
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }));
        group.shared = Some(shared);
        Ok(group)
    }

    fn wait(&self, phase: &str) -> anyhow::Result<()> {
        let start = Instant::now();
        loop {
            if self
                .shared
                .as_ref()
                .is_some_and(|shared| shared.committed())
            {
                return Ok(());
            }
            ensure!(
                !self.cancelled.load(Ordering::Relaxed),
                "group cancelled or peer failed"
            );
            let Some(shared) = &self.shared else {
                return Ok(());
            };
            if shared.peers()?.iter().all(|p| {
                p.as_ref().is_some_and(|m| {
                    matches!(m.phase.as_str(), "publishing" | "completed")
                        || (phase != "publishing" && m.phase == "prepared")
                        || (phase == "ready" && m.phase == "ready")
                })
            }) {
                return Ok(());
            }
            ensure!(
                phase != "ready" || start.elapsed() < self.timeout,
                "timed out waiting for all nodes to become ready"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn ready(&mut self) -> anyhow::Result<()> {
        if let Some(shared) = &self.shared {
            shared.phase("ready")?;
        }
        self.wait("ready")
    }

    pub fn fail(&self, error: String) {
        if let Some(shared) = &self.shared {
            shared.fail(error);
        }
    }

    pub fn reports(&self, local: &Path) -> anyhow::Result<ReportPaths> {
        let Some(shared) = &self.shared else {
            return Ok(ReportPaths {
                provisional: local.into(),
                candidate: local.into(),
            });
        };
        let root = shared.root.join("reports");
        fs::create_dir_all(root.join("pending"))?;
        fs::create_dir_all(root.join("final"))?;
        match std::os::unix::fs::symlink("pending", root.join("current")) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let rank = shared.member.lock().unwrap().context.node_rank;
        let filename = format!("{rank}.json");
        std::os::unix::fs::symlink(root.join("current").join(&filename), local)
            .context("creating local report reference")?;
        fs::File::open(local.parent().context("missing report parent")?)?.sync_all()?;
        Ok(ReportPaths {
            provisional: root.join("pending").join(&filename),
            candidate: root.join("final").join(&filename),
        })
    }

    pub fn prepare(&mut self, result: anyhow::Result<()>) -> anyhow::Result<()> {
        let result = result.and_then(|()| {
            if let Some(shared) = &self.shared {
                shared.phase("prepared")?;
            }
            self.wait("prepared")
        });
        if let Err(error) = &result {
            self.fail(format!("{error:#}"));
        }
        result
    }

    pub fn finish(&mut self, result: anyhow::Result<()>) -> anyhow::Result<()> {
        let result = result.and_then(|()| {
            let Some(shared) = &self.shared else {
                return Ok(());
            };
            if shared.committed() {
                return Ok(());
            }
            shared.prepare_publication()?;
            shared.phase("publishing")?;
            self.wait("publishing")?;
            let _decision = shared.decision_lock()?;
            if shared.committed() {
                return Ok(());
            }
            ensure!(
                !self.cancelled.load(Ordering::Relaxed),
                "group cancelled before report publication"
            );
            for peer in shared.peers()? {
                let peer = peer.context("missing member before report publication")?;
                ensure!(peer.phase == "publishing", "member is not ready to publish");
                ensure!(
                    shared
                        .root
                        .join(format!("reports/final/{}.json", peer.context.node_rank))
                        .is_file(),
                    "missing final report"
                );
            }
            let reports = shared.root.join("reports");
            let temporary = tempfile::tempdir_in(&reports)?;
            std::os::unix::fs::symlink("final", temporary.path().join("current"))?;
            // All local references switch together. The committed outcome is irreversible.
            fs::rename(temporary.path().join("current"), reports.join("current"))?;
            Ok(())
        });
        if let Err(error) = &result {
            self.fail(format!("{error:#}"));
        }
        self.finished = true;
        if let Some(shared) = &self.shared {
            if shared.published() {
                // A visible decision cannot become failure. Retry durability before acknowledging it.
                while !shared.committed() {
                    thread::sleep(Duration::from_millis(100));
                }
                let _ = shared.phase("completed");
                return Ok(());
            }
        }
        result
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        if let Some(shared) = &self.shared {
            if !self.finished {
                shared.fail("node stopped before completing the task".into());
            }
            shared.stopped.store(true, Ordering::Relaxed);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn context(rank: u32) -> NodeContext {
        NodeContext {
            run_id: "test".into(),
            node_id: format!("node-{rank}"),
            node_rank: rank,
            node_count: 2,
            master_addr: "127.0.0.1".into(),
            master_port: 29500,
        }
    }
    #[test]
    fn barrier_and_completion_wait_for_all_nodes() {
        let dir = tempfile::tempdir().unwrap();
        let mut left = Group::join(
            Some(dir.path()),
            &context(0),
            "task",
            3,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let mut right = Group::join(
            Some(dir.path()),
            &context(1),
            "task",
            3,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let left_reports = left.reports(&dir.path().join("left.json")).unwrap();
        let right_reports = right.reports(&dir.path().join("right.json")).unwrap();
        fs::write(&left_reports.candidate, "left").unwrap();
        fs::write(&right_reports.candidate, "right").unwrap();
        thread::scope(|scope| {
            let first = scope.spawn(move || {
                left.ready().unwrap();
                left.finish(Ok(())).unwrap();
            });
            right.ready().unwrap();
            right.finish(Ok(())).unwrap();
            first.join().unwrap();
        });
        assert_eq!(
            fs::read_to_string(dir.path().join("left.json")).unwrap(),
            "left"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("right.json")).unwrap(),
            "right"
        );
    }
    #[test]
    fn published_decision_waits_for_sync_recovery_even_after_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut group = Group::join(
            Some(dir.path()),
            &context(0),
            "task",
            3,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let report = dir.path().join("report.json");
        let paths = group.reports(&report).unwrap();
        fs::write(paths.candidate, "succeeded").unwrap();
        let shared = group.shared.as_ref().unwrap().clone();
        shared.prepare_publication().unwrap();
        let unavailable = fs::File::open("/dev/null").unwrap();
        assert!(unavailable.sync_all().is_err());
        *shared.publication_directories.lock().unwrap() = vec![unavailable];
        let reports = shared.root.join("reports");
        std::os::unix::fs::symlink("final", reports.join("next")).unwrap();
        fs::rename(reports.join("next"), reports.join("current")).unwrap();
        shared.cancelled.store(true, Ordering::Relaxed);
        let (sender, receiver) = std::sync::mpsc::channel();
        thread::scope(|scope| {
            scope.spawn(move || {
                sender
                    .send(group.finish(Err(anyhow::anyhow!("late storage error"))))
                    .unwrap();
            });
            let pending = receiver.recv_timeout(Duration::from_millis(200));
            shared.prepare_publication().unwrap();
            assert!(matches!(
                pending,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ));
            receiver
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
        });
        assert_eq!(fs::read_to_string(report).unwrap(), "succeeded");
    }

    #[test]
    fn peer_failure_and_duplicate_rank_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let left = Group::join(
            Some(dir.path()),
            &context(0),
            "task",
            3,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert!(Group::join(
            Some(dir.path()),
            &context(0),
            "task",
            3,
            Arc::new(AtomicBool::new(false))
        )
        .is_err());
        let mut right = Group::join(
            Some(dir.path()),
            &context(1),
            "task",
            3,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        drop(left);
        assert!(right.ready().is_err());
    }
    #[test]
    fn different_task_or_missing_peer_never_starts() {
        for other in [Some("different-task"), None] {
            let dir = tempfile::tempdir().unwrap();
            let mut left = Group::join(
                Some(dir.path()),
                &context(0),
                "task",
                1,
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
            let _right = other.map(|task| {
                Group::join(
                    Some(dir.path()),
                    &context(1),
                    task,
                    1,
                    Arc::new(AtomicBool::new(false)),
                )
                .unwrap()
            });
            assert!(left.ready().is_err());
        }
    }

    #[test]
    fn expired_heartbeat_cancels_a_running_group() {
        let dir = tempfile::tempdir().unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut left =
            Group::join(Some(dir.path()), &context(0), "task", 1, cancelled.clone()).unwrap();
        let peer = Member {
            task_id: "task".into(),
            context: context(1),
            phase: "ready".into(),
            sequence: 1,
            error: None,
        };
        fs::write(
            dir.path().join("test/1.json"),
            serde_json::to_vec(&peer).unwrap(),
        )
        .unwrap();
        left.ready().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !cancelled.load(Ordering::Relaxed) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
        }
        assert!(cancelled.load(Ordering::Relaxed));
        assert!(left.finish(Ok(())).is_err());
    }
}
