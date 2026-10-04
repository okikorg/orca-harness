//! What a sandbox should be when it starts.
//!
//! The field names follow the vocabulary the OpenAI Agents API settled on
//! for declared environments, so that anyone who has read those docs finds
//! this familiar: `workspace_directory`, `capability_directories`,
//! `packages`, `setup_commands`, `env`, `network`. The provider tags are
//! ours.

/// Egress policy. Providers that cannot enforce one report
/// `network_policy: false` in their [`Capabilities`](orca_harness_core::Capabilities),
/// and a host that requires [`Network::Restricted`] refuses to start
/// against them rather than running wide open.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Network {
    #[default]
    Enabled,
    Disabled,
    Restricted {
        allowed_domains: Vec<String>,
    },
}

/// Packages to install before the agent starts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Packages {
    pub system: Vec<String>,
    pub python: Vec<String>,
    pub npm: Vec<String>,
}

impl Packages {
    pub fn system<I: IntoIterator<Item = S>, S: Into<String>>(names: I) -> Self {
        Self {
            system: names.into_iter().map(Into::into).collect(),
            ..Default::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.system.is_empty() && self.python.is_empty() && self.npm.is_empty()
    }
}

/// One command run once, at startup, before the agent gets the sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupCommand {
    pub command: String,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EnvironmentSpec {
    pub image: Option<String>,
    pub workspace_directory: String,
    /// Directories the host stages capability content into (skills,
    /// plugins). Read-only to the agent: this protects the integrity of
    /// what the host installed. It does not stop the agent writing its own
    /// files elsewhere in the workspace, and does not pretend to.
    pub capability_directories: Vec<String>,
    pub packages: Packages,
    pub setup_commands: Vec<SetupCommand>,
    pub env: Vec<(String, String)>,
    pub network: Network,
    pub timeout_ms: Option<u64>,
}

impl Default for EnvironmentSpec {
    fn default() -> Self {
        Self {
            image: None,
            workspace_directory: "/workspace".into(),
            capability_directories: Vec::new(),
            packages: Packages::default(),
            setup_commands: Vec::new(),
            env: Vec::new(),
            network: Network::default(),
            timeout_ms: None,
        }
    }
}

impl EnvironmentSpec {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn image(mut self, image: impl Into<String>) -> Self {
        self.image = Some(image.into());
        self
    }

    pub fn workspace_directory(mut self, dir: impl Into<String>) -> Self {
        self.workspace_directory = dir.into();
        self
    }

    pub fn capability_directories<I, S>(mut self, dirs: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.capability_directories = dirs.into_iter().map(Into::into).collect();
        self
    }

    pub fn packages(mut self, packages: Packages) -> Self {
        self.packages = packages;
        self
    }

    pub fn setup_command(mut self, command: impl Into<String>, cwd: Option<String>) -> Self {
        self.setup_commands.push(SetupCommand {
            command: command.into(),
            cwd,
        });
        self
    }

    pub fn env<I, K, V>(mut self, vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.env = vars
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        self
    }

    pub fn network(mut self, network: Network) -> Self {
        self.network = network;
        self
    }
}
