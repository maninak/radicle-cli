#![allow(clippy::or_fun_call)]
use std::convert::TryFrom;
use std::ffi::OsString;

use anyhow::anyhow;

use librad::git::identities::local::LocalIdentity;
use librad::git::storage::ReadOnlyStorage;
use librad::git::Storage;
use librad::git_ext::{Oid, RefLike};
use librad::profile::Profile;

use radicle_common as common;
use radicle_common::args::{Args, Error, Help};
use radicle_common::cobs::patch::MergeTarget;
use radicle_common::{cobs, git, keys, patch, person, profile, project};
use radicle_terminal as term;

pub const HELP: Help = Help {
    name: "patch",
    description: env!("CARGO_PKG_DESCRIPTION"),
    version: env!("CARGO_PKG_VERSION"),
    usage: r#"
Usage

    rad patch [<option>...]

Create options

    --[no-]sync       Sync patch to seed (default: sync)

Options

    --list            List all patches (default: false)
    --help            Print help
"#,
};

pub const PATCH_MSG: &str = r#"
<!--
Please enter a patch message for your changes. An empty
message aborts the patch proposal.

The first line is the patch title. The patch description
follows, and must be separated with a blank line, just
like a commit message. Markdown is supported in the title
and description.
-->
"#;

#[derive(Default, Debug)]
pub struct Options {
    pub list: bool,
    pub verbose: bool,
    pub sync: bool,
}

impl Args for Options {
    fn from_args(args: Vec<OsString>) -> anyhow::Result<(Self, Vec<OsString>)> {
        use lexopt::prelude::*;

        let mut parser = lexopt::Parser::from_args(args);
        let mut list = false;
        let mut verbose = false;
        let mut sync = true;

        while let Some(arg) = parser.next()? {
            match arg {
                Long("list") | Short('l') => {
                    list = true;
                }
                Long("verbose") | Short('v') => {
                    verbose = true;
                }
                Long("sync") => {
                    sync = true;
                }
                Long("no-sync") => {
                    sync = false;
                }
                Long("help") => {
                    return Err(Error::Help.into());
                }
                _ => return Err(anyhow::anyhow!(arg.unexpected())),
            }
        }

        Ok((
            Options {
                list,
                sync,
                verbose,
            },
            vec![],
        ))
    }
}

pub fn run(options: Options) -> anyhow::Result<()> {
    let (urn, repo) = project::cwd()
        .map_err(|_| anyhow!("this command must be run in the context of a project"))?;

    let profile = profile::default()?;
    let signer = term::signer(&profile)?;
    let storage = keys::storage(&profile, signer)?;
    let project = project::get(&storage, &urn)?
        .ok_or_else(|| anyhow!("couldn't load project {} from local state", urn))?;

    if options.list {
        list(&storage, &repo, &profile, &project)?;
    } else {
        create(&storage, &profile, &project, &repo, &options)?;
    }

    Ok(())
}

fn list(
    storage: &Storage,
    repo: &git::Repository,
    profile: &Profile,
    project: &project::Metadata,
) -> anyhow::Result<()> {
    let whoami = person::local(storage)?;
    let patches = cobs::patch::Patches::new(whoami.clone(), profile.paths(), storage)?;
    let proposed = patches.proposed(&project.urn)?;

    // Our `HEAD`.
    let head = repo.head()?;
    // Patches the user authored.
    let mut own = Vec::new();
    // Patches other users authored.
    let mut other = Vec::new();

    for (id, patch) in proposed {
        if *patch.author.urn() == whoami.urn() {
            own.push((id, patch));
        } else {
            other.push((id, patch));
        }
    }

    term::print(&term::format::badge_positive("YOU PROPOSED"));
    term::blank();

    if own.is_empty() {
        term::print(&term::format::italic("Nothing to show."));
    } else {
        for (id, patch) in &mut own {
            patch.author.resolve(storage).ok();
            print(&whoami, id, patch, project, &head, repo, storage)?;
        }
    }
    term::blank();
    term::print(&term::format::badge_secondary("OTHERS PROPOSED"));
    term::blank();

    if other.is_empty() {
        term::print(&term::format::italic("Nothing to show."));
    } else {
        for (id, patch) in &mut other {
            patch.author.resolve(storage).ok();
            print(&whoami, id, patch, project, &head, repo, storage)?;
        }
    }

    term::blank();

    Ok(())
}

