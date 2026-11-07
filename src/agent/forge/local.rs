//! Bare repositories on this disk, `<remote>/<owner>/<name>.git`, so the
//! whole path runs offline. It clones and judges what a worker commits (the
//! `change` deliverable), and has no pull requests: `owes: pull_request` is
//! refused for it at load.

use super::mirror::{Mirrors, Remote};
use crate::agent::config::Identity;
use crate::agent::ports::{
    BoxFuture, Dependencies, Execute, Forge, Proposal, Refusal, TicketDirs, Workspace,
};
use anyhow::{Context, Result, bail};
use solos_agent_core::change::{Harvest, RepoName};
use solos_agent_core::delivery::Observed;
use std::path::PathBuf;

pub struct LocalForge {
    pub(super) mirrors: Mirrors,
    pub(super) identity: Identity,
    pub(super) remote: PathBuf,
}

impl Forge for LocalForge {
    fn prepare<'a>(
        &'a self,
        repo: &'a RepoName,
        dirs: TicketDirs<'a>,
    ) -> BoxFuture<'a, Result<Workspace>> {
        Box::pin(async move {
            let bare = self
                .remote
                .join(repo.owner())
                .join(format!("{}.git", repo.name()));
            let url = bare
                .to_str()
                .with_context(|| format!("{} is not UTF-8", bare.display()))?;
            let remote = Remote { url, auth: None };
            self.mirrors
                .prepare(repo, dirs, &remote, &self.identity)
                .await
        })
    }

    fn admit<'a>(
        &'a self,
        proposal: Proposal<'a>,
    ) -> BoxFuture<'a, Result<std::result::Result<Harvest, Refusal>>> {
        Box::pin(async move {
            self.mirrors
                .admit(proposal.repo, &proposal, &self.identity)
                .await
        })
    }

    fn dependencies<'a>(&'a self, workspace: &'a Workspace) -> BoxFuture<'a, Dependencies> {
        Box::pin(self.mirrors.dependencies(workspace))
    }

    fn execute<'a>(&'a self, job: Execute<'a>) -> BoxFuture<'a, Result<Observed>> {
        Box::pin(async move {
            match job {
                Execute::Publish(publish) => bail!(
                    "a local forge has no pull requests, so it cannot publish {}",
                    publish.ticket
                ),
            }
        })
    }
}
