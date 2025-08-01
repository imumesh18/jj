// Copyright 2020 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use clap_complete::ArgValueCandidates;
use clap_complete::ArgValueCompleter;
use itertools::Itertools as _;
use jj_lib::object_id::ObjectId as _;
use tracing::instrument;

use crate::cli_util::CommandHelper;
use crate::cli_util::RevisionArg;
use crate::cli_util::print_conflicted_paths;
use crate::command_error::CommandError;
use crate::command_error::cli_error;
use crate::complete;
use crate::ui::Ui;

/// Resolve conflicted files with an external merge tool
///
/// Only conflicts that can be resolved with a 3-way merge are supported. See
/// docs for merge tool configuration instructions. External merge tools will be
/// invoked for each conflicted file one-by-one until all conflicts are
/// resolved. To stop resolving conflicts, exit the merge tool without making
/// any changes.
///
/// Note that conflicts can also be resolved without using this command. You may
/// edit the conflict markers in the conflicted file directly with a text
/// editor.
//  TODOs:
//   - `jj resolve --editor` to resolve a conflict in the default text editor. Should work for
//     conflicts with 3+ adds. Useful to resolve conflicts in a commit other than the current one.
//   - A way to help split commits with conflicts that are too complicated (more than two sides)
//     into commits with simpler conflicts. In case of a tree with many merges, we could for example
//     point to existing commits with simpler conflicts where resolving those conflicts would help
//     simplify the present one.
#[derive(clap::Args, Clone, Debug)]
pub(crate) struct ResolveArgs {
    #[arg(
        long, short,
        default_value = "@",
        value_name = "REVSET",
        add = ArgValueCompleter::new(complete::revset_expression_mutable),
    )]
    revision: RevisionArg,
    /// Instead of resolving conflicts, list all the conflicts
    // TODO: Also have a `--summary` option. `--list` currently acts like
    // `diff --summary`, but should be more verbose.
    #[arg(long, short)]
    list: bool,
    /// Specify 3-way merge tool to be used
    ///
    /// The built-in merge tools `:ours` and `:theirs` can be used to choose
    /// side #1 and side #2 of the conflict respectively.
    #[arg(
        long,
        conflicts_with = "list",
        value_name = "NAME",
        add = ArgValueCandidates::new(complete::merge_editors),
    )]
    tool: Option<String>,
    /// Only resolve conflicts in these paths. You can use the `--list` argument
    /// to find paths to use here.
    #[arg(
        value_name = "FILESETS",
        value_hint = clap::ValueHint::AnyPath,
        add = ArgValueCompleter::new(complete::revision_conflicted_files),
    )]
    paths: Vec<String>,
}

#[instrument(skip_all)]
pub(crate) fn cmd_resolve(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &ResolveArgs,
) -> Result<(), CommandError> {
    let mut workspace_command = command.workspace_helper(ui)?;
    let matcher = workspace_command
        .parse_file_patterns(ui, &args.paths)?
        .to_matcher();
    let commit = workspace_command.resolve_single_rev(ui, &args.revision)?;
    let tree = commit.tree()?;
    let conflicts = tree
        .conflicts()
        .filter(|path| matcher.matches(&path.0))
        .collect_vec();
    if conflicts.is_empty() {
        return Err(cli_error(if args.paths.is_empty() {
            "No conflicts found at this revision"
        } else {
            "No conflicts found at the given path(s)"
        }));
    }
    if args.list {
        return print_conflicted_paths(
            conflicts,
            ui.stdout_formatter().as_mut(),
            &workspace_command,
        );
    };

    let repo_paths = conflicts
        .iter()
        .map(|(path, _)| path.as_ref())
        .collect_vec();
    workspace_command.check_rewritable([commit.id()])?;
    let merge_editor = workspace_command.merge_editor(ui, args.tool.as_deref())?;
    let mut tx = workspace_command.start_transaction();
    let (new_tree_id, partial_resolution_error) =
        merge_editor.edit_files(ui, &tree, &repo_paths)?;
    let new_commit = tx
        .repo_mut()
        .rewrite_commit(&commit)
        .set_tree_id(new_tree_id)
        .write()?;
    tx.finish(
        ui,
        format!("Resolve conflicts in commit {}", commit.id().hex()),
    )?;

    // Print conflicts that are still present after resolution if the workspace
    // working copy is not at the commit. Otherwise, the conflicting paths will
    // be printed by the `tx.finish()` instead.
    if workspace_command.get_wc_commit_id() != Some(new_commit.id()) {
        if let Some(mut formatter) = ui.status_formatter() {
            if new_commit.has_conflict()? {
                let new_tree = new_commit.tree()?;
                let new_conflicts = new_tree.conflicts().collect_vec();
                writeln!(
                    formatter.labeled("warning").with_heading("Warning: "),
                    "After this operation, some files at this revision still have conflicts:"
                )?;
                print_conflicted_paths(new_conflicts, formatter.as_mut(), &workspace_command)?;
            }
        }
    }

    if let Some(err) = partial_resolution_error {
        return Err(err.into());
    }
    Ok(())
}