fn create(
    storage: &Storage,
    profile: &Profile,
    project: &project::Metadata,
    repo: &git::Repository,
    options: &Options,
) -> anyhow::Result<()> {
    term::headline(&format!(
        "🌱 Creating patch for {}",
        term::format::highlight(&project.name)
    ));

    // `HEAD`; This is what we are proposing as a patch.
    let head = repo.head()?;
    let head_oid = head.target().ok_or(anyhow!("invalid HEAD ref; aborting"))?;
    let head_commit = repo.find_commit(head_oid)?;
    let head_branch = head
        .shorthand()
        .ok_or(anyhow!("cannot create patch from detatched head; aborting"))?;
    let head_branch = RefLike::try_from(head_branch)?;

    // Make sure the `HEAD` commit can be found in the monorepo. Otherwise there
    // is no way for anyone to merge this patch.
    let spinner = term::spinner(format!(
        "Looking for HEAD ({}) in storage...",
        term::format::secondary(common::fmt::oid(&head_oid))
    ));
    if storage.find_object(Oid::from(head_oid))?.is_none() {
        spinner.failed();
        term::blank();

        return Err(Error::WithHint {
            err: anyhow!("Current branch head not found in storage"),
            hint: "hint: run `rad push` and try again",
        }
        .into());
    }
    spinner.finish();
    term::blank();

    // Determine the merge target for this patch. This can ben any tracked remote's "default"
    // branch, as well as your own (eg. `rad/master`).
    let targets = patch::find_merge_targets(&head_oid, storage, project)?;

    // Show which peers have merged the patch.
    for peer in &targets.merged {
        term::info!(
            "{} {}",
            peer.name(),
            term::format::badge_secondary("merged")
        );
    }
    // eg. `refs/namespaces/<proj>/refs/remotes/<peer>/heads/master`
    let (target_peer, target_oid) = match targets.not_merged.as_slice() {
        [] => anyhow::bail!("no merge targets found for patch"),
        [target] => target,
        _ => {
            // TODO: Let user select which branch to use as a target.
            todo!();
        }
    };

    // TODO: List matching working copy refs for all targets.

    let user_name = storage.config_readonly()?.user_name()?;
    term::info!(
        "{}/{} ({}) <- {}/{} ({})",
        target_peer.name(),
        term::format::highlight(&project.default_branch.to_string()),
        term::format::secondary(&common::fmt::oid(target_oid)),
        user_name,
        term::format::highlight(&head_branch.to_string()),
        term::format::secondary(&common::fmt::oid(&head_oid)),
    );

    let (ahead, behind) = repo.graph_ahead_behind(head_oid, (*target_oid).into())?;
    term::info!(
        "{} commit(s) ahead, {} commit(s) behind",
        term::format::positive(ahead),
        if behind > 0 {
            term::format::negative(behind)
        } else {
            term::format::dim(behind)
        }
    );

    // List commits in patch that aren't in the target branch.
    let merge_base_ref = repo.merge_base((*target_oid).into(), head_oid);

    term::blank();
    term::patch::list_commits(repo, &merge_base_ref.unwrap(), &head_oid)?;
    term::blank();

    if !term::confirm("Continue?") {
        anyhow::bail!("patch proposal aborted by user");
    }

    let message = head_commit
        .message()
        .ok_or(anyhow!("commit summary is not valid UTF-8; aborting"))?;
    let (title, description) = edit_message(message)?;
    let title_pretty = &term::format::dim(format!("╭─ {} ───────", title));

    term::blank();
    term::print(title_pretty);
    term::blank();

    if description.is_empty() {
        term::print(term::format::italic("No description provided."));
    } else {
        term::markdown(&description);
    }

    term::blank();
    term::print(&term::format::dim(format!(
        "╰{}",
        "─".repeat(term::text_width(title_pretty) - 1)
    )));
    term::blank();

    if !term::confirm("Create patch?") {
        anyhow::bail!("patch proposal aborted by user");
    }

    let whoami = person::local(storage)?;
    let patches = cobs::patch::Patches::new(whoami, profile.paths(), storage)?;
    let id = patches.create(
        &project.urn,
        &title,
        &description,
        MergeTarget::default(),
        head_oid,
        &[],
    )?;

    term::blank();
    term::success!("Patch {} created 🌱", term::format::highlight(id));

    if options.sync {
        rad_sync::run(rad_sync::Options {
            refs: rad_sync::Refs::Branch(head_branch.to_string()),
            verbose: options.verbose,
            ..rad_sync::Options::default()
        })?;
    }

    Ok(())
}

