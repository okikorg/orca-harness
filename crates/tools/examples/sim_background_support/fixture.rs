// ---------------------------------------------------------------- fixture

pub(super) const AREAS: usize = 24;
pub(super) const FILES_PER_AREA: usize = 16;
pub(super) const TOPICS: [&str; 8] = [
    "authentication",
    "authorization",
    "secrets",
    "privacy",
    "telemetry",
    "rate limit",
    "sandbox",
    "retry",
];
const FILLER: [&str; 40] = [
    "the", "service", "returns", "a", "bounded", "response", "when", "the", "caller", "provides",
    "an", "explicit", "deadline", "and", "every", "worker", "reports", "usage", "after", "each",
    "step", "so", "that", "the", "host", "can", "budget", "context", "without", "guessing",
    "which", "path", "produced", "the", "result", "under", "load", "on", "shared", "storage",
];

struct Rng(u64);

impl Rng {
    fn seeded(parts: &[&str]) -> Self {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        parts.hash(&mut hasher);
        Self(hasher.finish() | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo + 1)
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Truth {
    files: usize,
    pub(super) lines: usize,
}

pub(super) struct Fixture {
    pub(super) truths: HashMap<(usize, usize), Truth>,
}

fn area_name(area: usize) -> String {
    format!("area-{area:02}")
}

/// A documentation tree with topic words scattered deterministically, and
/// the ground truth computed the way the `grep` tool counts: a line
/// matches when it contains the topic substring.
pub(super) fn build_fixture(root: &Path) -> Fixture {
    let mut truths = HashMap::new();
    for area in 0..AREAS {
        let dir = root.join("docs").join(area_name(area));
        std::fs::create_dir_all(&dir).expect("fixture dir");
        let mut counts = vec![Truth { files: 0, lines: 0 }; TOPICS.len()];
        for file in 0..FILES_PER_AREA {
            let mut rng = Rng::seeded(&["file", &area.to_string(), &file.to_string()]);
            let line_count = rng.range(20, 60) as usize;
            let mut lines = (0..line_count)
                .map(|_| {
                    let words = rng.range(6, 12) as usize;
                    (0..words)
                        .map(|_| FILLER[rng.next() as usize % FILLER.len()])
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect::<Vec<_>>();
            for topic in TOPICS {
                if rng.chance(35) {
                    for _ in 0..rng.range(1, 3) {
                        let index = rng.next() as usize % lines.len();
                        lines[index] = format!("{} {topic} {}", lines[index], "is documented here");
                    }
                }
            }
            let body = format!(
                "# {} note {file}\n\n{}\n",
                area_name(area),
                lines.join("\n")
            );
            std::fs::write(dir.join(format!("note-{file:02}.md")), &body).expect("fixture file");
            for (index, topic) in TOPICS.iter().enumerate() {
                let matching = body.lines().filter(|line| line.contains(topic)).count();
                if matching > 0 {
                    counts[index].files += 1;
                    counts[index].lines += matching;
                }
            }
        }
        for (index, truth) in counts.into_iter().enumerate() {
            truths.insert((area, index), truth);
        }
    }
    Fixture { truths }
}

fn task_for(index: usize) -> (usize, usize, String) {
    let area = index % AREAS;
    let topic = (index / AREAS + index) % TOPICS.len();
    let task = format!(
        "Explore docs/{}/ only. Count the files that mention \"{}\" and the total number of \
         matching lines. Report exactly: <files> files, <lines> matching lines.",
        area_name(area),
        TOPICS[topic]
    );
    (area, topic, task)
}

fn verify(answer: &str, truth: Truth) -> Result<(), String> {
    let files = answer
        .split(" files")
        .next()
        .and_then(|head| head.rsplit(' ').next())
        .and_then(|n| n.parse::<usize>().ok());
    let lines = answer
        .split(" matching lines")
        .next()
        .and_then(|head| head.rsplit(' ').next())
        .and_then(|n| n.parse::<usize>().ok());
    match (files, lines) {
        (Some(files), Some(lines)) if files == truth.files && lines == truth.lines => Ok(()),
        _ => Err(format!(
            "expected {} files, {} lines; answer: {answer:?}",
            truth.files, truth.lines
        )),
    }
}