fn edit_message(message: &str) -> anyhow::Result<(String, String)> {
    let message = match term::Editor::new()
        .require_save(true)
        .trim_newlines(true)
        .extension(".markdown")
        .edit(&format!("{}{}", message, PATCH_MSG))
        .unwrap()
    {
        Some(s) => s,
        None => anyhow::bail!("user aborted the patch"),
    };
    let (title, description) = message
        .split_once("\n\n")
        .ok_or(anyhow!("invalid title or description"))?;
    let (title, description) = (title.trim(), description.trim());
    let description = description.replace(PATCH_MSG.trim(), ""); // Delete help message.

    Ok((title.to_owned(), description))
}

/// Adds patch details as a new row to `table` and render later.
pub fn print(
    whoami: &LocalIdentity,
    patch_id: &cobs::patch::PatchId,
    patch: &cobs::patch::Patch,
    project: &project::Metadata,
    head: &git::Reference,
    repo: &git::Repository,
    storage: &Storage,
) -> anyhow::Result<()> {
    let revision = patch.revisions.last();
    let revision_oid = revision.tag;
    let revision_pretty = term::format::dim(format!("R{}", revision.version));
    let you = patch.author.urn() == &whoami.urn();
    let prefix = "└── ";
    let mut author_info = vec![format!(
        "{}{} opened by {} {}",
        prefix,
        term::format::secondary(common::fmt::cob(patch_id)),
        term::format::tertiary(patch.author.name()),
        term::format::dim(patch.timestamp)
    )];

    if you {
        author_info.push(term::format::badge_secondary("you"));
    }

    let diff = if let Some(head_oid) = head.target() {
        let (a, b) = repo.graph_ahead_behind(revision_oid.into(), head_oid)?;
        let ahead = term::format::positive(a);
        let behind = term::format::negative(b);

        format!("ahead {}, behind {}", ahead, behind)
    } else {
        String::default()
    };

    term::info!(
        "{} {} {} {}",
        term::format::bold(&patch.title),
        revision_pretty,
        term::format::secondary(common::fmt::oid(&revision_oid)),
        diff
    );
    term::info!("{}", author_info.join(" "));

    for merge in &revision.merges {
        let peer = project::PeerInfo::get(&merge.peer, project, storage);
        let mut badges = Vec::new();

        if peer.delegate {
            badges.push(term::format::badge_secondary("delegate"));
        }
        if peer.id == *storage.peer_id() {
            badges.push(term::format::badge_primary("you"));
        }

        term::info!(
            "{}{} by {} {} {}",
            " ".repeat(term::text_width(prefix)),
            term::format::secondary(term::format::dim("✓ merged")),
            term::format::tertiary(peer.name()),
            badges.join(" "),
            term::format::dim(merge.timestamp)
        );
    }

    Ok(())
}
